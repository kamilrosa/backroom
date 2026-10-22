//! End-to-end test: a real server + several clients (no audio) in one
//! process, over QUIC on localhost.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use backroom_client::audio::Incoming;
use backroom_client::config::Config;
use backroom_client::net::{self, Command, NetHandle, Target};
use backroom_client::state::{ConnState, Shared};
use backroom_proto as proto;
use parking_lot::Mutex;

struct Client {
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    net: NetHandle,
    voice: tokio::sync::mpsc::Sender<Vec<u8>>,
    play: Receiver<Incoming>,
}

impl Client {
    fn new() -> Self {
        let cfg = Config::default();
        let shared = Arc::new(Shared::new(&cfg));
        let config = Arc::new(Mutex::new(cfg));
        let (voice, voice_rx) = tokio::sync::mpsc::channel(64);
        let (play_tx, play) = crossbeam_channel::bounded(1024);
        let net = net::spawn(shared.clone(), config.clone(), voice_rx, play_tx);
        Self { shared, config, net, voice, play }
    }

    fn connect(&self, addr: &str, name: &str, password: &str) {
        self.net
            .cmd
            .send(Command::Connect(Target { addr: addr.into(), name: name.into(), password: password.into() }))
            .unwrap();
    }

    fn conn(&self) -> ConnState {
        self.shared.status.lock().conn.clone()
    }

    fn connected(&self) -> bool {
        matches!(self.conn(), ConnState::Connected { .. })
    }

    fn my_id(&self) -> proto::UserId {
        self.shared.status.lock().my_id.unwrap()
    }

    fn users(&self) -> Vec<String> {
        self.shared.status.lock().users.values().map(|u| u.info.name.clone()).collect()
    }
}

async fn wait_for(what: &str, secs: u64, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn recv_voice(c: &Client, ms: u64) -> Option<Incoming> {
    c.play.recv_timeout(Duration::from_millis(ms)).ok()
}

fn settings(dir: &std::path::Path, port: u16) -> backroom_server::Settings {
    backroom_server::Settings {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        password: "secret".into(),
        name: "Test room".into(),
        data_dir: dir.to_path_buf(),
        max_users: 3,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_flow() {
    let tmp = std::env::temp_dir().join(format!("backroom-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::env::set_var("BACKROOM_CONFIG_DIR", tmp.join("client"));
    let srv_dir = tmp.join("server");

    let server = backroom_server::start(settings(&srv_dir, 0)).unwrap();
    let port = server.addr.port();
    let addr = format!("127.0.0.1:{port}");

    // --- two clients connect and see each other ---
    let a = Client::new();
    let b = Client::new();
    a.connect(&addr, "Alice", "secret");
    b.connect(&addr, "Bob", "secret");
    wait_for("A and B connected", 10, || a.connected() && b.connected() && a.users().len() == 2 && b.users().len() == 2).await;
    assert!(a.config.lock().fingerprints.contains_key(&addr), "fingerprint remembered (TOFU)");

    // --- voice A → B, no echo back to A ---
    a.voice.send(proto::encode_voice_up(proto::FLAG_START, 1, &[9, 9, 9])).await.unwrap();
    let p = recv_voice(&b, 2000).expect("B received A's voice");
    assert_eq!((p.sender, p.seq, p.flags, p.data.as_slice()), (a.my_id(), 1, proto::FLAG_START, &[9u8, 9, 9][..]));
    assert!(recv_voice(&a, 200).is_none(), "A does not hear itself");

    // --- mute: state visible to others, voice not forwarded ---
    a.net.cmd.send(Command::SetState { muted: true, deafened: false }).unwrap();
    let a_id = a.my_id();
    wait_for("B sees A muted", 5, || b.shared.status.lock().users.get(&a_id).is_some_and(|u| u.info.muted)).await;
    a.voice.send(proto::encode_voice_up(0, 2, &[1])).await.unwrap();
    assert!(recv_voice(&b, 300).is_none(), "server does not forward a muted user's voice");
    a.net.cmd.send(Command::SetState { muted: false, deafened: false }).unwrap();

    // --- a deafened user receives no voice ---
    b.net.cmd.send(Command::SetState { muted: false, deafened: true }).unwrap();
    let b_id = b.my_id();
    wait_for("A sees B deafened", 5, || a.shared.status.lock().users.get(&b_id).is_some_and(|u| u.info.deafened)).await;
    a.voice.send(proto::encode_voice_up(0, 3, &[2])).await.unwrap();
    assert!(recv_voice(&b, 300).is_none(), "deafened user receives nothing");
    b.net.cmd.send(Command::SetState { muted: false, deafened: false }).unwrap();

    // --- wrong password ---
    let c = Client::new();
    c.connect(&addr, "Carol", "wrong");
    wait_for("wrong password rejected", 10, || matches!(c.conn(), ConnState::Failed { .. })).await;
    assert_eq!(c.conn(), ConnState::Failed { reason: "Wrong password.".into() });

    // --- full room (limit 3) ---
    c.connect(&addr, "Carol", "secret");
    wait_for("C joins", 10, || c.connected()).await;
    let d = Client::new();
    d.connect(&addr, "Dave", "secret");
    wait_for("rejected when the room is full", 10, || matches!(d.conn(), ConnState::Failed { .. })).await;
    assert!(matches!(d.conn(), ConnState::Failed { reason } if reason.contains("full")));

    // --- same nickname: the new session wins, the old one does NOT keep coming back ---
    let a2 = Client::new();
    a2.connect(&addr, "alice", "secret");
    wait_for("A2 replaces A", 10, || a2.connected() && matches!(a.conn(), ConnState::Failed { .. })).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(a2.connected(), "A2 still connected (no ping-pong)");
    assert!(matches!(a.conn(), ConnState::Failed { .. }));
    wait_for("B sees 3 people", 5, || b.users().len() == 3).await;

    // --- server restart: clients come back by themselves ---
    server.shutdown().await;
    wait_for("B notices the drop", 15, || !b.connected()).await;
    let server = backroom_server::start(settings(&srv_dir, port)).unwrap();
    wait_for("B reconnects after the server restart", 20, || b.connected() && b.users().len() >= 2).await;
    wait_for("C reconnects after the server restart", 20, || c.connected()).await;
    let b_id = b.my_id();
    c.voice.send(proto::encode_voice_up(proto::FLAG_START, 10, &[7])).await.unwrap();
    let p = recv_voice(&b, 2000).expect("voice works after the restart");
    assert_eq!(p.data, vec![7]);
    assert_ne!(p.sender, b_id);

    // --- a different certificate at the same address: warning, no connection ---
    server.shutdown().await;
    let server = backroom_server::start(settings(&tmp.join("other-server"), port)).unwrap();
    wait_for("B detects the certificate change", 20, || matches!(b.conn(), ConnState::CertChanged { .. })).await;

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

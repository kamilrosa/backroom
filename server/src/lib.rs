//! Backroom server — a single voice room.
//!
//! The server neither decodes nor mixes anything: it receives an Opus
//! datagram from a client, prepends the sender id and relays it to everyone
//! else. That keeps CPU usage on a Raspberry Pi at a fraction of a percent
//! even with 10 people.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use backroom_proto::{self as proto, ClientMsg, ServerMsg, UserId, UserInfo};
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Server settings (from the command line or from a test).
#[derive(Debug, Clone)]
pub struct Settings {
    pub bind: SocketAddr,
    pub password: String,
    pub name: String,
    pub data_dir: PathBuf,
    pub max_users: usize,
}

/// Largest sensible voice packet; bigger ones are dropped.
const MAX_VOICE_PACKET: usize = 1000;
const MAX_LINE: usize = 4096;

struct Peer {
    info: UserInfo,
    conn: quinn::Connection,
    tx: mpsc::UnboundedSender<ServerMsg>,
}

#[derive(Default)]
struct Room {
    peers: HashMap<UserId, Peer>,
    next_id: UserId,
}

impl Room {
    fn alloc_id(&mut self) -> UserId {
        loop {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.peers.contains_key(&self.next_id) {
                return self.next_id;
            }
        }
    }

    fn broadcast(&self, msg: &ServerMsg, except: Option<UserId>) {
        for (id, p) in &self.peers {
            if Some(*id) != except {
                let _ = p.tx.send(msg.clone());
            }
        }
    }
}

type Shared = Arc<Mutex<Room>>;

struct Ctx {
    room: Shared,
    password: String,
    name: String,
    max_users: usize,
}

pub struct Server {
    endpoint: quinn::Endpoint,
    pub addr: SocketAddr,
    pub fingerprint: String,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Closes all connections (clients will see "server shut down").
    pub async fn shutdown(self) {
        self.endpoint.close(0u32.into(), b"server shut down");
        self.endpoint.wait_idle().await;
        self.task.abort();
    }

    /// Waits until the server stops accepting connections.
    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

/// Starts the server on the current tokio runtime.
pub fn start(s: Settings) -> Result<Server> {
    let (cert, key) = load_or_create_cert(&s.data_dir)?;
    let fingerprint = fingerprint(&cert);
    let endpoint = quinn::Endpoint::server(server_config(cert, key)?, s.bind)?;
    let addr = endpoint.local_addr()?;

    let ctx = Arc::new(Ctx {
        room: Shared::default(),
        password: s.password,
        name: s.name,
        max_users: s.max_users,
    });
    let ping = tokio::spawn(ping_loop(ctx.room.clone()));
    let ep = endpoint.clone();
    let task = tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                if let Err(e) = handle(ctx, incoming).await {
                    info!("{remote}: {e:#}");
                }
            });
        }
        ping.abort();
    });
    Ok(Server {
        endpoint,
        addr,
        fingerprint,
        task,
    })
}

fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    // ChaCha20 before AES: the Raspberry Pi 4 has no hardware AES, and
    // software ChaCha20 is several times faster on it. AES-128-GCM has to
    // stay on the list because QUIC uses it for initial packets.
    let mut provider = rustls::crypto::ring::default_provider();
    provider.cipher_suites = vec![
        rustls::crypto::ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256,
    ];
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![proto::ALPN.to_vec()];
    tls.ignore_client_order = true;

    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_idle_timeout(Some(Duration::from_secs(15).try_into()?))
        .keep_alive_interval(Some(Duration::from_secs(4)))
        .max_concurrent_uni_streams(0u32.into())
        .max_concurrent_bidi_streams(1u32.into());
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
}

fn load_or_create_cert(dir: &Path) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    if cert_path.exists() && key_path.exists() {
        let cert = std::fs::read(&cert_path).context("reading cert.der")?;
        let key = std::fs::read(&key_path).context("reading key.der")?;
        return Ok((
            CertificateDer::from(cert),
            PrivatePkcs8KeyDer::from(key).into(),
        ));
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let ck = rcgen::generate_simple_self_signed(vec![proto::SERVER_NAME.to_string()])?;
    let cert = ck.cert.der().to_vec();
    let key = ck.signing_key.serialize_der();
    std::fs::write(&cert_path, &cert)?;
    std::fs::write(&key_path, &key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    info!("Generated a new certificate in {}", dir.display());
    Ok((
        CertificateDer::from(cert),
        PrivatePkcs8KeyDer::from(key).into(),
    ))
}

fn fingerprint(cert: &[u8]) -> String {
    let h = Sha256::digest(cert);
    h.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<bool> {
    buf.clear();
    let n = (&mut *r)
        .take(MAX_LINE as u64)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() != Some(&b'\n') {
        bail!("control line too long");
    }
    buf.pop();
    Ok(true)
}

async fn handle(ctx: Arc<Ctx>, incoming: quinn::Incoming) -> Result<()> {
    let conn = incoming.await.context("establishing connection")?;
    let remote = conn.remote_address();
    let (mut send, recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi())
        .await
        .context("no control stream")??;
    let mut recv = BufReader::new(recv);
    let mut line = Vec::new();

    if !tokio::time::timeout(Duration::from_secs(10), read_line(&mut recv, &mut line)).await?? {
        bail!("disconnected before saying hello");
    }
    let ClientMsg::Hello {
        version,
        name,
        password,
    } = proto::decode_line(&line)?
    else {
        bail!("first message is not Hello");
    };

    if version != proto::PROTOCOL_VERSION {
        return Err(reject(
            &mut send,
            &conn,
            proto::version_mismatch_reason(proto::PROTOCOL_VERSION, version),
        )
        .await);
    }
    if password != ctx.password {
        return Err(reject(&mut send, &conn, "Wrong password.".into()).await);
    }
    let name = proto::sanitize_name(&name);
    if name.is_empty() {
        return Err(reject(&mut send, &conn, "Please enter a nickname.".into()).await);
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    let joined = {
        let mut room = ctx.room.lock().unwrap();
        // The same person after a dropped connection: the old session is a
        // "ghost" that has not timed out yet. Kick it instead of blocking the nickname.
        let ghost = room
            .peers
            .iter()
            .find(|(_, p)| p.info.name.eq_ignore_ascii_case(&name))
            .map(|(id, _)| *id);
        if let Some(gid) = ghost {
            if let Some(old) = room.peers.remove(&gid) {
                old.conn.close(
                    proto::CLOSE_REPLACED.into(),
                    b"replaced by a new connection",
                );
                room.broadcast(&ServerMsg::Left { id: gid }, None);
                info!("{name}: replacing previous session #{gid}");
            }
        }
        if room.peers.len() >= ctx.max_users {
            None
        } else {
            let my_id = room.alloc_id();
            let info = UserInfo {
                id: my_id,
                name: name.clone(),
                muted: false,
                deafened: false,
            };
            let mut users: Vec<UserInfo> = room.peers.values().map(|p| p.info.clone()).collect();
            users.push(info.clone());
            users.sort_by_key(|u| u.id);
            let _ = tx.send(ServerMsg::Welcome {
                your_id: my_id,
                server_name: ctx.name.clone(),
                users,
            });
            room.broadcast(&ServerMsg::Joined { user: info.clone() }, None);
            room.peers.insert(
                my_id,
                Peer {
                    info,
                    conn: conn.clone(),
                    tx,
                },
            );
            Some(my_id)
        }
    };
    let Some(my_id) = joined else {
        return Err(reject(
            &mut send,
            &conn,
            format!("The room is full ({} people).", ctx.max_users),
        )
        .await);
    };
    info!("{name} (#{my_id}) joined from {remote}");

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if send.write_all(&proto::encode_line(&msg)).await.is_err() {
                break;
            }
        }
    });

    let result = session(&ctx, &conn, my_id, recv).await;

    {
        let mut room = ctx.room.lock().unwrap();
        // Remove only if this is still our session (it may have been replaced).
        if room
            .peers
            .get(&my_id)
            .is_some_and(|p| p.conn.stable_id() == conn.stable_id())
        {
            room.peers.remove(&my_id);
            room.broadcast(&ServerMsg::Left { id: my_id }, None);
        }
    }
    writer.abort();
    info!(
        "{name} (#{my_id}) left: {}",
        match &result {
            Ok(()) => "disconnected".to_string(),
            Err(e) => format!("{e:#}"),
        }
    );
    Ok(())
}

async fn reject(
    send: &mut quinn::SendStream,
    conn: &quinn::Connection,
    reason: String,
) -> anyhow::Error {
    let _ = send
        .write_all(&proto::encode_line(&ServerMsg::Reject {
            reason: reason.clone(),
        }))
        .await;
    let _ = send.finish();
    // Give the reason time to arrive before closing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.close(1u32.into(), b"rejected");
    anyhow::anyhow!("rejected: {reason}")
}

/// Reads control lines in a separate task — `select!` with `read_until`
/// could lose a partially read line when the voice branch wins.
fn spawn_reader(mut recv: BufReader<quinn::RecvStream>) -> mpsc::Receiver<Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            let mut line = Vec::new();
            let res = read_line(&mut recv, &mut line).await;
            let (msg, stop) = match res {
                Ok(true) => (Ok(line), false),
                Ok(false) => break,
                Err(e) => (Err(e), true),
            };
            if tx.send(msg).await.is_err() || stop {
                break;
            }
        }
    });
    rx
}

async fn session(
    ctx: &Ctx,
    conn: &quinn::Connection,
    my_id: UserId,
    recv: BufReader<quinn::RecvStream>,
) -> Result<()> {
    let mut lines = spawn_reader(recv);
    loop {
        tokio::select! {
            dg = conn.read_datagram() => {
                let dg = dg?;
                if dg.len() > MAX_VOICE_PACKET {
                    continue;
                }
                forward_voice(&ctx.room, my_id, &dg);
            }
            line = lines.recv() => {
                let Some(line) = line else { return Ok(()) };
                match proto::decode_line::<ClientMsg>(&line?) {
                    Ok(ClientMsg::State { muted, deafened }) => {
                        let mut room = ctx.room.lock().unwrap();
                        if let Some(p) = room.peers.get_mut(&my_id) {
                            p.info.muted = muted;
                            p.info.deafened = deafened;
                        }
                        room.broadcast(&ServerMsg::State { id: my_id, muted, deafened }, None);
                    }
                    Ok(other) => warn!("#{my_id}: unexpected message {other:?}"),
                    Err(e) => warn!("#{my_id}: malformed message: {e}"),
                }
            }
        }
    }
}

fn forward_voice(room: &Shared, sender: UserId, up: &[u8]) {
    let Some(down) = proto::up_to_down(sender, up) else {
        return;
    };
    let down = Bytes::from(down);
    let room = room.lock().unwrap();
    match room.peers.get(&sender) {
        // A muted user should not send anything; if they do, ignore it.
        Some(p) if !p.info.muted => {}
        _ => return,
    }
    for (id, p) in &room.peers {
        if *id != sender && !p.info.deafened {
            // An error (e.g. a slow client's full buffer) means a lost packet,
            // which the receiver conceals. Never block the others.
            let _ = p.conn.send_datagram(down.clone());
        }
    }
}

async fn ping_loop(room: Shared) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        let room = room.lock().unwrap();
        if room.peers.is_empty() {
            continue;
        }
        let pings = room
            .peers
            .iter()
            .map(|(id, p)| (*id, p.conn.rtt().as_millis() as u32))
            .collect();
        room.broadcast(&ServerMsg::Pings { pings }, None);
    }
}

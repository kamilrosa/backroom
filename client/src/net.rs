//! Server connection: QUIC, automatic reconnection, certificate TOFU.
//!
//! A dropped connection does not touch audio — the microphone and speakers
//! keep running while the client reconnects by itself (1 s, 2 s, 4 s, then every 5 s).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use crossbeam_channel::Sender;
use backroom_proto::{self as proto, ClientMsg, ServerMsg};
use parking_lot::Mutex;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio::Incoming;
use crate::config::Config;
use crate::state::{ConnState, Shared, UserView};

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub addr: String,
    pub name: String,
    pub password: String,
}

#[derive(Debug)]
pub enum Command {
    Connect(Target),
    Disconnect,
    SetState { muted: bool, deafened: bool },
}

pub struct NetHandle {
    pub cmd: mpsc::UnboundedSender<Command>,
}

pub fn spawn(
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    voice_rx: mpsc::Receiver<Vec<u8>>,
    play_tx: Sender<Incoming>,
) -> NetHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("backroom-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(manager(shared, config, cmd_rx, voice_rx, play_tx));
        })
        .expect("network thread");
    NetHandle { cmd: cmd_tx }
}

enum End {
    UserDisconnect,
    Switch(Target),
    Rejected(String),
    CertChanged { expected: String, got: String },
    Lost { reason: String, was_connected: bool },
}

struct Io {
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    voice_rx: mpsc::Receiver<Vec<u8>>,
    play_tx: Sender<Incoming>,
}

async fn manager(
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    voice_rx: mpsc::Receiver<Vec<u8>>,
    play_tx: Sender<Incoming>,
) {
    let mut io = Io { shared, config, cmd_rx, voice_rx, play_tx };
    let mut target: Option<Target> = None;
    let mut backoff = Duration::from_secs(1);

    loop {
        let Some(t) = target.clone() else {
            tokio::select! {
                cmd = io.cmd_rx.recv() => match cmd {
                    Some(Command::Connect(t)) => {
                        backoff = Duration::from_secs(1);
                        io.shared.update(|s| s.conn = ConnState::Connecting);
                        target = Some(t);
                    }
                    Some(_) => {}
                    None => return,
                },
                // Without a connection, microphone packets are simply dropped.
                _ = io.voice_rx.recv() => {}
            }
            continue;
        };

        let end = run_once(&t, &mut io).await;
        io.shared.update(|s| {
            s.users.clear();
            s.my_id = None;
        });
        match end {
            End::UserDisconnect => {
                target = None;
                io.shared.update(|s| s.conn = ConnState::Disconnected);
            }
            End::Switch(nt) => {
                backoff = Duration::from_secs(1);
                io.shared.update(|s| s.conn = ConnState::Connecting);
                target = Some(nt);
            }
            End::Rejected(reason) => {
                target = None;
                io.shared.update(|s| s.conn = ConnState::Failed { reason });
            }
            End::CertChanged { expected, got } => {
                target = None;
                io.shared.update(|s| s.conn = ConnState::CertChanged { expected, got });
            }
            End::Lost { reason, was_connected } => {
                if was_connected {
                    backoff = Duration::from_secs(1);
                }
                warn!("Connection: {reason} — retrying in {} s", backoff.as_secs());
                io.shared.update(|s| s.conn = ConnState::Reconnecting { reason });
                let deadline = tokio::time::sleep(backoff);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        _ = &mut deadline => break,
                        cmd = io.cmd_rx.recv() => match cmd {
                            Some(Command::Disconnect) | None => {
                                target = None;
                                io.shared.update(|s| s.conn = ConnState::Disconnected);
                                break;
                            }
                            Some(Command::Connect(nt)) => {
                                target = Some(nt);
                                break;
                            }
                            Some(Command::SetState { .. }) => {}
                        },
                        _ = io.voice_rx.recv() => {}
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

pub fn parse_addr(s: &str) -> (String, u16) {
    let s = s.trim();
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return (sa.ip().to_string(), sa.port());
    }
    if let Some((h, p)) = s.rsplit_once(':') {
        if !h.contains(':') {
            if let Ok(p) = p.parse() {
                return (h.to_string(), p);
            }
        }
    }
    (s.trim_matches(|c| c == '[' || c == ']').to_string(), proto::DEFAULT_PORT)
}

async fn run_once(t: &Target, io: &mut Io) -> End {
    let lost = |e: anyhow::Error, was_connected: bool| End::Lost { reason: format!("{e:#}"), was_connected };

    let (host, port) = parse_addr(&t.addr);
    let addr = match tokio::net::lookup_host((host.as_str(), port)).await.map(|mut a| a.next()) {
        Ok(Some(a)) => a,
        Ok(None) | Err(_) => return lost(anyhow!("server \"{host}\" not found"), false),
    };
    let key = format!("{host}:{port}");
    let expected = io.config.lock().fingerprints.get(&key).cloned();
    let seen = Arc::new(Mutex::new(None::<String>));

    let conn = match connect(addr, expected.clone(), seen.clone()).await {
        Ok(c) => c,
        Err(e) => {
            if let (Some(exp), Some(got)) = (expected, seen.lock().clone()) {
                if exp != got {
                    return End::CertChanged { expected: exp, got };
                }
            }
            return lost(e, false);
        }
    };
    if expected.is_none() {
        if let Some(fp) = seen.lock().clone() {
            info!("Remembering server fingerprint {key}: {fp}");
            let mut cfg = io.config.lock();
            cfg.fingerprints.insert(key.clone(), fp);
            cfg.save();
        }
    }

    let watch = conn.clone();
    match session(conn, t, io).await {
        Ok(end) => end,
        Err(e) => match watch.close_reason() {
            Some(quinn::ConnectionError::ApplicationClosed(c)) if c.error_code == proto::CLOSE_REPLACED.into() => {
                End::Rejected("Someone connected with the same nickname — you were disconnected.".into())
            }
            _ => lost(e, true),
        },
    }
}

async fn connect(addr: SocketAddr, expected: Option<String>, seen: Arc<Mutex<Option<String>>>) -> Result<quinn::Connection> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Tofu { expected, seen, provider: provider.clone() });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![proto::ALPN.to_vec()];

    let mut cfg = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_idle_timeout(Some(Duration::from_secs(10).try_into()?))
        .keep_alive_interval(Some(Duration::from_secs(2)));
    cfg.transport_config(Arc::new(transport));

    // A new local port on every attempt — sidesteps stale NAT entries.
    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(cfg);
    let conn = tokio::time::timeout(Duration::from_secs(8), endpoint.connect(addr, proto::SERVER_NAME)?)
        .await
        .map_err(|_| anyhow!("server is not responding ({addr})"))?
        .context("establishing connection")?;
    Ok(conn)
}

const MAX_LINE: u64 = 64 * 1024;

/// Reads control lines in a separate task so `select!` never loses
/// partially read data.
fn spawn_reader(recv: quinn::RecvStream) -> mpsc::Receiver<Result<ServerMsg>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut r = BufReader::new(recv);
        loop {
            let mut buf = Vec::new();
            let res = (&mut r).take(MAX_LINE).read_until(b'\n', &mut buf).await;
            let msg = match res {
                Ok(0) => break,
                Ok(_) if buf.last() != Some(&b'\n') => Err(anyhow!("malformed server message")),
                Ok(_) => proto::decode_line::<ServerMsg>(&buf[..buf.len() - 1]).map_err(Into::into),
                Err(e) => Err(e.into()),
            };
            let stop = msg.is_err();
            if tx.send(msg).await.is_err() || stop {
                break;
            }
        }
    });
    rx
}

async fn session(conn: quinn::Connection, t: &Target, io: &mut Io) -> Result<End> {
    let (mut send, recv) = conn.open_bi().await?;
    let mut lines = spawn_reader(recv);
    send.write_all(&proto::encode_line(&ClientMsg::Hello {
        version: proto::PROTOCOL_VERSION,
        name: t.name.clone(),
        password: t.password.clone(),
    }))
    .await?;

    let first = tokio::time::timeout(Duration::from_secs(8), lines.recv())
        .await
        .map_err(|_| anyhow!("server did not say hello"))?;
    let (my_id, server_name, users) = match first {
        Some(Ok(ServerMsg::Welcome { your_id, server_name, users })) => (your_id, server_name, users),
        Some(Ok(ServerMsg::Reject { reason })) => return Ok(End::Rejected(reason)),
        Some(Ok(other)) => bail!("unexpected server response: {other:?}"),
        Some(Err(e)) => return Err(e),
        None => bail!("server closed the connection"),
    };
    info!("Connected to \"{server_name}\" as #{my_id}");
    io.shared.update(|s| {
        s.conn = ConnState::Connected { server_name };
        s.my_id = Some(my_id);
        s.users = users.into_iter().map(|u| (u.id, UserView { info: u, ping_ms: None, last_voice: None })).collect();
    });
    // Mute state from before the (re)connection.
    let muted = io.shared.muted.load(std::sync::atomic::Ordering::Relaxed);
    let deafened = io.shared.deafened.load(std::sync::atomic::Ordering::Relaxed);
    if muted || deafened {
        send.write_all(&proto::encode_line(&ClientMsg::State { muted, deafened })).await?;
    }

    // Drop whatever piled up in the queue before connecting.
    while io.voice_rx.try_recv().is_ok() {}

    loop {
        tokio::select! {
            cmd = io.cmd_rx.recv() => match cmd {
                Some(Command::Disconnect) | None => {
                    conn.close(0u32.into(), b"bye");
                    return Ok(End::UserDisconnect);
                }
                Some(Command::Connect(nt)) => {
                    if nt == *t {
                        continue;
                    }
                    conn.close(0u32.into(), b"switching server");
                    return Ok(End::Switch(nt));
                }
                Some(Command::SetState { muted, deafened }) => {
                    send.write_all(&proto::encode_line(&ClientMsg::State { muted, deafened })).await?;
                }
            },
            v = io.voice_rx.recv() => {
                if let Some(v) = v {
                    // An error = the packet is lost; a dead connection is detected elsewhere.
                    let _ = conn.send_datagram(Bytes::from(v));
                }
            }
            dg = conn.read_datagram() => {
                let dg = dg?;
                if let Some(p) = proto::decode_voice_down(&dg) {
                    if let Some(u) = io.shared.status.lock().users.get_mut(&p.sender) {
                        u.last_voice = Some(Instant::now());
                    }
                    let _ = io.play_tx.try_send(Incoming { sender: p.sender, seq: p.seq, flags: p.flags, data: p.opus.to_vec() });
                }
            }
            msg = lines.recv() => {
                match msg {
                    Some(Ok(m)) => handle_msg(&io.shared, m),
                    Some(Err(e)) => return Err(e),
                    None => {
                        let reason = conn.close_reason().map(|r| r.to_string()).unwrap_or_else(|| "server closed the connection".into());
                        bail!("{reason}");
                    }
                }
            }
        }
    }
}

fn handle_msg(shared: &Shared, m: ServerMsg) {
    shared.update(|s| match m {
        ServerMsg::Joined { user } => {
            s.users.insert(user.id, UserView { info: user, ping_ms: None, last_voice: None });
        }
        ServerMsg::Left { id } => {
            s.users.remove(&id);
        }
        ServerMsg::State { id, muted, deafened } => {
            if let Some(u) = s.users.get_mut(&id) {
                u.info.muted = muted;
                u.info.deafened = deafened;
            }
        }
        ServerMsg::Pings { pings } => {
            for (id, ms) in pings {
                if let Some(u) = s.users.get_mut(&id) {
                    u.ping_ms = Some(ms);
                }
            }
        }
        ServerMsg::Welcome { .. } | ServerMsg::Reject { .. } => {}
    });
}

pub fn fingerprint(cert: &[u8]) -> String {
    Sha256::digest(cert).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// "Trust on first use": the first server certificate is remembered, every
/// later one must have the same fingerprint. Protects the password from
/// eavesdropping without buying a certificate for a home Raspberry Pi.
#[derive(Debug)]
struct Tofu {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Tofu {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity);
        *self.seen.lock() = Some(fp.clone());
        match &self.expected {
            Some(e) if *e != fp => Err(rustls::Error::General("server certificate fingerprint has changed".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::parse_addr;

    #[test]
    fn addresses() {
        assert_eq!(parse_addr("pi.local"), ("pi.local".into(), 7777));
        assert_eq!(parse_addr("pi.local:9000"), ("pi.local".into(), 9000));
        assert_eq!(parse_addr("192.168.1.5"), ("192.168.1.5".into(), 7777));
        assert_eq!(parse_addr("192.168.1.5:1234"), ("192.168.1.5".into(), 1234));
        assert_eq!(parse_addr("[::1]:5000"), ("::1".into(), 5000));
        assert_eq!(parse_addr("::1"), ("::1".into(), 7777));
        assert_eq!(parse_addr(" my.server.org "), ("my.server.org".into(), 7777));
    }
}

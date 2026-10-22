//! Quick server check without audio:
//!
//!   cargo run --release --example probe -- address[:port] password [nickname]
//!
//! Connects, prints the people in the room and their ping, then exits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use backroom_client::config::Config;
use backroom_client::net::{self, Command, Target};
use backroom_client::state::{ConnState, Shared};
use parking_lot::Mutex;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: probe address[:port] password [nickname]");
        std::process::exit(2);
    }
    // Separate directory: leave the real client's settings alone.
    let dir = std::env::temp_dir().join("backroom-probe");
    std::env::set_var("BACKROOM_CONFIG_DIR", &dir);

    let cfg = Config::default();
    let shared = Arc::new(Shared::new(&cfg));
    let (_voice_tx, voice_rx) = tokio::sync::mpsc::channel(8);
    let (play_tx, _play_rx) = crossbeam_channel::bounded(8);
    let h = net::spawn(shared.clone(), Arc::new(Mutex::new(cfg)), voice_rx, play_tx);
    let name = a.get(3).cloned().unwrap_or_else(|| "probe".into());
    h.cmd.send(Command::Connect(Target { addr: a[1].clone(), name, password: a[2].clone() })).unwrap();

    let t = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let st = shared.status.lock().clone();
        match &st.conn {
            ConnState::Connected { server_name } if t.elapsed() > Duration::from_millis(2500) => {
                println!("OK: \"{server_name}\", {} in the room:", st.users.len());
                for u in st.users.values() {
                    let ping = u.ping_ms.map_or("?".into(), |p| format!("{p} ms"));
                    println!("  - {} (ping {ping})", u.info.name);
                }
                let _ = h.cmd.send(Command::Disconnect);
                std::thread::sleep(Duration::from_millis(200));
                return;
            }
            ConnState::Failed { reason } => fail(reason),
            ConnState::CertChanged { .. } => fail("server certificate differs from the previous probe"),
            ConnState::Reconnecting { reason } if t.elapsed() > Duration::from_secs(10) => fail(reason),
            _ if t.elapsed() > Duration::from_secs(15) => fail("no response from the server"),
            _ => {}
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("ERROR: {msg}");
    std::process::exit(1);
}

// Release builds on Windows run without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use backroom_client::{audio, config, hotkey, net, state, ui};

use std::sync::Arc;

use eframe::egui;
use parking_lot::Mutex;

fn init_logging() {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let dir = config::dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("backroom.log");
    // Keep the log short: enough to report a problem.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,wgpu=warn,naga=warn,eframe=warn,egui=warn,quinn=warn,tract=warn".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => builder.with_writer(std::io::stderr.and(std::sync::Mutex::new(f))).init(),
        Err(_) => builder.with_writer(std::io::stderr).init(),
    }
    tracing::info!("Backroom {} start, log: {}", env!("CARGO_PKG_VERSION"), path.display());
}

fn main() -> eframe::Result {
    init_logging();
    let cfg = config::Config::load();
    let shared = Arc::new(state::Shared::new(&cfg));
    let hk = Arc::new(hotkey::HotkeyState::default());
    *hk.binding.lock() = cfg.ptt_binding.clone();
    let config = Arc::new(Mutex::new(cfg));

    let (voice_tx, voice_rx) = tokio::sync::mpsc::channel(64);
    let (play_tx, play_rx) = crossbeam_channel::bounded(1024);
    let (clip_tx, clip_rx) = crossbeam_channel::bounded(2);
    audio::spawn_playback(shared.clone(), play_rx, clip_rx);
    audio::spawn_capture(shared.clone(), voice_tx, clip_tx);
    let net = net::spawn(shared.clone(), config.clone(), voice_rx, play_tx);
    hotkey::spawn(shared.clone(), hk.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Backroom")
            .with_inner_size([420.0, 680.0])
            .with_min_inner_size([340.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Backroom",
        options,
        Box::new(move |cc| {
            shared.set_repaint(cc.egui_ctx.clone());
            Ok(Box::new(ui::App::new(shared, config, net, hk)))
        }),
    )
}

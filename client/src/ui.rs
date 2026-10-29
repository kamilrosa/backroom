//! The program window. Deliberately few options: connect, talk, listen.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backroom_proto::ChatLine;
use eframe::egui::{self, Color32, RichText};
use parking_lot::Mutex;

use crate::config::{Config, Sensitivity};
use crate::hotkey::HotkeyState;
use crate::net::{Command, NetHandle, Target};
use crate::state::{ConnState, DeviceStatus, MicTest, Shared, Status, MIC_TEST_LEN};

const GREEN: Color32 = Color32::from_rgb(64, 192, 87);
const RED: Color32 = Color32::from_rgb(230, 80, 80);
const AMBER: Color32 = Color32::from_rgb(230, 170, 50);
/// How long someone's last chat message is shown under their nickname.
const RECENT_MESSAGE: Duration = Duration::from_secs(5 * 60);

pub struct App {
    shared: Arc<Shared>,
    config: Arc<Mutex<Config>>,
    net: NetHandle,
    hk: Arc<HotkeyState>,
    server: String,
    name: String,
    password: String,
    dirty: Option<Instant>,
    show_settings: bool,
    chat_input: String,
    title: String,
    last_unread: u32,
}

impl App {
    pub fn new(shared: Arc<Shared>, config: Arc<Mutex<Config>>, net: NetHandle, hk: Arc<HotkeyState>) -> Self {
        let (server, name, password, auto) = {
            let c = config.lock();
            (c.server.clone(), c.name.clone(), c.password.clone(), c.auto_connect)
        };
        let app = Self {
            shared,
            config,
            net,
            hk,
            server,
            name,
            password,
            dirty: None,
            show_settings: false,
            chat_input: String::new(),
            title: "Backroom".into(),
            last_unread: 0,
        };
        if auto && !app.server.is_empty() && !app.name.is_empty() {
            app.connect();
        }
        app
    }

    fn connect(&self) {
        let _ = self.net.cmd.send(Command::Connect(Target {
            addr: self.server.trim().to_string(),
            name: self.name.trim().to_string(),
            password: self.password.clone(),
        }));
    }

    fn edit_config(&mut self, f: impl FnOnce(&mut Config)) {
        f(&mut self.config.lock());
        self.dirty.get_or_insert_with(Instant::now);
    }

    fn send_state(&self) {
        let _ = self.net.cmd.send(Command::SetState {
            muted: self.shared.muted.load(Ordering::Relaxed),
            deafened: self.shared.deafened.load(Ordering::Relaxed),
            away: self.shared.away.load(Ordering::Relaxed),
        });
    }

    /// Unread counter in the window title, taskbar flash on a new message.
    fn unread_ui(&mut self, ctx: &egui::Context, st: &Status) {
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        self.shared.focused.store(focused, Ordering::Relaxed);
        if focused && st.unread > 0 {
            self.shared.status.lock().unread = 0;
        }
        let unread = if focused { 0 } else { st.unread };
        if unread > self.last_unread {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }
        self.last_unread = unread;
        let title = if unread > 0 { format!("Backroom ({unread})") } else { "Backroom".into() };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn sync_gains(&self, st: &Status) {
        let cfg = self.config.lock();
        let gains = st.users.values().map(|u| (u.info.id, cfg.volume_of(&u.info.name))).collect();
        *self.shared.gains.lock() = gains;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let st = self.shared.status.lock().clone();
        self.sync_gains(&st);
        let captured = self.hk.captured.lock().take();
        if let Some(b) = captured {
            *self.hk.binding.lock() = Some(b.clone());
            self.edit_config(|c| c.ptt_binding = Some(b));
        }
        if self.dirty.is_some_and(|t| t.elapsed() > Duration::from_secs(1)) {
            self.config.lock().save();
            self.dirty = None;
        }
        // The speaking and level indicators need smooth repainting.
        ui.ctx().request_repaint_after(Duration::from_millis(50));
        self.unread_ui(&ui.ctx().clone(), &st);

        egui::Panel::top("conn").show(ui, |ui| {
            ui.add_space(6.0);
            self.connection_ui(ui, &st);
            ui.add_space(6.0);
        });
        egui::Panel::bottom("me").show(ui, |ui| {
            ui.add_space(6.0);
            self.me_ui(ui, &st);
            ui.add_space(6.0);
        });
        let mut typing = false;
        egui::CentralPanel::default().show(ui, |ui| {
            if matches!(st.conn, ConnState::Connected { .. } | ConnState::Reconnecting { .. }) {
                egui::Panel::bottom("chat")
                    .resizable(true)
                    .default_size(180.0)
                    .show(ui, |ui| typing = self.chat_ui(ui, &st));
            }
            egui::ScrollArea::vertical().show(ui, |ui| self.users_ui(ui, &st));
        });
        self.shared.typing.store(typing, Ordering::Relaxed);
    }

    fn on_exit(&mut self) {
        self.config.lock().save();
        let _ = self.net.cmd.send(Command::Disconnect);
        // A moment to send "bye" so others see us leave right away.
        std::thread::sleep(Duration::from_millis(150));
    }
}

impl App {
    fn connection_ui(&mut self, ui: &mut egui::Ui, st: &Status) {
        match &st.conn {
            ConnState::Connected { server_name } => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(GREEN));
                    ui.label(RichText::new(server_name).strong());
                    if let Some(p) = st.me().and_then(|u| u.ping_ms) {
                        ui.label(RichText::new(format!("ping {p} ms")).weak());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Disconnect").clicked() {
                            let _ = self.net.cmd.send(Command::Disconnect);
                            self.edit_config(|c| c.auto_connect = false);
                        }
                    });
                });
            }
            ConnState::Connecting | ConnState::Reconnecting { .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    match &st.conn {
                        ConnState::Reconnecting { reason } => {
                            ui.label(RichText::new("Connection lost — reconnecting…").color(AMBER))
                                .on_hover_text(reason);
                        }
                        _ => {
                            ui.label("Connecting…");
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            let _ = self.net.cmd.send(Command::Disconnect);
                        }
                    });
                });
            }
            ConnState::Disconnected | ConnState::Failed { .. } | ConnState::CertChanged { .. } => {
                self.login_form(ui, st);
            }
        }
    }

    fn login_form(&mut self, ui: &mut egui::Ui, st: &Status) {
        ui.heading("Backroom");
        egui::Grid::new("login").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Server");
            ui.add(egui::TextEdit::singleline(&mut self.server).hint_text("address, e.g. 85.12.34.56"));
            ui.end_row();
            ui.label("Nick");
            ui.add(egui::TextEdit::singleline(&mut self.name).char_limit(backroom_proto::MAX_NAME_LEN));
            ui.end_row();
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
            ui.end_row();
        });
        match &st.conn {
            ConnState::Failed { reason } => {
                ui.colored_label(RED, reason);
            }
            ConnState::CertChanged { expected, got } => {
                ui.colored_label(
                    AMBER,
                    "The server presented a different certificate than usual. That is normal after \
                     reinstalling the server; otherwise someone may be impersonating it.",
                );
                ui.label(RichText::new(format!("was: {}\nnow: {}", short_fp(expected), short_fp(got))).monospace().small());
                if ui.button("It's my server — trust and connect").clicked() {
                    let key = cert_key(&self.server);
                    self.edit_config(|c| {
                        c.fingerprints.remove(&key);
                    });
                    self.config.lock().save();
                    self.connect();
                }
            }
            _ => {}
        }
        let ready = !self.server.trim().is_empty() && !self.name.trim().is_empty();
        if ui.add_enabled(ready, egui::Button::new(RichText::new("Connect").strong())).clicked() {
            let (server, name, password) = (self.server.trim().to_string(), self.name.trim().to_string(), self.password.clone());
            self.edit_config(|c| {
                c.server = server;
                c.name = name;
                c.password = password;
                c.auto_connect = true;
            });
            self.connect();
        }
    }

    fn users_ui(&mut self, ui: &mut egui::Ui, st: &Status) {
        if st.users.is_empty() {
            if matches!(st.conn, ConnState::Connected { .. }) {
                ui.label("Nobody here.");
            }
            return;
        }
        let transmitting = self.shared.transmitting.load(Ordering::Relaxed);
        for u in st.users.values() {
            let me = Some(u.info.id) == st.my_id;
            let speaking = if me {
                transmitting
            } else {
                u.last_voice.is_some_and(|t| t.elapsed() < Duration::from_millis(300))
            };
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                let color = if speaking { GREEN } else { ui.visuals().weak_text_color().gamma_multiply(0.4) };
                ui.painter().circle_filled(rect.center(), 5.0, color);
                let mut name = RichText::new(&u.info.name);
                if me {
                    name = name.strong();
                }
                ui.label(name);
                if u.info.away {
                    ui.label(RichText::new("away").small().color(AMBER));
                }
                if u.info.deafened {
                    ui.label(RichText::new("deafened").small().color(AMBER));
                } else if u.info.muted {
                    ui.label(RichText::new("muted").small().color(AMBER));
                }
                if let Some(p) = u.ping_ms {
                    let c = if p > 150 { AMBER } else { ui.visuals().weak_text_color() };
                    ui.label(RichText::new(format!("{p} ms")).small().color(c));
                }
                if !me {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let name = u.info.name.clone();
                        let (mut vol, mut lmute) = {
                            let c = self.config.lock();
                            (c.volumes.get(&name).copied().unwrap_or(1.0), c.local_mutes.contains(&name))
                        };
                        let icon = if lmute { "🔇" } else { "🔈" };
                        let mute_btn = ui.toggle_value(&mut lmute, icon)
                            .on_hover_text("Mute for me only");
                        let slider = ui.add(
                            egui::Slider::new(&mut vol, 0.0..=2.0)
                                .show_value(false)
                                .clamping(egui::SliderClamping::Always),
                        ).on_hover_text(format!("Volume: {:.0}%", vol * 100.0));
                        if mute_btn.changed() || slider.changed() {
                            self.edit_config(|c| {
                                c.volumes.insert(name.clone(), vol);
                                if lmute {
                                    c.local_mutes.insert(name.clone());
                                } else {
                                    c.local_mutes.remove(&name);
                                }
                            });
                        }
                    });
                }
            });
            if let Some(text) = recent_message(&st.chat, &u.info.name) {
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    ui.add(egui::Label::new(RichText::new(text).small().italics().weak()).truncate());
                });
            }
        }
    }

    /// Returns whether the message box has focus (push-to-talk is ignored then).
    fn chat_ui(&mut self, ui: &mut egui::Ui, st: &Status) -> bool {
        let connected = matches!(st.conn, ConnState::Connected { .. });
        let my_name = st.my_name();
        let mut typing = false;
        ui.add_space(4.0);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if connected && !st.chat_available {
                ui.colored_label(AMBER, "Chat needs a newer server — ask whoever runs it to update.");
            } else {
                let input = ui.add_enabled(
                    connected,
                    egui::TextEdit::singleline(&mut self.chat_input)
                        .hint_text("Message… (Enter sends)")
                        .char_limit(backroom_proto::MAX_CHAT_LEN)
                        .desired_width(f32::INFINITY),
                );
                typing = input.has_focus();
                if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let text = std::mem::take(&mut self.chat_input);
                    if !text.trim().is_empty() {
                        let _ = self.net.cmd.send(Command::Chat(text));
                    }
                    input.request_focus();
                }
            }
            ui.add_space(4.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                    if st.chat.is_empty() {
                        ui.label(RichText::new("No messages yet.").weak());
                    }
                    for line in &st.chat {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.label(RichText::new(clock(line.time)).color(ui.visuals().weak_text_color().gamma_multiply(0.7)));
                            let mut name = RichText::new(&line.name).strong();
                            if Some(line.name.as_str()) == my_name {
                                name = name.color(GREEN);
                            }
                            ui.label(name);
                            ui.label(&line.text);
                        });
                    }
                });
            });
        });
        typing
    }

    fn me_ui(&mut self, ui: &mut egui::Ui, st: &Status) {
        let shared = self.shared.clone();
        ui.horizontal(|ui| {
            let mut muted = shared.muted.load(Ordering::Relaxed);
            let mut deaf = shared.deafened.load(Ordering::Relaxed);
            let mute_label = if muted || deaf { "🎤 Microphone muted" } else { "🎤 Microphone on" };
            if ui.add_enabled(!deaf, egui::Button::selectable(muted, mute_label)).clicked() {
                muted = !muted;
                shared.muted.store(muted, Ordering::Relaxed);
                self.send_state();
            }
            let deaf_label = if deaf { "🔇 Deafened" } else { "🔊 Listening" };
            if ui.add(egui::Button::selectable(deaf, deaf_label)).on_hover_text("Deafen = you hear nothing and transmit nothing").clicked() {
                deaf = !deaf;
                shared.deafened.store(deaf, Ordering::Relaxed);
                self.send_state();
            }
            let away = shared.away.load(Ordering::Relaxed);
            if ui.add(egui::Button::selectable(away, "☕ Away")).on_hover_text("Tell the others you'll be right back").clicked() {
                shared.away.store(!away, Ordering::Relaxed);
                self.send_state();
            }
        });

        ui.add_space(4.0);
        let mut ptt = shared.push_to_talk.load(Ordering::Relaxed);
        ui.horizontal(|ui| {
            let a = ui.radio_value(&mut ptt, false, "Voice activation");
            let b = ui.radio_value(&mut ptt, true, "Push-to-talk");
            if a.changed() || b.changed() {
                shared.push_to_talk.store(ptt, Ordering::Relaxed);
                self.edit_config(|c| c.push_to_talk = ptt);
            }
        });
        if ptt {
            ui.horizontal(|ui| {
                ui.label("Key:");
                let capturing = self.hk.capturing.load(Ordering::Relaxed);
                let label = if capturing {
                    "press a key or mouse button… (Esc = cancel)".to_string()
                } else {
                    self.hk.binding.lock().as_ref().map_or("not set".into(), |b| b.label())
                };
                ui.label(RichText::new(label).strong());
                if !capturing && ui.button("Change").clicked() {
                    self.hk.capturing.store(true, Ordering::Relaxed);
                }
            });
            if !st.ptt_available && !st.ptt_message.is_empty() {
                ui.colored_label(AMBER, &st.ptt_message);
            }
        } else {
            ui.horizontal(|ui| {
                ui.label("Sensitivity:");
                let mut sens = self.config.lock().sensitivity;
                let mut changed = false;
                for s in [Sensitivity::Low, Sensitivity::Normal, Sensitivity::High] {
                    changed |= ui.selectable_value(&mut sens, s, s.label()).changed();
                }
                if changed {
                    shared.vad_threshold.set(sens.threshold_db());
                    self.edit_config(|c| c.sensitivity = sens);
                }
            })
            .response
            .on_hover_text("High — also picks up quiet speech. Low — only clear speech.");
        }

        // Level meter + whether we are transmitting.
        let level = shared.mic_level_db.get();
        let tx = shared.transmitting.load(Ordering::Relaxed);
        let frac = ((level + 60.0) / 60.0).clamp(0.0, 1.0);
        ui.horizontal(|ui| {
            let bar = egui::ProgressBar::new(frac)
                .desired_height(8.0)
                .desired_width(ui.available_width() - 80.0)
                .fill(if tx { GREEN } else { ui.visuals().weak_text_color() });
            ui.add(bar);
            let testing = shared.mic_test.lock().active();
            ui.label(match (tx, testing) {
                (true, false) => RichText::new("transmitting").color(GREEN),
                (true, true) => RichText::new("speech").color(GREEN),
                _ => RichText::new("silence").weak(),
            });
        });

        self.mic_test_ui(ui);

        ui.add_space(4.0);
        device_line(ui, "🎤", &st.input);
        device_line(ui, "🔊", &st.output);

        ui.add_space(2.0);
        ui.toggle_value(&mut self.show_settings, "⚙ Settings");
        if self.show_settings {
            self.settings_ui(ui, st);
        }
    }

    fn mic_test_ui(&mut self, ui: &mut egui::Ui) {
        let mut state = *self.shared.mic_test.lock();
        if let MicTest::Playing { until, sent_pct } = state {
            if Instant::now() >= until {
                state = MicTest::Done { sent_pct };
                *self.shared.mic_test.lock() = state;
            }
        }
        let mut next = None;
        ui.horizontal(|ui| match state {
            MicTest::Idle | MicTest::Done { .. } => {
                if ui
                    .button("🎙 Mic test")
                    .on_hover_text(
                        "Records 5 s and plays it back — you hear exactly what others hear \
                         (after noise suppression). Nobody hears you meanwhile.",
                    )
                    .clicked()
                {
                    next = Some(MicTest::Recording { started: Instant::now() });
                }
                if let MicTest::Done { sent_pct } = state {
                    if sent_pct == 0 {
                        ui.colored_label(AMBER, "Nothing was transmitted — try the \"High\" sensitivity.");
                    } else {
                        ui.label(RichText::new(format!("Transmitted: {sent_pct}% of the recording")).weak());
                    }
                }
            }
            MicTest::Recording { started } => {
                let left = MIC_TEST_LEN.saturating_sub(started.elapsed()).as_secs_f32();
                ui.label(RichText::new(format!("● Speak now… {left:.0} s")).color(RED));
                if ui.button("Cancel").clicked() {
                    next = Some(MicTest::Idle);
                }
            }
            MicTest::Playing { .. } => {
                ui.label(RichText::new("▶ This is how you sound…").color(GREEN));
                if ui.button("Stop").clicked() {
                    next = Some(MicTest::Idle);
                }
            }
        });
        if let Some(n) = next {
            *self.shared.mic_test.lock() = n;
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, st: &Status) {
        let shared = self.shared.clone();
        egui::Grid::new("settings").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Microphone");
            let mut choice = shared.input_choice.lock().clone();
            if device_combo(ui, "in", &mut choice, &st.input_devices) {
                *shared.input_choice.lock() = choice.clone();
                shared.restart_audio();
                self.edit_config(|c| c.input_device = choice);
            }
            ui.end_row();
            ui.label("Speakers");
            let mut choice = shared.output_choice.lock().clone();
            if device_combo(ui, "out", &mut choice, &st.output_devices) {
                *shared.output_choice.lock() = choice.clone();
                shared.restart_audio();
                self.edit_config(|c| c.output_device = choice);
            }
            ui.end_row();
            ui.label("Master volume");
            let mut vol = shared.master_volume.get();
            if ui.add(egui::Slider::new(&mut vol, 0.0..=2.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0))).changed() {
                shared.master_volume.set(vol);
                self.edit_config(|c| c.master_volume = vol);
            }
            ui.end_row();
        });
        let mut dn = shared.denoise.load(Ordering::Relaxed);
        let resp = ui.add_enabled(st.denoise_available, egui::Checkbox::new(&mut dn, "Noise suppression (recommended)"));
        if resp.changed() {
            shared.denoise.store(dn, Ordering::Relaxed);
            self.edit_config(|c| c.denoise = dn);
        }
        if !st.denoise_available {
            ui.colored_label(AMBER, "Noise suppression failed to start on this computer.");
        }
        let mut cs = shared.chat_sound.load(Ordering::Relaxed);
        if ui
            .checkbox(&mut cs, "Sound on new chat message")
            .on_hover_text("Only while the Backroom window is in the background")
            .changed()
        {
            shared.chat_sound.store(cs, Ordering::Relaxed);
            self.edit_config(|c| c.chat_sound = cs);
        }
        ui.horizontal(|ui| {
            if ui.button("Restart audio").on_hover_text("Reopens the microphone and speakers").clicked() {
                shared.restart_audio();
            }
            if ui.button("Show log").on_hover_text("The file to send when reporting a problem").clicked() {
                open_path(&crate::config::dir());
            }
        });
    }
}

fn device_line(ui: &mut egui::Ui, icon: &str, d: &DeviceStatus) {
    ui.horizontal(|ui| {
        ui.label(icon);
        let name = if d.name.is_empty() { "—" } else { d.name.as_str() };
        let (txt, color) = if d.ok && d.message.is_empty() {
            (format!("{name}: OK"), ui.visuals().weak_text_color())
        } else if d.ok {
            (format!("{name}: {}", d.message), AMBER)
        } else {
            (format!("{name}: {}", if d.message.is_empty() { "opening…" } else { &d.message }), RED)
        };
        let r = ui.label(RichText::new(txt).small().color(color));
        if let Some((t, reason)) = &d.last_restart {
            r.on_hover_text(format!(
                "Fixed automatically {}× — last time {} s ago: {reason}",
                d.restarts,
                t.elapsed().as_secs()
            ));
        }
    });
}

fn device_combo(ui: &mut egui::Ui, id: &str, choice: &mut Option<String>, list: &[String]) -> bool {
    let before = choice.clone();
    let text = choice.clone().unwrap_or_else(|| "System default".into());
    egui::ComboBox::from_id_salt(id).selected_text(text).width(220.0).show_ui(ui, |ui| {
        ui.selectable_value(choice, None, "System default");
        for name in list {
            ui.selectable_value(choice, Some(name.clone()), name);
        }
    });
    *choice != before
}

/// Server time (Unix seconds) as local HH:MM.
fn clock(unix: u64) -> String {
    chrono::DateTime::from_timestamp(unix as i64, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Someone's last chat message, if it is from the last few minutes.
fn recent_message<'a>(chat: &'a VecDeque<ChatLine>, name: &str) -> Option<&'a str> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    chat.iter()
        .rev()
        .find(|l| l.name == name)
        .filter(|l| now.saturating_sub(l.time) < RECENT_MESSAGE.as_secs())
        .map(|l| l.text.as_str())
}

fn short_fp(fp: &str) -> String {
    fp.split(':').take(8).collect::<Vec<_>>().join(":") + "…"
}

/// Fingerprint key in the config — same format as in `net::run_once`.
fn cert_key(server: &str) -> String {
    let (h, p) = crate::net::parse_addr(server);
    format!("{h}:{p}")
}

fn open_path(p: &std::path::Path) {
    let cmd = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd).arg(p).spawn();
}

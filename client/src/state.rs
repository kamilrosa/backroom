//! State shared between threads: audio, network, hotkeys and UI.
//!
//! Settings read in the audio loop are atomics (lock-free); display state
//! sits behind a mutex and the UI copies it once per frame.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use backroom_proto::{ChatLine, UserId, UserInfo};
use parking_lot::Mutex;

/// Length of the mic test recording.
pub const MIC_TEST_LEN: Duration = Duration::from_secs(5);

/// Mic test: record a few seconds of what others would hear, then play it
/// back. Not live — live monitoring sounds like an echo, and on speakers it
/// creates a feedback loop. Nothing is sent to others during the test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MicTest {
    Idle,
    Recording { started: Instant },
    Playing { until: Instant, sent_pct: u8 },
    Done { sent_pct: u8 },
}

impl MicTest {
    /// Whether the test is using the microphone (nothing is sent to the server then).
    pub fn active(&self) -> bool {
        matches!(self, MicTest::Recording { .. } | MicTest::Playing { .. })
    }
}

#[derive(Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeviceStatus {
    pub name: String,
    pub ok: bool,
    pub message: String,
    pub restarts: u32,
    pub last_restart: Option<(Instant, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected { server_name: String },
    /// Connection dropped — the client is reconnecting by itself.
    Reconnecting { reason: String },
    /// Rejected by the server (password, version…) — no retries.
    Failed { reason: String },
    /// Certificate fingerprint differs from the remembered one.
    CertChanged { expected: String, got: String },
}

#[derive(Debug, Clone)]
pub struct UserView {
    pub info: UserInfo,
    pub ping_ms: Option<u32>,
    pub last_voice: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub input: DeviceStatus,
    pub output: DeviceStatus,
    pub input_devices: Vec<String>,
    pub output_devices: Vec<String>,
    pub conn: ConnState,
    pub my_id: Option<UserId>,
    pub users: BTreeMap<UserId, UserView>,
    pub ptt_available: bool,
    pub ptt_message: String,
    pub denoise_available: bool,
    /// The server supports chat (older servers do not).
    pub chat_available: bool,
    pub chat: VecDeque<ChatLine>,
    /// Messages from others that arrived while the window was in the background.
    pub unread: u32,
}

impl Status {
    pub fn my_name(&self) -> Option<&str> {
        self.my_id.and_then(|id| self.users.get(&id)).map(|u| u.info.name.as_str())
    }
}

impl Default for Status {
    fn default() -> Self {
        Self {
            input: DeviceStatus::default(),
            output: DeviceStatus::default(),
            input_devices: vec![],
            output_devices: vec![],
            conn: ConnState::Disconnected,
            my_id: None,
            users: BTreeMap::new(),
            ptt_available: false,
            ptt_message: String::new(),
            denoise_available: true,
            chat_available: false,
            chat: VecDeque::new(),
            unread: 0,
        }
    }
}

pub struct Shared {
    // --- settings (UI → audio) ---
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
    pub away: AtomicBool,
    pub push_to_talk: AtomicBool,
    pub ptt_down: AtomicBool,
    pub vad_threshold: AtomicF32,
    pub denoise: AtomicBool,
    pub mic_test: Mutex<MicTest>,
    pub master_volume: AtomicF32,
    pub chat_sound: AtomicBool,
    /// The window has focus — new messages are not "unread" and make no sound.
    pub focused: AtomicBool,
    /// Typing in the chat box — push-to-talk is ignored meanwhile, in case
    /// the key is a letter.
    pub typing: AtomicBool,
    /// Play the new-message sound (network → playback thread).
    pub chime: AtomicBool,
    /// Per-person volume (local mute already applied).
    pub gains: Mutex<HashMap<UserId, f32>>,
    /// Selected devices + a change counter so the audio threads know they have
    /// to reopen their devices (new selection or a manual restart).
    pub input_choice: Mutex<Option<String>>,
    pub output_choice: Mutex<Option<String>>,
    pub device_gen: AtomicU64,

    // --- measurements (audio → UI) ---
    pub mic_level_db: AtomicF32,
    pub transmitting: AtomicBool,

    pub status: Mutex<Status>,
    repaint: Mutex<Option<eframe::egui::Context>>,
}

impl Shared {
    pub fn new(cfg: &crate::config::Config) -> Self {
        Self {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            away: AtomicBool::new(false),
            push_to_talk: AtomicBool::new(cfg.push_to_talk),
            ptt_down: AtomicBool::new(false),
            vad_threshold: AtomicF32::new(cfg.sensitivity.threshold_db()),
            denoise: AtomicBool::new(cfg.denoise),
            mic_test: Mutex::new(MicTest::Idle),
            master_volume: AtomicF32::new(cfg.master_volume),
            chat_sound: AtomicBool::new(cfg.chat_sound),
            focused: AtomicBool::new(true),
            typing: AtomicBool::new(false),
            chime: AtomicBool::new(false),
            gains: Mutex::new(HashMap::new()),
            input_choice: Mutex::new(cfg.input_device.clone()),
            output_choice: Mutex::new(cfg.output_device.clone()),
            device_gen: AtomicU64::new(0),
            mic_level_db: AtomicF32::new(-90.0),
            transmitting: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
            repaint: Mutex::new(None),
        }
    }

    /// Forces the microphone and speakers to be reopened.
    pub fn restart_audio(&self) {
        self.device_gen.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_repaint(&self, ctx: eframe::egui::Context) {
        *self.repaint.lock() = Some(ctx);
    }

    pub fn repaint(&self) {
        if let Some(ctx) = &*self.repaint.lock() {
            ctx.request_repaint();
        }
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut Status) -> R) -> R {
        let r = f(&mut self.status.lock());
        self.repaint();
        r
    }
}

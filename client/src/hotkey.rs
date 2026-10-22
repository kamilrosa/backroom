//! Global push-to-talk: polls keyboard and mouse state every 10 ms.
//!
//! Polling (instead of registering a system hotkey) does not "swallow" the
//! key — the game still sees it, so PTT can live on e.g. a side mouse
//! button that the game uses too.
//!
//! macOS: needs permission in Settings → Privacy → Accessibility.
//! Linux: works under X11 (under Wayland only for XWayland apps).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use device_query::{DeviceQuery, DeviceState, Keycode};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::state::Shared;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binding {
    Key(String),
    Mouse(usize),
}

impl Binding {
    pub fn label(&self) -> String {
        match self {
            Binding::Key(k) => k.clone(),
            Binding::Mouse(3) => "Middle mouse button".into(),
            Binding::Mouse(n) => format!("Mouse button {n}"),
        }
    }
}

/// Shared with the UI: the current binding and the "press a key to assign" mode.
#[derive(Default)]
pub struct HotkeyState {
    pub binding: Mutex<Option<Binding>>,
    pub capturing: AtomicBool,
    pub captured: Mutex<Option<Binding>>,
}

pub fn spawn(shared: Arc<Shared>, hk: Arc<HotkeyState>) {
    std::thread::Builder::new()
        .name("backroom-ptt".into())
        .spawn(move || run(shared, hk))
        .expect("PTT thread");
}

fn pressed(keys: &[Keycode], mouse: &[bool], b: &Binding) -> bool {
    match b {
        Binding::Key(k) => keys.iter().any(|x| x.to_string() == *k),
        Binding::Mouse(n) => mouse.get(*n).copied().unwrap_or(false),
    }
}

fn run(shared: Arc<Shared>, hk: Arc<HotkeyState>) {
    let mut ds: Option<DeviceState> = None;
    let mut last_try = Instant::now() - Duration::from_secs(10);
    // When assigning, first wait until everything is released
    // (e.g. the click on the "Change" button).
    let mut armed = false;

    loop {
        std::thread::sleep(Duration::from_millis(10));
        // Without PTT and without assigning there is nothing to read.
        let need = shared.push_to_talk.load(Ordering::Relaxed) || hk.capturing.load(Ordering::Relaxed);
        if !need {
            shared.ptt_down.store(false, Ordering::Relaxed);
            continue;
        }
        if ds.is_none() {
            if last_try.elapsed() < Duration::from_secs(3) {
                continue;
            }
            last_try = Instant::now();
            ds = DeviceState::checked_new();
            let msg = if ds.is_some() {
                String::new()
            } else if cfg!(target_os = "macos") {
                "Push-to-talk needs permission: System Settings → Privacy & Security → Accessibility → Backroom".into()
            } else if cfg!(target_os = "linux") {
                "Push-to-talk only works in an X11 session".into()
            } else {
                "Could not read the keyboard".into()
            };
            shared.update(|s| {
                s.ptt_available = ds.is_some();
                s.ptt_message = msg;
            });
            continue;
        }
        let d = ds.as_ref().unwrap();
        let keys = d.get_keys();
        let mouse = d.get_mouse().button_pressed;

        if hk.capturing.load(Ordering::Relaxed) {
            let any_mouse = mouse.iter().any(|&b| b);
            if !armed {
                armed = keys.is_empty() && !any_mouse;
                continue;
            }
            let got = if keys.contains(&Keycode::Escape) {
                Some(None)
            } else if let Some(k) = keys.first() {
                Some(Some(Binding::Key(k.to_string())))
            } else {
                // Skip the left and right buttons — they are for clicking in the UI.
                mouse.iter().enumerate().skip(3).find(|(_, &b)| b).map(|(i, _)| Some(Binding::Mouse(i)))
            };
            if let Some(b) = got {
                *hk.captured.lock() = b.or_else(|| hk.binding.lock().clone());
                hk.capturing.store(false, Ordering::Relaxed);
                armed = false;
                shared.repaint();
            }
            continue;
        }
        armed = false;

        let down = hk.binding.lock().as_ref().is_some_and(|b| pressed(&keys, &mouse, b));
        shared.ptt_down.store(down, Ordering::Relaxed);
    }
}

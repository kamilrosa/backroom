//! Settings persisted between runs (JSON in the config directory).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hotkey::Binding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Sensitivity {
    Low,
    #[default]
    Normal,
    High,
}

impl Sensitivity {
    /// SNR threshold (dB) that opens the microphone. The model rates dog
    /// barking at about −15…−3 dB and speech at +20…+35 dB — hence the wide margin.
    pub fn threshold_db(self) -> f32 {
        match self {
            Sensitivity::Low => 15.0,
            Sensitivity::Normal => 8.0,
            Sensitivity::High => 3.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sensitivity::Low => "Low",
            Sensitivity::Normal => "Normal",
            Sensitivity::High => "High",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: String,
    pub name: String,
    pub password: String,
    pub auto_connect: bool,
    pub push_to_talk: bool,
    pub ptt_binding: Option<Binding>,
    pub sensitivity: Sensitivity,
    pub denoise: bool,
    /// `None` = the system default device (and following it when it changes).
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub master_volume: f32,
    /// Sound on a new chat message while the window is in the background.
    pub chat_sound: bool,
    /// Per-person volume (by nickname), 1.0 = 100%.
    pub volumes: BTreeMap<String, f32>,
    /// People muted locally (by nickname).
    pub local_mutes: BTreeSet<String>,
    /// Trusted server certificate fingerprints: address → SHA-256.
    pub fingerprints: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: String::new(),
            name: String::new(),
            password: String::new(),
            auto_connect: false,
            push_to_talk: false,
            ptt_binding: None,
            sensitivity: Sensitivity::Normal,
            denoise: true,
            input_device: None,
            output_device: None,
            master_volume: 1.0,
            chat_sound: false,
            volumes: BTreeMap::new(),
            local_mutes: BTreeSet::new(),
            fingerprints: BTreeMap::new(),
        }
    }
}

/// Config and log directory. `BACKROOM_CONFIG_DIR` lets a second instance
/// run with separate settings (tests, two accounts on one PC).
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("BACKROOM_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    directories::ProjectDirs::from("pl", "backroom", "Backroom")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn path() -> PathBuf {
    dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        match std::fs::read(path()) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!("Corrupted config, using defaults: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let p = path();
        let res = std::fs::create_dir_all(dir()).and_then(|_| {
            let tmp = p.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap())?;
            std::fs::rename(&tmp, &p)
        });
        if let Err(e) = res {
            tracing::warn!("Could not save config: {e}");
        }
    }

    pub fn volume_of(&self, name: &str) -> f32 {
        if self.local_mutes.contains(name) {
            0.0
        } else {
            self.volumes.get(name).copied().unwrap_or(1.0)
        }
    }
}

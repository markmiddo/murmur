//! Shared types for the Murmur daemon and panel applet.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "io.github.markmiddo.Murmur";
pub const DBUS_NAME: &str = "io.github.markmiddo.Murmur";
pub const DBUS_PATH: &str = "/io/github/markmiddo/Murmur";
pub const DBUS_IFACE: &str = "io.github.markmiddo.Murmur1";

/// What the engine is doing right now. Sent over D-Bus as a lowercase string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Starting,
    Downloading,
    Loading,
    Idle,
    Recording,
    Transcribing,
    Disabled,
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Downloading => "downloading",
            State::Loading => "loading",
            State::Idle => "idle",
            State::Recording => "recording",
            State::Transcribing => "transcribing",
            State::Disabled => "disabled",
            State::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "downloading" => State::Downloading,
            "loading" => State::Loading,
            "idle" => State::Idle,
            "recording" => State::Recording,
            "transcribing" => State::Transcribing,
            "disabled" => State::Disabled,
            "error" => State::Error,
            _ => State::Starting,
        }
    }
}

/// A speech model Murmur knows how to download and run.
pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub repo: &'static str,
    /// (file name, expected size in bytes)
    pub files: &'static [(&'static str, u64)],
}

impl ModelInfo {
    pub fn dir(&self) -> PathBuf {
        data_dir().join("models").join(self.id)
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|(_, s)| s).sum()
    }

    pub fn is_installed(&self) -> bool {
        let dir = self.dir();
        self.files.iter().all(|(name, size)| {
            std::fs::metadata(dir.join(name))
                .map(|m| m.len() == *size)
                .unwrap_or(false)
        })
    }
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "parakeet-tdt-0.6b-v2",
        name: "Parakeet v2 (English)",
        description: "Most accurate for English. Punctuation and capitals included.",
        repo: "istupakov/parakeet-tdt-0.6b-v2-onnx",
        files: &[
            ("encoder-model.int8.onnx", 652_184_014),
            ("decoder_joint-model.int8.onnx", 8_998_286),
            ("vocab.txt", 9_384),
            ("config.json", 97),
        ],
    },
    ModelInfo {
        id: "parakeet-tdt-0.6b-v3",
        name: "Parakeet v3 (25 languages)",
        description: "Multilingual with automatic language detection.",
        repo: "istupakov/parakeet-tdt-0.6b-v3-onnx",
        files: &[
            ("encoder-model.int8.onnx", 652_183_999),
            ("decoder_joint-model.int8.onnx", 18_202_004),
            ("vocab.txt", 93_939),
            ("config.json", 97),
        ],
    },
];

pub fn model_by_id(id: &str) -> &'static ModelInfo {
    MODELS.iter().find(|m| m.id == id).unwrap_or(&MODELS[0])
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("murmur")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("murmur")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Linux input key name held to dictate, e.g. "KEY_RIGHTALT".
    pub hotkey: String,
    /// Model id from [`MODELS`].
    pub model: String,
    /// Play short start/stop tones.
    pub sounds: bool,
    /// Append a space after each dictation so consecutive ones join up.
    pub trailing_space: bool,
    /// Cancel the recording if another key is pressed while the hotkey is held
    /// (so Right Alt still works as a normal modifier / AltGr).
    pub cancel_on_other_key: bool,
    /// Drop filler sounds like "um" and "mm-hmm".
    pub remove_fillers: bool,
    /// Keep recording this long after the key is released, in milliseconds.
    pub release_tail_ms: u64,
    /// Ignore presses shorter than this, in milliseconds.
    pub min_hold_ms: u64,
    /// Words the model tends to mishear. Matched case-insensitively on word boundaries.
    pub replacements: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "KEY_RIGHTALT".into(),
            model: MODELS[0].id.into(),
            sounds: true,
            trailing_space: true,
            cancel_on_other_key: true,
            remove_fillers: true,
            release_tail_ms: 200,
            min_hold_ms: 250,
            replacements: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load the config, falling back to defaults on any problem. Never fails.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(err) => {
                    tracing::warn!("Invalid {}: {err}. Using defaults.", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        std::fs::create_dir_all(config_dir())?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

/// Human label for an evdev key name, e.g. "KEY_RIGHTALT" -> "Right Alt".
pub fn hotkey_label(key: &str) -> String {
    let k = key.trim_start_matches("KEY_");
    let pretty = |side: &str, rest: &str| {
        let rest = match rest {
            "ALT" => "Alt",
            "CTRL" => "Ctrl",
            "SHIFT" => "Shift",
            "META" => "Super",
            other => other,
        };
        format!("{side} {rest}")
    };
    if let Some(rest) = k.strip_prefix("RIGHT") {
        pretty("Right", rest)
    } else if let Some(rest) = k.strip_prefix("LEFT") {
        pretty("Left", rest)
    } else {
        k.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_labels() {
        assert_eq!(hotkey_label("KEY_RIGHTALT"), "Right Alt");
        assert_eq!(hotkey_label("KEY_LEFTCTRL"), "Left Ctrl");
        assert_eq!(hotkey_label("KEY_F13"), "F13");
    }

    #[test]
    fn state_round_trip() {
        for s in [
            State::Starting,
            State::Downloading,
            State::Loading,
            State::Idle,
            State::Recording,
            State::Transcribing,
            State::Disabled,
            State::Error,
        ] {
            assert_eq!(State::parse(s.as_str()), s);
        }
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("sounds = false").unwrap();
        assert!(!cfg.sounds);
        assert_eq!(cfg.hotkey, "KEY_RIGHTALT");
    }
}

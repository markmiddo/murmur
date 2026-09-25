//! Turning a transcript into keystrokes.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};
use regex::{Regex, RegexBuilder};

/// Case-insensitive whole-word replacements, longest phrase first.
pub struct Replacer {
    rules: Vec<(Regex, String)>,
}

impl Replacer {
    pub fn new(map: &BTreeMap<String, String>) -> Self {
        let mut pairs: Vec<_> = map.iter().collect();
        pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        let rules = pairs
            .into_iter()
            .filter_map(|(from, to)| {
                let pattern = format!(r"\b{}\b", regex::escape(from.trim()));
                RegexBuilder::new(&pattern)
                    .case_insensitive(true)
                    .build()
                    .ok()
                    .map(|re| (re, to.clone()))
            })
            .collect();
        Self { rules }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (re, to) in &self.rules {
            out = re.replace_all(&out, regex::NoExpand(to)).into_owned();
        }
        out
    }
}

const FILLERS: &[&str] = &[
    "um", "umm", "uh", "uhh", "er", "erm", "hmm", "hm", "mm", "mmm", "mhm", "mm-hmm", "uh-huh",
    "ah", "ahh",
];

/// Drop filler sounds ("um", "mm-hmm") while keeping sentence punctuation.
pub fn strip_fillers(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let core = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
        if FILLERS.contains(&core.to_lowercase().as_str()) {
            // Keep a sentence end ("Mm-hmm." after "sounds good") on the previous word.
            let end: String = word
                .chars()
                .rev()
                .take_while(|c| ".?!".contains(*c))
                .collect();
            if let Some(prev) = out.last_mut()
                && !end.is_empty()
            {
                let trimmed = prev
                    .trim_end_matches(|c: char| ",;:".contains(c))
                    .to_string();
                *prev = if trimmed.ends_with(['.', '?', '!']) {
                    trimmed
                } else {
                    trimmed + &end.chars().rev().collect::<String>()
                };
            }
            continue;
        }
        out.push(word.to_string());
    }
    let mut joined = out.join(" ");
    // A removed leading filler may leave "so, ..." lowercase or a stray comma.
    joined = joined.trim_start_matches([',', ' ']).to_string();
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) if text.chars().next().is_some_and(char::is_uppercase) => {
            first.to_uppercase().collect::<String>() + chars.as_str()
        }
        _ => joined,
    }
}

/// Type text into the focused window via the Wayland virtual-keyboard
/// protocol (wtype). Falls back to the clipboard if typing is unavailable.
pub fn type_text(text: &str) -> Result<Delivery> {
    let mut cmd = Command::new("wtype");
    cmd.arg("--").arg(text);
    if let Some(display) = wayland_display() {
        cmd.env("WAYLAND_DISPLAY", display);
    }
    match cmd.status() {
        Ok(s) if s.success() => return Ok(Delivery::Typed),
        Ok(s) => tracing::warn!("wtype exited with {s}"),
        Err(err) => tracing::warn!("wtype not available: {err}"),
    }
    copy_to_clipboard(text)?;
    Ok(Delivery::Clipboard)
}

/// The session's Wayland socket. If we were started before the session
/// exported WAYLAND_DISPLAY (early at login), find the socket ourselves.
fn wayland_display() -> Option<String> {
    if let Ok(d) = std::env::var("WAYLAND_DISPLAY")
        && !d.is_empty()
    {
        return Some(d);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let mut sockets: Vec<String> = std::fs::read_dir(runtime)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
        .collect();
    sockets.sort();
    sockets.into_iter().next()
}

pub enum Delivery {
    Typed,
    Clipboard,
}

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut cmd = Command::new("wl-copy");
    if let Some(display) = wayland_display() {
        cmd.env("WAYLAND_DISPLAY", display);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("wl-copy not available: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no stdin"))?
        .write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

/// Desktop notification over D-Bus (works inside a Flatpak sandbox).
pub async fn notify(conn: &zbus::Connection, summary: &str, body: &str) {
    let hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
        std::collections::HashMap::new();
    let result = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Murmur",
                0u32,
                "io.github.markmiddo.Murmur",
                summary,
                body,
                Vec::<&str>::new(),
                hints,
                5000i32,
            ),
        )
        .await;
    if let Err(err) = result {
        tracing::debug!("notification failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacer(pairs: &[(&str, &str)]) -> Replacer {
        Replacer::new(
            &pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        )
    }

    #[test]
    fn replaces_whole_words_case_insensitively() {
        let r = replacer(&[("java script", "JavaScript"), ("open ai", "OpenAI")]);
        assert_eq!(
            r.apply("Ask Open AI about Java Script."),
            "Ask OpenAI about JavaScript."
        );
    }

    #[test]
    fn does_not_touch_partial_words() {
        let r = replacer(&[("can", "CAN")]);
        assert_eq!(r.apply("scanner can"), "scanner CAN");
    }

    #[test]
    fn longest_phrase_wins() {
        let r = replacer(&[("script", "Script"), ("java script", "JavaScript")]);
        assert_eq!(r.apply("java script and script"), "JavaScript and Script");
    }

    #[test]
    fn strips_fillers() {
        assert_eq!(strip_fillers("Mm-hmm."), "");
        assert_eq!(
            strip_fillers("Um, so we ship Friday."),
            "So we ship Friday."
        );
        assert_eq!(strip_fillers("That works, mm-hmm."), "That works.");
        assert_eq!(
            strip_fillers("I think, uh, it's fine"),
            "I think, it's fine"
        );
        assert_eq!(
            strip_fillers("Yeah, humming along."),
            "Yeah, humming along."
        );
    }

    #[test]
    fn dollar_signs_are_literal() {
        let r = replacer(&[("price", "$5")]);
        assert_eq!(r.apply("the price"), "the $5");
    }
}

//! Microphone capture and feedback tones through PipeWire.
//!
//! Capture runs `pw-record` as a child process for each dictation and reads
//! raw 16 kHz mono floats from its stdout. PipeWire does the resampling and
//! device routing, and a fresh process per dictation means a stalled or
//! unplugged device can never wedge the next one. (Going through the ALSA
//! compatibility layer instead intermittently stalled after ~100 ms.)

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;

pub const TARGET_RATE: u32 = 16_000;
const TONE_RATE: u32 = 48_000;

#[derive(Clone, Copy, Debug)]
pub enum Tone {
    Start,
    Stop,
    Cancel,
    Error,
}

struct Recording {
    child: Child,
    reader: JoinHandle<()>,
    buf: Arc<Mutex<Vec<f32>>>,
}

#[derive(Clone)]
pub struct Audio {
    events: UnboundedSender<Event>,
    current: Arc<Mutex<Option<Recording>>>,
}

impl Audio {
    pub fn new(events: UnboundedSender<Event>) -> Self {
        Self {
            events,
            current: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start(&self) -> Result<()> {
        // Never leave an old recorder running.
        let _ = self.stop();

        let mut child = Command::new("pw-record")
            .args([
                "--raw",
                "--rate",
                "16000",
                "--channels",
                "1",
                "--format",
                "f32",
                "--latency",
                "20ms",
                "--media-role",
                "Communication",
                "-P",
                "node.name=murmur-capture",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not start pw-record (is PipeWire installed?)")?;
        let mut stdout = child.stdout.take().context("pw-record has no stdout")?;

        let buf = Arc::new(Mutex::new(Vec::<f32>::with_capacity(
            TARGET_RATE as usize * 30,
        )));
        let b = buf.clone();
        let mut meter = LevelMeter::new(self.events.clone());
        let reader = std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || {
                let mut bytes = [0u8; 4096];
                let mut carry: Vec<u8> = Vec::with_capacity(4);
                loop {
                    let n = match stdout.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    carry.extend_from_slice(&bytes[..n]);
                    let whole = carry.len() / 4 * 4;
                    let samples: Vec<f32> = carry[..whole]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    carry.drain(..whole);
                    meter.feed(&samples);
                    b.lock().unwrap().extend_from_slice(&samples);
                }
            })
            .context("spawn capture reader")?;

        *self.current.lock().unwrap() = Some(Recording { child, reader, buf });
        tracing::info!("Recording");
        Ok(())
    }

    /// Stop recording and return mono 16 kHz samples.
    pub fn stop(&self) -> Vec<f32> {
        let Some(mut rec) = self.current.lock().unwrap().take() else {
            return Vec::new();
        };
        let _ = rec.child.kill();
        let _ = rec.child.wait();
        let _ = rec.reader.join();
        std::mem::take(&mut *rec.buf.lock().unwrap())
    }

    pub fn tone(&self, tone: Tone) {
        std::thread::spawn(move || {
            if let Err(err) = play_tone(tone) {
                tracing::debug!("tone failed: {err:#}");
            }
        });
    }
}

/// Sends a smoothed input level (0..1) about 20 times a second.
struct LevelMeter {
    events: UnboundedSender<Event>,
    acc: f32,
    count: usize,
    smooth: f32,
}

impl LevelMeter {
    const WINDOW: usize = (TARGET_RATE / 20) as usize;

    fn new(events: UnboundedSender<Event>) -> Self {
        Self {
            events,
            acc: 0.0,
            count: 0,
            smooth: 0.0,
        }
    }

    fn feed(&mut self, samples: &[f32]) {
        for &s in samples {
            self.acc += s * s;
            self.count += 1;
            if self.count >= Self::WINDOW {
                let rms = (self.acc / self.count as f32).sqrt();
                // Map roughly -50 dBFS..-10 dBFS onto 0..1.
                let db = 20.0 * rms.max(1e-6).log10();
                let level = ((db + 50.0) / 40.0).clamp(0.0, 1.0);
                self.smooth = if level > self.smooth {
                    level
                } else {
                    self.smooth * 0.6 + level * 0.4
                };
                let _ = self.events.send(Event::Level(self.smooth));
                self.acc = 0.0;
                self.count = 0;
            }
        }
    }
}

/// Resample mono audio. Box-filters before decimating so speech-band
/// content survives without aliasing mush; plenty for ASR.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let width = ratio.round().max(1.0) as usize;
    let filtered: Vec<f32> = if width > 1 {
        let mut out = Vec::with_capacity(input.len());
        let mut sum = 0.0f32;
        for i in 0..input.len() {
            sum += input[i];
            if i >= width {
                sum -= input[i - width];
            }
            out.push(sum / width.min(i + 1) as f32);
        }
        out
    } else {
        input.to_vec()
    };
    let out_len = (input.len() as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = filtered[idx.min(filtered.len() - 1)];
            let b = filtered[(idx + 1).min(filtered.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

/// How long the start tone lasts; that much audio is blanked so the
/// model never hears the chime.
pub const START_TONE: Duration = Duration::from_millis(125);

fn tone_samples(tone: Tone) -> Vec<f32> {
    // (frequency Hz, duration ms) segments
    let notes: &[(f32, f32)] = match tone {
        Tone::Start => &[(784.0, 55.0), (1046.5, 70.0)],
        Tone::Stop => &[(1046.5, 55.0), (784.0, 70.0)],
        Tone::Cancel => &[(523.25, 60.0)],
        Tone::Error => &[(329.6, 90.0), (261.6, 140.0)],
    };
    let rate = TONE_RATE as f32;
    let mut samples = Vec::new();
    for &(freq, ms) in notes {
        let n = (rate * ms / 1000.0) as usize;
        let fade = (rate * 0.008) as usize;
        for i in 0..n {
            let env = (i.min(n - i) as f32 / fade as f32).min(1.0);
            let t = i as f32 / rate;
            samples.push((t * freq * std::f32::consts::TAU).sin() * 0.12 * env);
        }
    }
    // Trailing silence so the device doesn't cut the tail off.
    samples.extend(std::iter::repeat_n(0.0, TONE_RATE as usize / 20));
    samples
}

fn play_tone(tone: Tone) -> Result<()> {
    let mut child = Command::new("pw-play")
        .args([
            "--raw",
            "--rate",
            "48000",
            "--channels",
            "1",
            "--format",
            "f32",
            "--media-role",
            "Notification",
            "-P",
            "node.name=murmur-tone",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("pw-play not available")?;
    let bytes: Vec<u8> = tone_samples(tone)
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    let written = child.stdin.take().map(|mut stdin| stdin.write_all(&bytes));
    // Always reap, even if the write failed, so no zombies pile up.
    child.wait()?;
    if let Some(Err(err)) = written {
        return Err(err.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_length() {
        let input = vec![0.0; 48_000];
        assert_eq!(resample(&input, 48_000, 16_000).len(), 16_000);
        assert_eq!(resample(&input, 16_000, 16_000).len(), 48_000);
    }

    #[test]
    fn resample_keeps_dc() {
        let input = vec![0.5; 4410];
        let out = resample(&input, 44_100, 16_000);
        assert!(out.iter().skip(5).all(|&s| (s - 0.5).abs() < 1e-4));
    }

    #[test]
    fn tones_are_quiet_and_short() {
        let s = tone_samples(Tone::Start);
        assert!(s.iter().all(|x| x.abs() <= 0.12));
        assert!(s.len() < TONE_RATE as usize / 2);
    }
}

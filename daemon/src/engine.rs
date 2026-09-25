//! Speech-to-text engine thread: downloads the model if needed, keeps it
//! loaded, and transcribes one clip at a time in order.

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use murmur_common::ModelInfo;
use parakeet_rs::{ParakeetTDT, Transcriber};
use tokio::sync::mpsc::UnboundedSender;

use crate::Event;
use crate::audio::TARGET_RATE;

enum Cmd {
    Load(&'static ModelInfo, u64),
    Transcribe { seq: u64, samples: Vec<f32> },
}

#[derive(Clone)]
pub struct Engine {
    tx: Sender<Cmd>,
}

impl Engine {
    pub fn spawn(events: UnboundedSender<Event>) -> Self {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || run(rx, events))
            .expect("spawn engine thread");
        Self { tx }
    }

    pub fn load(&self, model: &'static ModelInfo, generation: u64) {
        let _ = self.tx.send(Cmd::Load(model, generation));
    }

    pub fn transcribe(&self, seq: u64, samples: Vec<f32>) {
        let _ = self.tx.send(Cmd::Transcribe { seq, samples });
    }
}

fn run(rx: Receiver<Cmd>, events: UnboundedSender<Event>) {
    let mut model: Option<ParakeetTDT> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Load(info, generation) => {
                model = None;
                match prepare(info, generation, &events) {
                    Ok(m) => {
                        model = Some(m);
                        let _ = events.send(Event::ModelReady(generation));
                    }
                    Err(err) => {
                        tracing::error!("Model failed: {err:#}");
                        let _ = events.send(Event::ModelFailed(generation, format!("{err:#}")));
                    }
                }
            }
            Cmd::Transcribe { seq, samples } => {
                let result = match model.as_mut() {
                    Some(m) => transcribe(m, samples),
                    None => Err(anyhow!("Model not loaded")),
                };
                let _ = events.send(Event::Transcribed {
                    seq,
                    result: result.map_err(|e| format!("{e:#}")),
                });
            }
        }
    }
}

fn prepare(
    info: &'static ModelInfo,
    generation: u64,
    events: &UnboundedSender<Event>,
) -> Result<ParakeetTDT> {
    if !info.is_installed() {
        download(info, generation, events)?;
    }
    let _ = events.send(Event::Loading(generation));
    let t = Instant::now();
    let mut m = match ParakeetTDT::from_pretrained(info.dir(), None) {
        Ok(m) => m,
        Err(err) => {
            // Right size but unloadable means corrupt: delete it so Retry
            // downloads a fresh copy instead of failing forever.
            tracing::warn!(
                "Removing unloadable model files in {}",
                info.dir().display()
            );
            for (name, _) in info.files {
                let _ = std::fs::remove_file(info.dir().join(name));
            }
            return Err(anyhow!("{err}"))
                .context("Could not load speech model (it will re-download on retry)");
        }
    };
    // Warm up so the first real dictation is as fast as the rest.
    let _ = m.transcribe_samples(vec![0.0; TARGET_RATE as usize], TARGET_RATE, 1, None);
    tracing::info!("Loaded {} in {:?}", info.name, t.elapsed());
    Ok(m)
}

fn transcribe(m: &mut ParakeetTDT, samples: Vec<f32>) -> Result<String> {
    let secs = samples.len() as f32 / TARGET_RATE as f32;
    let t = Instant::now();
    // Pad with a little silence; the model drops trailing words on tight clips.
    let mut padded = samples;
    padded.extend(std::iter::repeat_n(0.0, TARGET_RATE as usize / 4));
    let out = m
        .transcribe_samples(padded, TARGET_RATE, 1, None)
        .map_err(|e| anyhow!("{e}"))?;
    tracing::info!("Transcribed {secs:.1}s in {:?}", t.elapsed());
    Ok(out.text.trim().to_string())
}

/// One-shot transcription of a wav file with the configured model, for testing.
pub fn transcribe_file(path: &std::path::Path) -> Result<String> {
    let info = murmur_common::model_by_id(&murmur_common::Config::load().model);
    if !info.is_installed() {
        return Err(anyhow!(
            "{} is not downloaded yet; start murmurd first",
            info.name
        ));
    }
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()?
        }
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    };
    let ch = spec.channels.max(1) as usize;
    let mono: Vec<f32> = raw
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect();
    let samples = crate::audio::resample(&mono, spec.sample_rate, TARGET_RATE);
    let mut m = ParakeetTDT::from_pretrained(info.dir(), None).map_err(|e| anyhow!("{e}"))?;
    transcribe(&mut m, samples)
}

/// Bytes fetched per HTTP request. Each request has a body timeout, so a
/// stalled connection costs at most one chunk's timeout before a retry.
const CHUNK: u64 = 32 << 20;
const RETRIES: u32 = 6;

fn download(
    info: &'static ModelInfo,
    generation: u64,
    events: &UnboundedSender<Event>,
) -> Result<()> {
    let dir = info.dir();
    std::fs::create_dir_all(&dir)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(180)))
        .build()
        .into();
    let total = info.total_bytes();
    let mut done: u64 = 0;
    tracing::info!("Downloading {} ({} MB)", info.name, total / 1_000_000);
    let _ = events.send(Event::Downloading(generation, 0.0));
    let mut last_report = Instant::now();

    for &(name, size) in info.files {
        let path = dir.join(name);
        if std::fs::metadata(&path)
            .map(|m| m.len() == size)
            .unwrap_or(false)
        {
            done += size;
            continue;
        }
        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{name}",
            info.repo, info.revision
        );
        let part = path.with_extension("part");
        // Resume a previous partial download.
        let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if have > size {
            have = 0;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)?;
        file.set_len(have)?;

        let mut failures = 0;
        while have < size {
            let end = (have + CHUNK).min(size) - 1;
            let result = (|| -> Result<u64> {
                let resp = agent
                    .get(&url)
                    .header("Range", &format!("bytes={have}-{end}"))
                    .call()?;
                let mut reader = resp.into_body().into_reader();
                let mut buf = vec![0u8; 1 << 16];
                let mut got = 0u64;
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    file.write_all(&buf[..n])?;
                    got += n as u64;
                    if last_report.elapsed().as_millis() > 250 {
                        last_report = Instant::now();
                        let p = (done + have + got) as f64 / total as f64;
                        let _ = events.send(Event::Downloading(generation, p));
                    }
                }
                Ok(got)
            })();
            match result {
                Ok(got) if got == end - have + 1 => {
                    have += got;
                    failures = 0;
                }
                other => {
                    // Keep whatever arrived; the next attempt resumes from there.
                    file.flush()?;
                    have = std::fs::metadata(&part)?.len();
                    failures += 1;
                    let why = match other {
                        Ok(got) => format!("short read ({got} bytes)"),
                        Err(e) => format!("{e:#}"),
                    };
                    tracing::warn!("Download of {name} interrupted: {why}");
                    if failures >= RETRIES {
                        return Err(anyhow!("Download failed: {why}"));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2u64.pow(failures)));
                }
            }
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&part, &path)?;
        done += size;
    }
    let _ = events.send(Event::Downloading(generation, 1.0));
    Ok(())
}

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
    Load(&'static ModelInfo),
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

    pub fn load(&self, model: &'static ModelInfo) {
        let _ = self.tx.send(Cmd::Load(model));
    }

    pub fn transcribe(&self, seq: u64, samples: Vec<f32>) {
        let _ = self.tx.send(Cmd::Transcribe { seq, samples });
    }
}

fn run(rx: Receiver<Cmd>, events: UnboundedSender<Event>) {
    let mut model: Option<ParakeetTDT> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Load(info) => {
                model = None;
                match prepare(info, &events) {
                    Ok(m) => {
                        model = Some(m);
                        let _ = events.send(Event::ModelReady);
                    }
                    Err(err) => {
                        tracing::error!("Model failed: {err:#}");
                        let _ = events.send(Event::ModelFailed(format!("{err:#}")));
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

fn prepare(info: &'static ModelInfo, events: &UnboundedSender<Event>) -> Result<ParakeetTDT> {
    if !info.is_installed() {
        download(info, events)?;
    }
    let _ = events.send(Event::Loading);
    let t = Instant::now();
    let mut m = ParakeetTDT::from_pretrained(info.dir(), None)
        .map_err(|e| anyhow!("{e}"))
        .context("Could not load speech model")?;
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

fn download(info: &'static ModelInfo, events: &UnboundedSender<Event>) -> Result<()> {
    let dir = info.dir();
    std::fs::create_dir_all(&dir)?;
    let total = info.total_bytes();
    let mut done: u64 = 0;
    tracing::info!("Downloading {} ({} MB)", info.name, total / 1_000_000);
    let _ = events.send(Event::Downloading(0.0));

    for &(name, size) in info.files {
        let path = dir.join(name);
        if std::fs::metadata(&path)
            .map(|m| m.len() == size)
            .unwrap_or(false)
        {
            done += size;
            continue;
        }
        let url = format!("https://huggingface.co/{}/resolve/main/{name}", info.repo);
        let resp = ureq::get(&url)
            .call()
            .with_context(|| format!("Download failed: {name}"))?;
        let mut reader = resp.into_body().into_reader();
        let part = path.with_extension("part");
        let mut file = std::fs::File::create(&part)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut written: u64 = 0;
        let mut last_report = Instant::now();
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            written += n as u64;
            if last_report.elapsed().as_millis() > 250 {
                last_report = Instant::now();
                let _ = events.send(Event::Downloading((done + written) as f64 / total as f64));
            }
        }
        file.sync_all()?;
        if written != size {
            let _ = std::fs::remove_file(&part);
            return Err(anyhow!("{name} is {written} bytes, expected {size}"));
        }
        std::fs::rename(&part, &path)?;
        done += size;
    }
    let _ = events.send(Event::Downloading(1.0));
    Ok(())
}

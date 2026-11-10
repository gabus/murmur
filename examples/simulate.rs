//! Dev tool: stream WAV files through the real re-check engine (Parakeet +
//! Silero) as if they came from the mic, and report what would be typed and when.
//!
//! cargo run --release --example simulate -- [--gap secs] [--min-age s] [--interval s] a.wav [b.wav ...]
//! Files are concatenated with `--gap` seconds of silence between them.

use std::time::{Duration, Instant};

use murmur::{
    asr::{
        engine::{EngineConfig, RecheckEngine, SAMPLE_RATE, Transcriber},
        moonshine::Moonshine,
        parakeet::Parakeet,
    },
    models::{self, Kind},
    paths,
    vad::SileroVad,
};

fn transcriber(id: &str, threads: i32) -> anyhow::Result<Box<dyn Transcriber + Send>> {
    let m = models::find(id).ok_or_else(|| anyhow::anyhow!("unknown model {id}"))?;
    let dir = m.dir(&paths::models_dir().ok_or_else(|| anyhow::anyhow!("no data dir"))?);
    Ok(match m.kind {
        Kind::Parakeet => Box::new(Parakeet::load(&dir, threads)?),
        Kind::Moonshine => Box::new(Moonshine::load(&dir, threads)?),
    })
}

fn load_wav(path: &str) -> anyhow::Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    let ch = usize::from(spec.channels);
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i32>()
            .map(|s| s.map(|v| v as f32 / (1i64 << (spec.bits_per_sample - 1)) as f32))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    };
    let mono: Vec<f32> = raw
        .chunks(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect();
    if spec.sample_rate as usize == SAMPLE_RATE {
        return Ok(mono);
    }
    let rs = sherpa_onnx::LinearResampler::create(spec.sample_rate as i32, SAMPLE_RATE as i32)
        .ok_or_else(|| anyhow::anyhow!("resampler"))?;
    Ok(rs.resample(&mono, true))
}

/// Wraps a transcriber to time every decode.
struct Timed<T> {
    inner: T,
    runs: Vec<(f64, Duration)>,
    dump: bool,
}

impl<T: Transcriber> Transcriber for Timed<T> {
    fn has_timestamps(&self) -> bool {
        self.inner.has_timestamps()
    }

    fn transcribe(
        &mut self,
        samples: &[f32],
    ) -> Result<Vec<murmur::asr::words::Word>, murmur::asr::AsrError> {
        let t = Instant::now();
        let r = self.inner.transcribe(samples);
        if self.dump
            && let Ok(words) = &r
        {
            let line: Vec<String> = words
                .iter()
                .map(|w| format!("{}[{:.2}-{:.2}]", w.text, w.start, w.end))
                .collect();
            println!(
                "    hyp (buf {:.2}s): {}",
                samples.len() as f64 / SAMPLE_RATE as f64,
                line.join(" ")
            );
        }
        self.runs
            .push((samples.len() as f64 / SAMPLE_RATE as f64, t.elapsed()));
        r
    }
}

fn main() -> anyhow::Result<()> {
    let mut gap = 1.5f64;
    let mut agc_on = false;
    let mut dump = false;
    let mut model = models::DEFAULT_MODEL.to_owned();
    let mut cfg = EngineConfig::default();
    let mut threads = 4;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--agc" => agc_on = true,
            "--dump" => dump = true,
            "--model" => model = args.next().unwrap_or(model),
            "--gap" => gap = args.next().and_then(|s| s.parse().ok()).unwrap_or(gap),
            "--min-age" => {
                cfg.min_age = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(cfg.min_age)
            }
            "--interval" => {
                cfg.rerun_interval = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(cfg.rerun_interval)
            }
            "--threads" => threads = args.next().and_then(|s| s.parse().ok()).unwrap_or(threads),
            _ => files.push(a),
        }
    }
    let models = paths::models_dir().ok_or_else(|| anyhow::anyhow!("no XDG data dir"))?;
    let mut audio = vec![0.0f32; (0.5 * SAMPLE_RATE as f64) as usize];
    for f in &files {
        audio.extend(load_wav(f)?);
        audio.extend(std::iter::repeat_n(
            0.0,
            (gap * SAMPLE_RATE as f64) as usize,
        ));
    }

    let t = Instant::now();
    let parakeet = transcriber(&model, threads)?;
    let vad = SileroVad::load(&models.join("silero_vad.onnx"), 0.8)?;
    println!(
        "load {:?}; audio {:.1}s; {cfg:?}",
        t.elapsed(),
        audio.len() as f64 / SAMPLE_RATE as f64
    );

    let mut engine = RecheckEngine::new(
        cfg,
        Timed {
            inner: parakeet,
            runs: Vec::new(),
            dump,
        },
        vad,
    );
    let mut typed = String::new();
    let wall = Instant::now();
    let mut agc = murmur::agc::Agc::new(SAMPLE_RATE as u32);
    for chunk in audio.chunks(320) {
        let mut chunk = chunk.to_vec();
        if agc_on {
            agc.process(&mut chunk);
        }
        for w in engine.push(&chunk)? {
            println!(
                "  {:>6.2}s  {:<16} (word end {:.2}s, lag {:.2}s)",
                engine.now(),
                w.text,
                w.end,
                engine.now() - w.end
            );
            typed.push_str(&w.text);
            typed.push(' ');
        }
    }
    for w in engine.finish()? {
        println!("  {:>6.2}s  {:<16} (final)", engine.now(), w.text);
        typed.push_str(&w.text);
        typed.push(' ');
    }
    let cpu = wall.elapsed();
    let s = engine.stats().clone();
    println!("\nTYPED: {typed}");
    println!(
        "words {} | avg audio lag {:.2}s | max {:.2}s | decodes {} | decode wall total {:?} for {:.1}s audio ({:.0}% of real time)",
        s.committed_words,
        s.total_commit_lag / f64::from(s.committed_words.max(1)),
        s.max_commit_lag,
        s.decodes,
        cpu,
        audio.len() as f64 / SAMPLE_RATE as f64,
        100.0 * cpu.as_secs_f64() / (audio.len() as f64 / SAMPLE_RATE as f64)
    );

    // Reference: whole-file transcription of each input on its own.
    let mut whole = transcriber(&model, threads)?;
    let mut reference = String::new();
    for f in &files {
        for w in whole.transcribe(&load_wav(f)?)? {
            reference.push_str(&w.text);
            reference.push(' ');
        }
    }
    println!("WHOLE: {reference}");
    println!("match: {}", typed.trim() == reference.trim());
    Ok(())
}

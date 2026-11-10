//! Dev probe: load Parakeet, decode a WAV at several prefix lengths, print tokens,
//! timestamps and timings. `cargo run --release --example asr_probe -- <model-dir> <wav>`

use std::time::Instant;

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).map(String::as_str).unwrap_or(".");
    let wav = args.get(2).map(String::as_str).unwrap_or("test.wav");
    let threads: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);

    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(format!("{dir}/encoder.int8.onnx"));
    config.model_config.transducer.decoder = Some(format!("{dir}/decoder.int8.onnx"));
    config.model_config.transducer.joiner = Some(format!("{dir}/joiner.int8.onnx"));
    config.model_config.tokens = Some(format!("{dir}/tokens.txt"));
    config.model_config.model_type = Some("nemo_transducer".into());
    config.model_config.num_threads = threads;
    config.decoding_method = Some("greedy_search".into());

    let t = Instant::now();
    let rec = OfflineRecognizer::create(&config).ok_or_else(|| anyhow::anyhow!("create failed"))?;
    println!("load: {:?} (threads={threads})", t.elapsed());

    let mut reader = hound::WavReader::open(wav)?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
    };
    println!(
        "wav: {} Hz, {} ch, {:.2}s",
        spec.sample_rate,
        spec.channels,
        samples.len() as f32 / spec.sample_rate as f32
    );

    for frac in [0.25f32, 0.5, 0.75, 1.0] {
        let n = (samples.len() as f32 * frac) as usize;
        let t = Instant::now();
        let stream = rec.create_stream();
        stream.accept_waveform(spec.sample_rate as i32, &samples[..n]);
        rec.decode(&stream);
        let el = t.elapsed();
        let r = stream
            .get_result()
            .ok_or_else(|| anyhow::anyhow!("no result"))?;
        println!(
            "\n{:.2}s audio → decode {:?} (RTF {:.3}): {}",
            n as f32 / 16000.0,
            el,
            el.as_secs_f32() / (n as f32 / 16000.0),
            r.text
        );
        if frac == 1.0 {
            let ts = r.timestamps.unwrap_or_default();
            let du = r.durations.unwrap_or_default();
            for (i, tok) in r.tokens.iter().enumerate().take(40) {
                println!(
                    "  {:?} @{:.2} +{:.2}",
                    tok,
                    ts.get(i).copied().unwrap_or(-1.0),
                    du.get(i).copied().unwrap_or(-1.0)
                );
            }
        }
    }
    Ok(())
}

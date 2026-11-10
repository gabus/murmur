//! Dev probe: sherpa online recognizer on a WAV, fed whole vs. in 20 ms chunks.
use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (dir, wav) = (&a[1], &a[2]);
    let pick = |part: &str, int8: bool| -> anyhow::Result<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(part) && n.ends_with(".onnx"))
            .collect();
        names.sort_by_key(|n| n.contains(".int8.") != int8);
        names
            .first()
            .map(|n| format!("{dir}/{n}"))
            .ok_or_else(|| anyhow::anyhow!("no {part}"))
    };
    let mut c = OnlineRecognizerConfig::default();
    c.model_config.transducer.encoder = Some(pick("encoder", true)?);
    c.model_config.transducer.decoder = Some(pick("decoder", false)?);
    c.model_config.transducer.joiner = Some(pick("joiner", true)?);
    c.model_config.tokens = Some(format!("{dir}/tokens.txt"));
    c.model_config.num_threads = 2;
    c.decoding_method = Some("greedy_search".into());
    let r = OnlineRecognizer::create(&c).ok_or_else(|| anyhow::anyhow!("create"))?;
    let mut rd = hound::WavReader::open(wav)?;
    let spec = rd.spec();
    let s: Vec<f32> = rd
        .samples::<i16>()
        .map(|x| x.map(|v| v as f32 / 32768.0))
        .collect::<Result<_, _>>()?;
    println!(
        "{} Hz {} ch {:.2}s",
        spec.sample_rate,
        spec.channels,
        s.len() as f32 / spec.sample_rate as f32
    );
    for chunked in [false, true] {
        let st = r.create_stream();
        if chunked {
            let n = spec.sample_rate as usize / 50;
            for (i, ch) in s.chunks(n).enumerate() {
                st.accept_waveform(spec.sample_rate as i32, ch);
                while r.is_ready(&st) {
                    r.decode(&st);
                }
                if i % 25 == 0 {
                    println!(
                        "  {:.2}s: {:?}",
                        (i * n) as f32 / spec.sample_rate as f32,
                        r.get_result(&st).map(|x| x.text)
                    );
                }
            }
        } else {
            st.accept_waveform(spec.sample_rate as i32, &s);
        }
        st.input_finished();
        while r.is_ready(&st) {
            r.decode(&st);
        }
        println!("chunked={chunked}: {:?}", r.get_result(&st).map(|x| x.text));
    }
    Ok(())
}

//! `cargo run -p stems-sep --features onnx --example align_smoke -- <model.onnx> [wav16k_mono.wav "words to align"]`
//! Loads the aligner, runs it on a WAV (or noise) and prints the spans.
use stems_sep::align::{Aligner, to_16k};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("debug").init();
    let args: Vec<String> = std::env::args().collect();
    let mut al = Aligner::load(std::path::Path::new(&args[1]), 4)?;
    let (audio, words): (Vec<f32>, Vec<String>) = match (args.get(2), args.get(3)) {
        (Some(wav), Some(text)) => {
            let mut r = hound::WavReader::open(wav)?;
            let spec = r.spec();
            let scale = 1.0 / i16::MAX as f32;
            let ch = spec.channels as usize;
            let mono: Vec<f32> = r
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 * scale)
                .collect::<Vec<_>>()
                .chunks(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32)
                .collect();
            let audio = if spec.sample_rate == 16_000 {
                mono
            } else {
                to_16k(&mono)
            };
            (audio, text.split_whitespace().map(String::from).collect())
        }
        _ => (
            (0..48_000)
                .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
                .collect(),
            vec!["hello".into(), "world".into()],
        ),
    };
    let t = std::time::Instant::now();
    let spans = al.align(&audio, &words)?;
    println!(
        "{:.2}s for {:.1}s audio",
        t.elapsed().as_secs_f32(),
        audio.len() as f32 / 16_000.0
    );
    match spans {
        Some(s) => {
            for (w, sp) in words.iter().zip(s) {
                println!(
                    "{:>6} - {:>6} ms  {:.2}  {w}",
                    sp.start_ms, sp.end_ms, sp.score
                );
            }
        }
        None => println!("no alignment"),
    }
    Ok(())
}

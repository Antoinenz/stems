//! `cargo run -p stems-sep --features onnx --example mdx_window -- <model.onnx> [in.wav] [out.wav]`
//! Runs one model window. Without a WAV it synthesises a test signal.
use std::time::Instant;

use stems_sep::mdx::{Device, MdxModel, known_model};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("debug").init();
    let args: Vec<String> = std::env::args().collect();
    let path = std::path::PathBuf::from(&args[1]);
    let name = path.file_stem().unwrap().to_string_lossy().to_string();
    let (_, params) = known_model(&name).expect("known model name");
    let n = params.chunk_size();
    let mut model = MdxModel::load(&path, params, Device::Auto, 4)?;

    let chunk: Vec<f32> = match args.get(2) {
        Some(wav) => {
            let mut r = hound::WavReader::open(wav)?;
            let spec = r.spec();
            assert_eq!(spec.channels, 2);
            let scale = 1.0 / i16::MAX as f32;
            let mut v: Vec<f32> = r
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 * scale)
                .collect();
            v.resize(n * 2, 0.0);
            v
        }
        None => (0..n)
            .flat_map(|i| {
                let t = i as f32 / 44_100.0;
                let bass = (t * 55.0 * std::f32::consts::TAU).sin() * 0.4;
                let pad = (t * 660.0 * std::f32::consts::TAU).sin() * 0.15;
                [bass + pad, bass - pad * 0.5]
            })
            .collect(),
    };

    let started = Instant::now();
    let inst = model.instrumental_for_window(&chunk)?;
    let secs = started.elapsed().as_secs_f32();
    let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
    let finite = inst.iter().all(|x| x.is_finite());
    // correlation over the valid (untrimmed) region
    let trim = model.params().trim() * 2;
    let (a, b) = (
        &chunk[trim..chunk.len() - trim],
        &inst[trim..inst.len() - trim],
    );
    let (ma, mb) = (
        a.iter().sum::<f32>() / a.len() as f32,
        b.iter().sum::<f32>() / b.len() as f32,
    );
    let cov: f32 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f32 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f32 = b.iter().map(|y| (y - mb).powi(2)).sum();
    let corr = cov / (va.sqrt() * vb.sqrt());
    println!("correlation in/out over valid region: {corr:.4}");
    println!(
        "window {:.1}s audio in {secs:.1}s ({:.2}x realtime) provider={} rms in={:.3} out={:.3} finite={finite}",
        n as f32 / 44_100.0,
        n as f32 / 44_100.0 / secs,
        model.provider(),
        rms(&chunk),
        rms(&inst)
    );
    if let Some(out) = args.get(3) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(out, spec)?;
        for s in &inst {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        w.finalize()?;
    }
    Ok(())
}

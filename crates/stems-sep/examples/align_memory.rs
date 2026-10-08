//! Memory check for the word aligner: runs it many times on inputs of
//! random length (as real lyric lines are) and prints the resident size.
//!
//!     cargo run --release -p stems-sep --example align_memory --features onnx -- <wav2vec2.onnx> [runs]

/// Resident and virtual size of this process, MB.
fn mem() -> String {
    let mut sys = sysinfo::System::new();
    let pid = sysinfo::get_current_pid().expect("pid");
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let p = sys.process(pid).expect("process");
    format!(
        "{:.0} MB resident, {:.0} MB virtual",
        p.memory() as f64 / 1e6,
        p.virtual_memory() as f64 / 1e6
    )
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let model = args.next().expect("model path");
    let runs: usize = args.next().map(|r| r.parse().unwrap()).unwrap_or(300);
    let mut aligner = stems_sep::align::Aligner::load(std::path::Path::new(&model), 4)?;
    let mut seed = 12345u64;
    println!("start: {}", mem());
    for i in 1..=runs {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // 1.5 – 12 s of 16 kHz audio, a different length every time, and
        // now and then a long one (60 s)
        let len = if i % 40 == 0 {
            960_000
        } else {
            24_000 + (seed >> 33) as usize % 168_000
        };
        let audio: Vec<f32> = (0..len).map(|n| ((n as f32) * 0.05).sin() * 0.1).collect();
        aligner.log_probs(&audio)?;
        if i % 50 == 0 {
            println!("{i:4} runs: {}", mem());
        }
    }
    Ok(())
}

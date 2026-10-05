//! A stand-in `stemsd worker` for testing the remote/pre-buffer plumbing
//! without a model: returns the window attenuated by 0.5 (so "karaoke on"
//! is audibly different) after an optional delay.
//!
//! `cargo run -p stems-sep --example fake_worker -- [bind] [delay_ms]`
use std::net::TcpListener;

use stems_sep::remote::Hello;
use stems_sep::remote::serve::{self, Runner};
use stems_sep::{PrimaryStem, known_model};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("debug").init();
    let args: Vec<String> = std::env::args().collect();
    let bind = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "0.0.0.0:7374".into());
    let delay_ms: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let (_, mut params) = known_model("UVR-MDX-NET-Inst_HQ_3").unwrap();
    params.primary_stem = PrimaryStem::Instrumental;
    let hello = Hello {
        model: "fake-half-gain".into(),
        provider: "fake".into(),
        params,
        align: false,
    };
    let runners: Vec<Runner> = (0..2)
        .map(|_| -> Runner {
            Box::new(move |w: &[f32], _denoise: bool| {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                Ok(w.iter().map(|x| x * 0.5).collect())
            })
        })
        .collect();
    let listener = TcpListener::bind(&bind)?;
    eprintln!("fake worker on {bind} (delay {delay_ms} ms per window)");
    serve::run_with(listener, hello, runners, None)
}

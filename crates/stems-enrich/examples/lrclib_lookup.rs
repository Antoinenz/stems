//! `cargo run -p stems-enrich --example lrclib_lookup -- "Artist" "Title" 193`
use stems_core::{TrackId, TrackRef};
use stems_enrich::LrcLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let args: Vec<String> = std::env::args().collect();
    let track = TrackRef {
        id: TrackId::Spotify("example".into()),
        isrc: None,
        title: args.get(2).cloned().unwrap_or_default(),
        artists: vec![args.get(1).cloned().unwrap_or_default()],
        album: None,
        duration_ms: args.get(3).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 1000,
        art: None,
    };
    match LrcLib::new().lookup(&track).await? {
        Some(l) => {
            println!(
                "provider={} synced_lines={} plain={}",
                l.provider,
                l.lines.len(),
                l.plain.is_some()
            );
            for line in l.lines.iter().take(5) {
                println!("  [{:>6} ms] {}", line.time_ms, line.text);
            }
        }
        None => println!("no lyrics"),
    }
    Ok(())
}

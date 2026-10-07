//! The "machines" view: one JSON document about the audio path, the
//! separation workers and the library, refreshed once a second. The server
//! adds system load and connected screens and hands it to hosts
//! (`/api/status`, and pushed over the WebSocket).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::config::Config;
use crate::engine::AudioStatus;
use crate::library::LibraryService;

pub fn spawn(
    cfg: &Config,
    mode: &'static str,
    audio: watch::Receiver<AudioStatus>,
    library: Option<Arc<LibraryService>>,
) -> watch::Receiver<Value> {
    let (tx, rx) = watch::channel(Value::Null);
    let separation = separation(cfg);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let workers = stems_sep::remote::worker_stats();
            let library = match library.as_ref() {
                Some(l) => {
                    let l = Arc::clone(l);
                    // reads the disk: keep it off the async workers
                    tokio::task::spawn_blocking(move || l.status())
                        .await
                        .unwrap_or(Value::Null)
                }
                None => Value::Null,
            };
            let doc = document(mode, &audio.borrow(), &separation, &workers, library);
            if tx.send(doc).is_err() {
                break;
            }
        }
    });
    rx
}

/// The separation settings the status shows (fixed for the process).
pub fn separation(cfg: &Config) -> Value {
    json!({
        "backend": cfg.karaoke.backend,
        "mode": cfg.karaoke.mode,
        "device": cfg.karaoke.device,
        "remote": cfg.karaoke.remote,
        "local_fallback": cfg.karaoke.local_fallback,
    })
}

/// One status document; the server adds `system` and `clients`.
pub fn document(
    mode: &str,
    audio: &AudioStatus,
    separation: &Value,
    workers: &[stems_sep::remote::WorkerStat],
    library: Value,
) -> Value {
    let mut sep = separation.clone();
    sep["workers"] = json!(workers);
    json!({
        "mode": mode,
        "audio": audio,
        "separation": sep,
        "library": library,
    })
}

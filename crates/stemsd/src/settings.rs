//! Live settings: the config as the daemon runs it, editable from the
//! host remote through the server's `/api/settings`. A patch is
//! validated by round-tripping through [`Config`], written back to
//! `stems.toml` with formatting and comments preserved, and applied on
//! the fly where the running components allow it; the rest is reported
//! as needing a restart.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use serde_json::Value;
use stems_enrich::VideoFetcher;
use stems_server::{Access, SettingsUpdate};
use tokio::sync::watch;

use crate::config::Config;

/// What the engine can change while running.
#[derive(Clone)]
pub struct Live {
    pub fade_ms: u64,
    pub lyrics_offset_ms: i64,
    pub video: Option<VideoFetcher>,
    pub video_align: bool,
}

/// Dotted keys that take effect without a restart.
const LIVE_KEYS: &[&str] = &[
    "host_token",
    "lyrics_offset_ms",
    "karaoke.fade_ms",
    "karaoke.gpu_limit",
    "video.enabled",
    "video.align",
    "video.ytdlp",
    "video.max_height",
    "guests.karaoke",
    "guests.vocals",
    "guests.volume",
    "guests.stage",
    "guests.timing",
];

pub struct Settings {
    cfg: RwLock<Config>,
    path: PathBuf,
    video_dir: PathBuf,
    live_tx: watch::Sender<Live>,
    /// The pre-buffer governor's limit (f32 bits), once it exists.
    gpu_limit: Mutex<Option<Arc<AtomicU32>>>,
}

impl Settings {
    pub fn new(cfg: Config, path: PathBuf, video_dir: PathBuf) -> Arc<Self> {
        let (live_tx, _) = watch::channel(live_of(&cfg, &video_dir));
        Arc::new(Self {
            cfg: RwLock::new(cfg),
            path,
            video_dir,
            live_tx,
            gpu_limit: Mutex::new(None),
        })
    }

    pub fn live(&self) -> watch::Receiver<Live> {
        self.live_tx.subscribe()
    }

    /// Let `karaoke.gpu_limit` reach the running governor.
    pub fn attach_gpu_limit(&self, handle: Arc<AtomicU32>) {
        *self.gpu_limit.lock().expect("gpu limit") = Some(handle);
    }

    fn apply(&self, cfg: &Config) {
        let _ = self.live_tx.send(live_of(cfg, &self.video_dir));
        if let Some(h) = self.gpu_limit.lock().expect("gpu limit").as_ref() {
            h.store(cfg.karaoke.gpu_limit.to_bits(), Ordering::Relaxed);
        }
    }
}

fn live_of(cfg: &Config, video_dir: &std::path::Path) -> Live {
    Live {
        fade_ms: cfg.karaoke.fade_ms,
        lyrics_offset_ms: cfg.lyrics_offset_ms,
        video: cfg
            .video
            .enabled
            .then(|| VideoFetcher::new(video_dir, cfg.video.ytdlp.clone(), cfg.video.max_height)),
        video_align: cfg.video.align,
    }
}

impl Access for Settings {
    fn is_host(&self, token: &str) -> bool {
        let cfg = self.cfg.read().expect("config");
        !cfg.host_token.is_empty() && cfg.host_token == token
    }

    fn guest_allowed(&self, permission: &str) -> bool {
        self.cfg.read().expect("config").guests.allows(permission)
    }

    fn settings(&self) -> Value {
        let cfg = self.cfg.read().expect("config").clone();
        serde_json::json!({
            "config": serde_json::to_value(cfg).unwrap_or(Value::Null),
            "live_keys": LIVE_KEYS,
        })
    }

    fn update_settings(&self, patch: Value) -> std::result::Result<SettingsUpdate, String> {
        let current = self.cfg.read().expect("config").clone();
        let before = serde_json::to_value(&current).map_err(|e| e.to_string())?;
        let mut merged = before.clone();
        merge(&mut merged, &patch, "")?;
        let next: Config = serde_json::from_value(merged.clone()).map_err(|e| e.to_string())?;
        validate(&next)?;
        let after = serde_json::to_value(&next).map_err(|e| e.to_string())?;
        let mut changed = Vec::new();
        diff(&before, &after, "", &mut changed);
        if changed.is_empty() {
            return Ok(SettingsUpdate::default());
        }
        write_toml(&self.path, &after, &changed).map_err(|e| format!("{e:#}"))?;
        *self.cfg.write().expect("config") = next.clone();
        self.apply(&next);
        let restart_required: Vec<String> = changed
            .iter()
            .filter(|k| !LIVE_KEYS.contains(&k.as_str()))
            .cloned()
            .collect();
        tracing::info!(changed = ?changed, restart = ?restart_required, "settings updated");
        Ok(SettingsUpdate {
            changed,
            restart_required,
        })
    }
}

/// Overlay `patch` onto `base`, refusing keys the config does not have.
fn merge(base: &mut Value, patch: &Value, path: &str) -> std::result::Result<(), String> {
    let Value::Object(p) = patch else {
        return Err(format!("`{path}` must be an object"));
    };
    let Value::Object(b) = base else {
        return Err(format!("`{path}` is not a section"));
    };
    for (k, v) in p {
        let key = if path.is_empty() {
            k.clone()
        } else {
            format!("{path}.{k}")
        };
        match b.get_mut(k) {
            None => return Err(format!("unknown setting `{key}`")),
            Some(cur) if cur.is_object() => merge(cur, v, &key)?,
            Some(cur) => {
                if v.is_object() || v.is_array() {
                    return Err(format!("`{key}` expects a single value"));
                }
                *cur = v.clone();
            }
        }
    }
    Ok(())
}

fn diff(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, va) in x {
                let key = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                if let Some(vb) = y.get(k) {
                    diff(va, vb, &key, out);
                }
            }
        }
        _ if a != b => out.push(path.to_string()),
        _ => {}
    }
}

fn validate(cfg: &Config) -> std::result::Result<(), String> {
    let g = cfg.karaoke.gpu_limit;
    if !(0.1..=1.0).contains(&g) {
        return Err("karaoke.gpu_limit must be between 0.1 and 1.0".into());
    }
    if cfg.karaoke.fade_ms > 10_000 {
        return Err("karaoke.fade_ms must be at most 10000".into());
    }
    if cfg.device_name.trim().is_empty() {
        return Err("device_name must not be empty".into());
    }
    if cfg.port == 0 {
        return Err("port must be set".into());
    }
    if !(-30.0..=-5.0).contains(&cfg.library.target_lufs) {
        return Err("library.target_lufs must be between -30 and -5".into());
    }
    Ok(())
}

/// Set the changed keys in the file, keeping everything else as the
/// user wrote it.
fn write_toml(path: &std::path::Path, after: &Value, changed: &[String]) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parse {}", path.display()))?;
    for key in changed {
        let parts: Vec<&str> = key.split('.').collect();
        let mut v = after;
        for p in &parts {
            v = &v[*p];
        }
        let item = toml_edit::value(toml_value(v)?);
        match parts.as_slice() {
            [k] => {
                doc[k] = item;
            }
            [table, k] => {
                if doc.get(table).is_none() {
                    doc[table] = toml_edit::table();
                }
                doc[table][k] = item;
            }
            _ => anyhow::bail!("unsupported key depth: {key}"),
        }
    }
    let tmp = path.with_extension("toml.part");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn toml_value(v: &Value) -> Result<toml_edit::Value> {
    Ok(match v {
        Value::Bool(b) => (*b).into(),
        Value::Number(n) if n.is_i64() => n.as_i64().unwrap().into(),
        Value::Number(n) if n.is_u64() => (n.as_u64().unwrap() as i64).into(),
        Value::Number(n) => n.as_f64().unwrap().into(),
        Value::String(s) => s.as_str().into(),
        other => anyhow::bail!("cannot store {other} in the config"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, cfg: &Config) -> (Arc<Settings>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("stems-settings-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stems.toml");
        std::fs::write(
            &path,
            "# my notes\ndevice_name = \"Party TV\"\n\n[karaoke]\nbackend = \"mdx\" # gpu\n",
        )
        .unwrap();
        (
            Settings::new(cfg.clone(), path.clone(), dir.join("video")),
            path,
        )
    }

    #[test]
    fn patch_persists_and_reports_live_vs_restart() {
        let mut cfg = Config {
            device_name: "Party TV".into(),
            ..Default::default()
        };
        cfg.karaoke.backend = "mdx".into();
        let (s, path) = temp("patch", &cfg);
        let u = s
            .update_settings(serde_json::json!({
                "karaoke": {"gpu_limit": 0.6, "workers": 2},
                "guests": {"volume": true}
            }))
            .unwrap();
        // keys come back in config (alphabetical) order, not patch order
        let mut changed = u.changed.clone();
        changed.sort();
        assert_eq!(
            changed,
            vec!["guests.volume", "karaoke.gpu_limit", "karaoke.workers"]
        );
        assert_eq!(u.restart_required, vec!["karaoke.workers"]);
        assert!(s.guest_allowed("volume"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my notes"), "{text}");
        assert!(text.contains("backend = \"mdx\" # gpu"), "{text}");
        assert!(text.contains("gpu_limit = 0.6"), "{text}");
        assert!(text.contains("[guests]"), "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.karaoke.workers, 2);
        assert!(back.guests.volume);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        let (s, path) = temp("reject", &Config::default());
        assert!(
            s.update_settings(serde_json::json!({"karaoke": {"nope": 1}}))
                .unwrap_err()
                .contains("unknown setting `karaoke.nope`")
        );
        assert!(
            s.update_settings(serde_json::json!({"karaoke": {"gpu_limit": 3.0}}))
                .unwrap_err()
                .contains("gpu_limit")
        );
        assert!(
            s.update_settings(serde_json::json!({"port": "abc"}))
                .is_err()
        );
        assert!(
            s.update_settings(serde_json::json!({"library": {"target_lufs": 0.0}}))
                .unwrap_err()
                .contains("target_lufs")
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn host_token_and_guest_policy() {
        let cfg = Config {
            host_token: "ABC123".into(),
            ..Default::default()
        };
        let (s, path) = temp("token", &cfg);
        assert!(s.is_host("ABC123"));
        assert!(!s.is_host("nope"));
        assert!(!s.is_host(""));
        assert!(s.guest_allowed("karaoke"));
        assert!(!s.guest_allowed("volume"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

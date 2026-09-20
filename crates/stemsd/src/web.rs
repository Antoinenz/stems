//! Keep the web apps' `dist/` current when running from a checkout: if any
//! source is newer than the built `index.html` (or there is no build),
//! run `pnpm build` before serving. Skipped when pnpm is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

const APPS: &[&str] = &["stage", "remote"];

/// Rebuild stale apps under `web_dir`. Never fails the launch: problems
/// are logged and the old build (if any) keeps being served.
pub fn ensure_built(web_dir: &Path) {
    let shared = web_dir.join("shared");
    for app in APPS {
        let dir = web_dir.join(app);
        if !dir.join("package.json").is_file() {
            continue;
        }
        let dist_index = dir.join("dist").join("index.html");
        let built = mtime(&dist_index);
        let newest_src = [
            dir.join("src"),
            dir.join("index.html"),
            dir.join("package.json"),
            shared.clone(),
        ]
        .iter()
        .filter_map(|p| newest(p))
        .max();
        let stale = match (built, newest_src) {
            (None, _) => true,
            (Some(b), Some(s)) => s > b,
            (Some(_), None) => false,
        };
        if !stale {
            continue;
        }
        if !dir.join("node_modules").is_dir() {
            tracing::info!(app, "installing web app dependencies");
            if !run(&dir, &["install"]) {
                continue;
            }
        }
        tracing::info!(app, "web app sources changed; rebuilding");
        run(&dir, &["build"]);
    }
}

fn run(dir: &Path, args: &[&str]) -> bool {
    match Command::new("pnpm").args(args).current_dir(dir).output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            tracing::warn!(
                dir = %dir.display(),
                "pnpm {} failed: {}",
                args.join(" "),
                err.lines().last().unwrap_or("")
            );
            false
        }
        Err(e) => {
            tracing::warn!("pnpm not available ({e}); serving the existing build");
            false
        }
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Newest modification time under `p` (file or directory tree).
fn newest(p: &Path) -> Option<SystemTime> {
    if p.is_file() {
        return mtime(p);
    }
    let mut best: Option<SystemTime> = None;
    let mut stack: Vec<PathBuf> = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(t) = mtime(&path) {
                best = Some(best.map_or(t, |b| b.max(t)));
            }
        }
    }
    best
}

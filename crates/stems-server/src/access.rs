//! Who may do what. Two roles: the **host** (whoever runs the party;
//! proves it with a token) and **guests** (anyone on the LAN). Guests
//! get a host-editable subset of the commands. Settings are host-only.
//!
//! The daemon implements [`Access`]; the server only asks questions.

use std::sync::Arc;

use axum::http::HeaderMap;
use serde::Serialize;
use stems_core::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Guest,
}

/// Permission keys guests can be granted, in the order a UI lists them.
pub const PERMISSIONS: &[&str] = &["karaoke", "vocals", "volume", "stage", "timing", "queue"];

/// The permission a command needs.
pub fn permission(cmd: &Command) -> &'static str {
    match cmd {
        Command::SetKaraoke { .. } => "karaoke",
        Command::SetVocalGain { .. } => "vocals",
        Command::SetVolume { .. } => "volume",
        Command::SetViewMode { .. } => "stage",
        Command::SetLyricsOffset { .. } | Command::SetVideoOffset { .. } => "timing",
        Command::Play
        | Command::Pause
        | Command::Next
        | Command::Seek { .. }
        | Command::PlayTrack { .. }
        | Command::Enqueue { .. }
        | Command::Dequeue { .. }
        | Command::MoveInQueue { .. }
        | Command::ClearQueue => "queue",
        // running the show is the host's alone: not in PERMISSIONS, so
        // guests can never be granted it
        Command::SetMode { .. }
        | Command::SetAfterSong { .. }
        | Command::FadeOut { .. }
        | Command::TalkOver { .. }
        | Command::Resume
        | Command::SetAutoDj { .. }
        | Command::SetPool { .. } => "show",
    }
}

/// Result of a settings patch.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SettingsUpdate {
    /// Dotted keys that changed.
    pub changed: Vec<String>,
    /// Changed keys that only take effect after a restart.
    pub restart_required: Vec<String>,
}

pub trait Access: Send + Sync {
    /// Whether `token` identifies the host.
    fn is_host(&self, token: &str) -> bool;
    /// Whether guests may use `permission`.
    fn guest_allowed(&self, permission: &str) -> bool;
    /// Live settings as JSON.
    fn settings(&self) -> serde_json::Value;
    /// Merge a JSON patch into the settings, persist and apply what can
    /// be applied live. `Err` carries a user-facing message (bad value).
    fn update_settings(&self, patch: serde_json::Value) -> Result<SettingsUpdate, String>;
}

/// Everyone is the host, nothing is configurable. For tests and for
/// embedding without a config file.
pub struct Open;

impl Access for Open {
    fn is_host(&self, _token: &str) -> bool {
        true
    }
    fn guest_allowed(&self, _permission: &str) -> bool {
        true
    }
    fn settings(&self) -> serde_json::Value {
        serde_json::Value::Object(Default::default())
    }
    fn update_settings(&self, _patch: serde_json::Value) -> Result<SettingsUpdate, String> {
        Err("settings are not configurable here".into())
    }
}

/// Token from `Authorization: Bearer …`, `X-Stems-Token`, or `?token=`
/// (the WebSocket upgrade cannot carry headers from a browser).
pub fn token_from(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(v) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        return Some(v.trim().to_string());
    }
    if let Some(v) = headers.get("x-stems-token").and_then(|h| h.to_str().ok()) {
        return Some(v.trim().to_string());
    }
    query.and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            .map(|v| v.to_string())
    })
}

pub fn role_of(access: &Arc<dyn Access>, headers: &HeaderMap, query: Option<&str>) -> Role {
    match token_from(headers, query) {
        Some(t) if !t.is_empty() && access.is_host(&t) => Role::Host,
        _ => Role::Guest,
    }
}

/// Whether `role` may issue `cmd`; `Err` names the missing permission.
pub fn check(access: &Arc<dyn Access>, role: Role, cmd: &Command) -> Result<(), &'static str> {
    let p = permission(cmd);
    if role == Role::Host || access.guest_allowed(p) {
        Ok(())
    } else {
        Err(p)
    }
}

/// What the caller may do, for a UI to hide what it cannot.
#[derive(Debug, Serialize)]
pub struct Me {
    pub role: Role,
    pub permissions: Vec<&'static str>,
}

pub fn me(access: &Arc<dyn Access>, role: Role) -> Me {
    let permissions = PERMISSIONS
        .iter()
        .copied()
        .filter(|p| role == Role::Host || access.guest_allowed(p))
        .collect();
    Me { role, permissions }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl Access for Fixed {
        fn is_host(&self, token: &str) -> bool {
            token == "secret"
        }
        fn guest_allowed(&self, permission: &str) -> bool {
            permission == "karaoke"
        }
        fn settings(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn update_settings(&self, _: serde_json::Value) -> Result<SettingsUpdate, String> {
            Ok(Default::default())
        }
    }

    #[test]
    fn token_sources() {
        let mut h = HeaderMap::new();
        assert_eq!(token_from(&h, Some("a=1&token=abc")), Some("abc".into()));
        h.insert("x-stems-token", "xyz".parse().unwrap());
        assert_eq!(token_from(&h, None), Some("xyz".into()));
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer top".parse().unwrap(),
        );
        assert_eq!(token_from(&h, None), Some("top".into()));
    }

    #[test]
    fn guests_get_only_granted_commands() {
        let a: Arc<dyn Access> = Arc::new(Fixed);
        let h = HeaderMap::new();
        let guest = role_of(&a, &h, None);
        assert_eq!(guest, Role::Guest);
        assert!(check(&a, guest, &Command::SetKaraoke { enabled: true }).is_ok());
        assert_eq!(
            check(&a, guest, &Command::SetVolume { level: 0.5 }),
            Err("volume")
        );
        let host = role_of(&a, &h, Some("token=secret"));
        assert_eq!(host, Role::Host);
        assert!(check(&a, host, &Command::SetVolume { level: 0.5 }).is_ok());
        assert_eq!(me(&a, guest).permissions, vec!["karaoke"]);
        assert_eq!(me(&a, host).permissions, PERMISSIONS);
    }
}

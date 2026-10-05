//! Local HTTP + WebSocket server.
//!
//! * `GET  /api/state`  — current [`PlaybackState`] as JSON
//! * `GET  /api/ws`     — WebSocket: `{"type":"state",...}` on connect and on
//!   every change, `{"type":"clock",...}` at ~10 Hz; accepts [`Command`] JSON
//! * `POST /api/command` — one [`Command`] as JSON
//! * `GET  /api/me` — the caller's role and permissions
//! * `GET|PATCH /api/settings` — live settings (host only)
//! * `GET  /api/library?q=` — the offline library (guests: playable tracks)
//! * `GET|POST /api/library/import` — import progress / queue imports (host)
//! * `GET  /api/health`
//! * `/stage`, `/remote`, `/` — static files from the web directory, if present
//!
//! Callers are the host (token via `Authorization: Bearer`, `X-Stems-Token`
//! or `?token=`) or guests, who get the commands the host allows; see
//! [`access`]. The server owns no state: it forwards `watch` channels from
//! the engine and pushes commands back through an `mpsc`.

pub mod access;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use stems_core::{ClockTick, Command, PlaybackState};

pub use access::{Access, Open, Role, SettingsUpdate};
use tokio::sync::{mpsc, watch};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

#[derive(Clone)]
pub struct Channels {
    pub state: watch::Receiver<PlaybackState>,
    pub clock: watch::Receiver<ClockTick>,
    pub commands: mpsc::Sender<Command>,
}

#[derive(Clone)]
struct App {
    ch: Channels,
    access: Arc<dyn Access>,
    library: Option<Arc<dyn LibraryApi>>,
}

/// The offline library as the server sees it; the daemon implements it.
pub trait LibraryApi: Send + Sync {
    /// Tracks matching every word of `query` (all when empty) as a JSON
    /// array. `full` adds import details (host view); otherwise only
    /// playable tracks are listed.
    fn list(&self, query: &str, full: bool) -> Result<serde_json::Value, String>;
    /// Queue Spotify links / audio paths for import.
    fn import(&self, sources: Vec<String>) -> Result<(), String>;
    /// Import progress as JSON.
    fn import_status(&self) -> serde_json::Value;
}

/// Everything optional the router serves.
#[derive(Clone, Default)]
pub struct Options {
    /// Name this box advertises to Spotify Connect / AirPlay.
    pub device_name: String,
    /// Directory with the built web apps (`<dir>/stage/dist`, …).
    pub web_dir: Option<PathBuf>,
    /// Downloaded music videos, served at `/media/video/`.
    pub video_dir: Option<PathBuf>,
    /// The offline library folder; each track's video and cover are
    /// served at `/media/library/<key>/{video.mp4,cover.jpg}`.
    pub library_dir: Option<PathBuf>,
    pub library: Option<Arc<dyn LibraryApi>>,
}

/// Host load, for the remote's status line. Sampled at most every 2 s.
#[derive(Serialize, Clone, Default)]
pub struct SystemStats {
    pub cpu_percent: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    /// Temperature sensors the OS exposes (none on some platforms).
    pub temps: Vec<Temp>,
}

#[derive(Serialize, Clone)]
pub struct Temp {
    pub label: String,
    pub celsius: f32,
}

struct SysMon {
    sys: sysinfo::System,
    components: sysinfo::Components,
    last: Option<std::time::Instant>,
    stats: SystemStats,
}

impl SysMon {
    fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self {
            sys,
            components: sysinfo::Components::new_with_refreshed_list(),
            last: None,
            stats: SystemStats::default(),
        }
    }

    fn sample(&mut self) -> SystemStats {
        let fresh = self
            .last
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2));
        if fresh {
            return self.stats.clone();
        }
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.components.refresh(false);
        let mut temps: Vec<Temp> = self
            .components
            .iter()
            .filter_map(|c| {
                c.temperature().map(|t| Temp {
                    label: c.label().to_string(),
                    celsius: t,
                })
            })
            .filter(|t| t.celsius > 0.0 && t.celsius < 150.0)
            .collect();
        // keep the interesting ones: CPU/GPU/SoC-ish, hottest first
        let keyword = |l: &str| {
            let l = l.to_ascii_lowercase();
            [
                "cpu", "gpu", "soc", "core", "package", "thermal", "pmu", "die",
            ]
            .iter()
            .any(|k| l.contains(k))
        };
        if temps.iter().any(|t| keyword(&t.label)) {
            temps.retain(|t| keyword(&t.label));
        }
        temps.sort_by(|a, b| b.celsius.total_cmp(&a.celsius));
        temps.truncate(4);
        self.stats = SystemStats {
            cpu_percent: self.sys.global_cpu_usage(),
            mem_used_mb: self.sys.used_memory() / (1024 * 1024),
            mem_total_mb: self.sys.total_memory() / (1024 * 1024),
            temps,
        };
        self.last = Some(std::time::Instant::now());
        self.stats.clone()
    }
}

/// What a client needs to point other devices at this box.
#[derive(Serialize)]
pub struct Info {
    /// Name this box advertises to Spotify Connect / AirPlay.
    pub device_name: String,
    /// Best guess at the LAN address of the server, `host:port`.
    pub lan_host: Option<String>,
    pub remote_url: Option<String>,
    pub stage_url: Option<String>,
}

/// The LAN interface a default route would use. No packet is sent: a
/// UDP "connect" only picks the source address.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET; never routed anywhere
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Outbound<'a> {
    State(&'a PlaybackState),
    Clock(&'a ClockTick),
    Error { message: String },
}

pub struct Server {
    pub addr: SocketAddr,
    pub access: Arc<dyn Access>,
    pub options: Options,
}

impl Server {
    pub fn router(ch: Channels, access: Arc<dyn Access>, opts: &Options) -> Router {
        let device_name = Arc::new(opts.device_name.clone());
        let sysmon = Arc::new(std::sync::Mutex::new(SysMon::new()));
        let mut app = Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/api/state", get(get_state))
            .route("/api/command", post(post_command))
            .route("/api/me", get(get_me))
            .route("/api/settings", get(get_settings).patch(patch_settings))
            .route("/api/ws", get(ws_upgrade))
            .route("/api/library", get(get_library))
            .route(
                "/api/library/import",
                get(get_import_status).post(post_import),
            )
            .route("/api/info", {
                let name = device_name.clone();
                get(move |headers| async move { Json(info(&headers, &name)) })
            })
            .route("/api/remote-qr.svg", get(get_remote_qr))
            .route("/api/system", {
                let m = sysmon.clone();
                get(move || async move { Json(m.lock().expect("sysmon").sample()) })
            })
            .with_state(App {
                ch,
                access,
                library: opts.library.clone(),
            })
            .layer(CorsLayer::permissive());

        if let Some(dir) = opts.web_dir.as_deref() {
            // Vite hashes asset file names, so only the HTML shell must not
            // be cached — otherwise browsers keep loading the old bundle.
            let no_cache = SetResponseHeaderLayer::if_not_present(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache"),
            );
            for app_name in ["stage", "remote"] {
                let dist = dir.join(app_name).join("dist");
                if dist.join("index.html").is_file() {
                    tracing::info!(app = app_name, dir = %dist.display(), "serving web app");
                    let index = ServeFile::new(dist.join("index.html"));
                    let svc = tower::ServiceBuilder::new()
                        .layer(no_cache.clone())
                        .service(ServeDir::new(&dist).fallback(index));
                    app = app.nest_service(&format!("/{app_name}"), svc);
                } else {
                    tracing::warn!(app = app_name, dir = %dist.display(), "web app not built; skipping");
                }
            }
            app = app.route("/", get(|| async { Redirect::temporary("/remote/") }));
        }
        if let Some(dir) = opts.video_dir.as_deref() {
            // ServeDir handles Range requests, which <video> seeking needs.
            app = app.nest_service("/media/video", ServeDir::new(dir));
        }
        if let Some(dir) = opts.library_dir.as_deref() {
            let tracks = Arc::new(dir.join("tracks"));
            app = app.route(
                "/media/library/{key}/{file}",
                get(move |path, req| library_media(tracks.clone(), path, req)),
            );
        }
        app
    }

    /// Bind and serve until the task is dropped / the process exits.
    pub async fn serve(self, ch: Channels) -> Result<()> {
        let app = Self::router(ch, self.access.clone(), &self.options);
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .with_context(|| format!("bind {}", self.addr))?;
        tracing::info!(addr = %self.addr, "http server listening");
        axum::serve(listener, app).await.context("http server")
    }
}

/// A track's video or cover from the library. Only those two files are
/// exposed (never the audio), and keys cannot climb out of the folder.
async fn library_media(
    tracks: Arc<PathBuf>,
    axum::extract::Path((key, file)): axum::extract::Path<(String, String)>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use tower::ServiceExt;
    let key_ok = !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !key_ok || !matches!(file.as_str(), "video.mp4" | "cover.jpg") {
        return StatusCode::NOT_FOUND.into_response();
    }
    // ServeFile handles Range requests, which <video> seeking needs
    match ServeFile::new(tracks.join(&key).join(&file))
        .oneshot(req)
        .await
    {
        Ok(r) => r.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn info(headers: &axum::http::HeaderMap, device_name: &str) -> Info {
    let port = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.rsplit(':').next())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(80);
    let lan_host = lan_ip().map(|ip| format!("{ip}:{port}"));
    Info {
        device_name: device_name.to_string(),
        remote_url: lan_host.as_ref().map(|h| format!("http://{h}/remote/")),
        stage_url: lan_host.as_ref().map(|h| format!("http://{h}/stage/")),
        lan_host,
    }
}

/// QR code of the remote's LAN URL, for the stage to show.
async fn get_remote_qr(headers: axum::http::HeaderMap) -> impl IntoResponse {
    let Some(url) = info(&headers, "").remote_url else {
        return (StatusCode::NOT_FOUND, "no LAN address").into_response();
    };
    match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(160, 160)
                .quiet_zone(true)
                .dark_color(qrcode::render::svg::Color("#000"))
                .light_color(qrcode::render::svg::Color("#fff"))
                .build();
            ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_state(State(app): State<App>) -> Json<PlaybackState> {
    Json(app.ch.state.borrow().clone())
}

fn forbidden(permission: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": format!("guests may not change {permission} here"),
            "permission": permission,
        })),
    )
}

async fn post_command(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    Json(cmd): Json<Command>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let role = access::role_of(&app.access, &headers, query.as_deref());
    access::check(&app.access, role, &cmd).map_err(forbidden)?;
    app.ch
        .commands
        .send(cmd)
        .await
        .map(|_| StatusCode::ACCEPTED)
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "engine not running"})),
            )
        })
}

#[derive(serde::Deserialize, Default)]
struct LibraryQuery {
    #[serde(default)]
    q: String,
}

fn no_library() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "no library in this mode (run `stemsd library`)"})),
    )
}

async fn get_library(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    axum::extract::Query(q): axum::extract::Query<LibraryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let lib = app.library.as_ref().ok_or_else(no_library)?;
    let full = access::role_of(&app.access, &headers, query.as_deref()) == Role::Host;
    lib.list(&q.q, full).map(Json).map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": e})),
        )
    })
}

#[derive(serde::Deserialize)]
struct ImportRequest {
    sources: Vec<String>,
}

async fn post_import(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    Json(req): Json<ImportRequest>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    host_only(&app, &headers, query.as_deref())?;
    let lib = app.library.as_ref().ok_or_else(no_library)?;
    lib.import(req.sources)
        .map(|_| StatusCode::ACCEPTED)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            )
        })
}

async fn get_import_status(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    host_only(&app, &headers, query.as_deref())?;
    let lib = app.library.as_ref().ok_or_else(no_library)?;
    Ok(Json(lib.import_status()))
}

async fn get_me(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Json<access::Me> {
    let role = access::role_of(&app.access, &headers, query.as_deref());
    Json(access::me(&app.access, role))
}

fn host_only(
    app: &App,
    headers: &axum::http::HeaderMap,
    query: Option<&str>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match access::role_of(&app.access, headers, query) {
        Role::Host => Ok(()),
        Role::Guest => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "host token required"})),
        )),
    }
}

async fn get_settings(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    host_only(&app, &headers, query.as_deref())?;
    Ok(Json(app.access.settings()))
}

async fn patch_settings(
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<SettingsUpdate>, (StatusCode, Json<serde_json::Value>)> {
    host_only(&app, &headers, query.as_deref())?;
    app.access.update_settings(patch).map(Json).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
    })
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(app): State<App>,
    headers: axum::http::HeaderMap,
    RawQuery(query): RawQuery,
) -> impl IntoResponse {
    let role = access::role_of(&app.access, &headers, query.as_deref());
    ws.on_upgrade(move |socket| ws_session(socket, app, role))
}

async fn send(
    tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    msg: Outbound<'_>,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(&msg).expect("serializable");
    tx.send(Message::Text(text.into())).await
}

async fn ws_session(socket: WebSocket, app: App, role: Role) {
    let App { mut ch, access, .. } = app;
    let (mut tx, mut rx) = socket.split();

    // Initial snapshot.
    {
        let snapshot = ch.state.borrow().clone();
        if send(&mut tx, Outbound::State(&snapshot)).await.is_err() {
            return;
        }
    }
    ch.state.mark_unchanged();
    ch.clock.mark_unchanged();

    loop {
        tokio::select! {
            changed = ch.state.changed() => {
                if changed.is_err() { break; }
                let snapshot = ch.state.borrow_and_update().clone();
                if send(&mut tx, Outbound::State(&snapshot)).await.is_err() { break; }
            }
            changed = ch.clock.changed() => {
                if changed.is_err() { break; }
                let tick = *ch.clock.borrow_and_update();
                if send(&mut tx, Outbound::Clock(&tick)).await.is_err() { break; }
            }
            incoming = rx.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Command>(&text) {
                        Ok(cmd) => match access::check(&access, role, &cmd) {
                            Ok(()) => {
                                if ch.commands.send(cmd).await.is_err() { break; }
                            }
                            Err(p) => {
                                let _ = send(&mut tx, Outbound::Error { message: format!("guests may not change {p} here") }).await;
                            }
                        },
                        Err(e) => {
                            let _ = send(&mut tx, Outbound::Error { message: format!("bad command: {e}") }).await;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn channels() -> (
        Channels,
        watch::Sender<PlaybackState>,
        mpsc::Receiver<Command>,
    ) {
        let (state_tx, state_rx) = watch::channel(PlaybackState::default());
        let (_clock_tx, clock_rx) = watch::channel(ClockTick::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        (
            Channels {
                state: state_rx,
                clock: clock_rx,
                commands: cmd_tx,
            },
            state_tx,
            cmd_rx,
        )
    }

    #[tokio::test]
    async fn state_endpoint_returns_json() {
        let (ch, state_tx, _rx) = channels();
        state_tx.send_modify(|s| s.volume = 0.42);
        let app = Server::router(ch, Arc::new(Open), &Options::default());
        let res = app
            .oneshot(Request::get("/api/state").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["volume"], 0.42);
        assert_eq!(v["status"], "idle");
    }

    #[tokio::test]
    async fn command_endpoint_forwards_to_engine() {
        let (ch, _state_tx, mut rx) = channels();
        let app = Server::router(ch, Arc::new(Open), &Options::default());
        let res = app
            .oneshot(
                Request::post("/api/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"cmd":"set_karaoke","enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert_eq!(rx.recv().await, Some(Command::SetKaraoke { enabled: true }));
    }

    /// Host is whoever sends token `T`.
    struct TokenT;
    impl Access for TokenT {
        fn is_host(&self, token: &str) -> bool {
            token == "T"
        }
        fn guest_allowed(&self, _p: &str) -> bool {
            false
        }
        fn settings(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn update_settings(&self, _p: serde_json::Value) -> Result<SettingsUpdate, String> {
            Err("no".into())
        }
    }

    #[derive(Default)]
    struct FakeLibrary {
        imported: std::sync::Mutex<Vec<String>>,
    }
    impl LibraryApi for FakeLibrary {
        fn list(&self, query: &str, full: bool) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!([{"q": query, "full": full}]))
        }
        fn import(&self, sources: Vec<String>) -> Result<(), String> {
            self.imported.lock().unwrap().extend(sources);
            Ok(())
        }
        fn import_status(&self) -> serde_json::Value {
            serde_json::json!({"running": false})
        }
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    #[tokio::test]
    async fn library_routes_respect_roles() {
        let (ch, _state_tx, _rx) = channels();
        let lib = Arc::new(FakeLibrary::default());
        let opts = Options {
            library: Some(lib.clone()),
            ..Default::default()
        };
        let app = Server::router(ch, Arc::new(TokenT), &opts);

        let get = |uri: &str, host: bool| {
            let mut r = Request::get(uri);
            if host {
                r = r.header("x-stems-token", "T");
            }
            r.body(Body::empty()).unwrap()
        };
        let (s, v) = call(&app, get("/api/library?q=killers", false)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v[0], serde_json::json!({"q": "killers", "full": false}));
        let (_, v) = call(&app, get("/api/library", true)).await;
        assert_eq!(v[0]["full"], true);

        let post = |host: bool| {
            let mut r =
                Request::post("/api/library/import").header("content-type", "application/json");
            if host {
                r = r.header("x-stems-token", "T");
            }
            r.body(Body::from(r#"{"sources":["spotify:track:x"]}"#))
                .unwrap()
        };
        assert_eq!(call(&app, post(false)).await.0, StatusCode::FORBIDDEN);
        assert_eq!(call(&app, post(true)).await.0, StatusCode::ACCEPTED);
        assert_eq!(*lib.imported.lock().unwrap(), vec!["spotify:track:x"]);
        assert_eq!(
            call(&app, get("/api/library/import", false)).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&app, get("/api/library/import", true)).await.0,
            StatusCode::OK
        );

        // without a library (live modes) the routes say so
        let (ch, _s, _r) = channels();
        let bare = Server::router(ch, Arc::new(TokenT), &Options::default());
        assert_eq!(
            call(&bare, get("/api/library", true)).await.0,
            StatusCode::NOT_FOUND
        );
    }
}

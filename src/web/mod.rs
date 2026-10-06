//! `diskeye serve`: a token-protected JSON API on 127.0.0.1 plus an embedded
//! single-page frontend (vanilla JS + vendored d3, see `web/`).

pub mod api;
#[cfg(test)]
mod tests;

use crate::model::Snapshot;
use anyhow::{Context, Result};
use api::{AppState, Rescanner, Server};
use axum::Router;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(rust_embed::RustEmbed)]
#[folder = "web/"]
struct Assets;

pub fn serve(snap: Snapshot, path: Option<PathBuf>, port: u16, open: bool, rescanner: Option<Rescanner>) -> Result<()> {
    let token = random_token()?;
    let is_root = crate::util::is_root();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("starting tokio runtime")?;
    rt.block_on(async move {
        let (listener, port) = bind(port).await?;
        let mut state = AppState::new(snap, path, token.clone(), is_root);
        state.allowed_hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        let app = router_with(Server::new(Arc::new(state), rescanner));
        let url = format!("http://127.0.0.1:{port}/#token={token}");
        if is_root {
            eprintln!(
                "\x1b[1;31m!! diskeye is serving as ROOT.\x1b[0m Cleanup actions run with full privileges and need `delete` \
                 typed to confirm. Stop the server (Ctrl-C) when you're done."
            );
        }
        eprintln!("diskeye web UI: {url}");
        eprintln!("(listening on 127.0.0.1:{port} only; the token in the URL is required for the API. Ctrl-C to stop)");
        if open {
            open_browser(&url);
        }
        axum::serve(listener, app).await.context("serving")
    })
}

/// The full router over a fixed snapshot (no rescan).
#[cfg(test)]
pub fn router(state: Arc<AppState>) -> Router {
    router_with(Server::new(state, None))
}

/// The full router; `/api/*` requires the token, static assets don't.
pub fn router_with(state: Server) -> Router {
    Router::new()
        .route("/api/summary", get(api::summary))
        .route("/api/physical", get(api::physical))
        .route("/api/filesystems", get(api::filesystems))
        .route("/api/roots", get(api::roots))
        .route("/api/node/{id}", get(api::node))
        .route("/api/path", get(api::path))
        .route("/api/workloads", get(api::workloads))
        .route("/api/entity/{id}", get(api::entity))
        .route("/api/reclaim", get(api::reclaim))
        .route("/api/hotspots", get(api::hotspots))
        .route("/api/deleted-open", get(api::deleted_open))
        .route("/api/snapshots", get(api::snapshots))
        .route("/api/diff", get(api::diff))
        .route("/api/rescan", get(api::rescan_status).post(api::rescan_start))
        .route("/api/action/{id}/preview", post(api::action_preview))
        .route("/api/action/{id}/execute", post(api::action_execute))
        .route("/api/{*rest}", get(api_not_found).post(api_not_found))
        .fallback(static_file)
        .layer(axum::middleware::from_fn_with_state(state.clone(), api::guard))
        .with_state(state)
}

async fn api_not_found() -> Response {
    api::err(StatusCode::NOT_FOUND, "no such endpoint")
}

async fn bind(port: u16) -> Result<(tokio::net::TcpListener, u16)> {
    let mut last = None;
    for p in port..port.saturating_add(20) {
        match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => return Ok((l, p)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("no port")))
        .with_context(|| format!("binding 127.0.0.1:{port}..{}", port.saturating_add(19)))
}

/// 128 random bits from the kernel, hex encoded.
fn random_token() -> Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).context("reading /dev/urandom")?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Open the URL in the desktop user's browser. Under sudo, run `xdg-open` as
/// the invoking user (root usually can't reach their display or browser profile).
fn open_browser(url: &str) {
    let mut cmd = match crate::util::sudo_user() {
        Some(user) => {
            let mut c = std::process::Command::new("sudo");
            c.args(["-u", &user, "env"]);
            let (uid, _) = crate::util::invoking_ids();
            for k in ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS"] {
                if let Ok(v) = std::env::var(k) {
                    c.arg(format!("{k}={v}"));
                }
            }
            c.arg(format!("XDG_RUNTIME_DIR=/run/user/{uid}"));
            c.args(["xdg-open", url]);
            c
        }
        None if crate::util::is_root() => {
            eprintln!("(running as root without sudo: open the URL above yourself)");
            return;
        }
        None => {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(url);
            c
        }
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                if !child.wait().is_ok_and(|s| s.success()) {
                    eprintln!("(could not open a browser automatically; open the URL above)");
                }
            });
        }
        Err(_) => eprintln!("(xdg-open not available; open the URL above)"),
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(f) => {
            let mut r = f.data.into_owned().into_response();
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(path)));
            let cache = if path == "index.html" { "no-store" } else { "no-cache" };
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            if path.ends_with(".html") {
                h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
            }
            r
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

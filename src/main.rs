use anyhow::{Context as _, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use uuid::Uuid;

const DEFAULT_ADDR: &str = "127.0.0.1:8756";
const DEFAULT_SCINTILLA_URL: &str = "http://127.0.0.1:8750";

#[derive(Clone)]
struct AppState {
    token: Arc<str>,
    scintilla_token: Arc<str>,
    scintilla_url: Arc<str>,
    client: reqwest::Client,
    started_at: Instant,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    product: &'static str,
    execution_backend: &'static str,
    uptime_ms: u128,
    scintilla: Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "giw_desktop_daemon=info".into()),
        )
        .init();

    let addr = parse_loopback_addr(env::var("GIW_DESKTOP_ADDR").as_deref().unwrap_or(DEFAULT_ADDR))?;
    let scintilla_url = env::var("GIW_SCINTILLA_DAEMON_URL")
        .unwrap_or_else(|_| DEFAULT_SCINTILLA_URL.to_owned());
    require_loopback_url(&scintilla_url)?;

    let state = AppState {
        token: Arc::from(load_or_create_token(&giw_token_path()?)?),
        scintilla_token: Arc::from(read_scintilla_token(&scintilla_token_path()?)?),
        scintilla_url: Arc::from(scintilla_url),
        client: reqwest::Client::builder().build()?,
        started_at: Instant::now(),
    };

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v1/status", get(status))
        .route("/v1/jobs/dispatch", post(dispatch_job))
        .route("/v1/scintilla/status", get(scintilla_status))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "IndieBuild desktop daemon listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    return Ok(());
}

async fn health() -> &'static str {
    return "ok";
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    let scintilla = fetch_scintilla_status(&state).await?;
    return Ok(Json(StatusResponse {
        product: "indiebuild",
        execution_backend: "scintilla-run",
        uptime_ms: state.started_at.elapsed().as_millis(),
        scintilla,
    }));
}

async fn scintilla_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    return fetch_scintilla_status(&state).await.map(Json);
}

async fn dispatch_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    authorize(&headers, &state)?;

    if let Some(object) = payload.as_object_mut() {
        object.entry("execution_provider").or_insert_with(|| Value::String("indiebuild".to_owned()));
    }

    let response = state
        .client
        .post(format!("{}/v1/invoke", state.scintilla_url.trim_end_matches('/')))
        .bearer_auth(state.scintilla_token.as_ref())
        .json(&payload)
        .send()
        .await
        .map_err(bad_gateway)?;

    let status = response.status();
    let bytes = response.bytes().await.map_err(bad_gateway)?;
    if !status.is_success() {
        return Err((StatusCode::BAD_GATEWAY, format!("Scintilla desktop daemon returned {status}")));
    }
    let value = serde_json::from_slice::<Value>(&bytes)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Scintilla desktop daemon returned non-JSON".to_owned()))?;
    return Ok(Json(value));
}

async fn fetch_scintilla_status(state: &AppState) -> Result<Value, (StatusCode, String)> {
    let response = state
        .client
        .get(format!("{}/v1/status", state.scintilla_url.trim_end_matches('/')))
        .bearer_auth(state.scintilla_token.as_ref())
        .send()
        .await
        .map_err(bad_gateway)?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(bad_gateway)?;
    if !status.is_success() {
        return Err((StatusCode::BAD_GATEWAY, format!("Scintilla desktop daemon returned {status}")));
    }
    return serde_json::from_slice::<Value>(&bytes)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Scintilla desktop daemon returned non-JSON".to_owned()));
}

fn authorize(headers: &HeaderMap, state: &AppState) -> Result<(), (StatusCode, String)> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided == Some(state.token.as_ref()) {
        return Ok(());
    }
    return Err((StatusCode::UNAUTHORIZED, "unauthorized".to_owned()));
}

fn parse_loopback_addr(value: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = value.parse().context("GIW_DESKTOP_ADDR is not a socket address")?;
    if !addr.ip().is_loopback() {
        bail!("GIW_DESKTOP_ADDR must bind to loopback");
    }
    return Ok(addr);
}

fn require_loopback_url(value: &str) -> Result<()> {
    let allowed = value.starts_with("http://127.0.0.1:")
        || value.starts_with("http://localhost:")
        || value.starts_with("http://[::1]:");
    if !allowed {
        bail!("GIW_SCINTILLA_DAEMON_URL must be loopback HTTP");
    }
    return Ok(());
}

fn giw_token_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("GIW_DESKTOP_TOKEN_FILE") {
        return Ok(expand_home(Path::new(&path))?);
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required"))?;
    return Ok(PathBuf::from(home).join(".indiebuild/daemon/token"));
}

fn scintilla_token_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("GIW_SCINTILLA_TOKEN_FILE") {
        return Ok(expand_home(Path::new(&path))?);
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required"))?;
    return Ok(PathBuf::from(home).join(".scintilla/daemon/token"));
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required"))?;
        let suffix = text.trim_start_matches('~').trim_start_matches('/');
        return Ok(PathBuf::from(home).join(suffix));
    }
    return Ok(path.to_path_buf());
}

fn read_scintilla_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read Scintilla daemon token at {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 {
        bail!("Scintilla daemon token is invalid");
    }
    return Ok(token.to_owned());
}

fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(token) = std::fs::read_to_string(path) {
        let token = token.trim();
        if token.len() >= 32 {
            return Ok(token.to_owned());
        }
        bail!("IndieBuild daemon token file is too short");
    }
    let parent = path.parent().ok_or_else(|| anyhow!("token path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    std::fs::write(path, format!("{token}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    return Ok(token);
}

fn bad_gateway(error: impl std::fmt::Display) -> (StatusCode, String) {
    return (StatusCode::BAD_GATEWAY, error.to_string());
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

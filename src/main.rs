use anyhow::{Context as _, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
const MAX_JOB_TIMEOUT_SECS: u64 = 6 * 60 * 60;
const MAX_REPOSITORY_CHARS: usize = 256;
const MAX_REF_CHARS: usize = 256;
const MAX_JOB_NAME_CHARS: usize = 128;

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
    isolation: &'static str,
    worker_reuse: &'static str,
    uptime_ms: u128,
    scintilla: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchJobRequest {
    repository: String,
    git_ref: String,
    workflow: String,
    job: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    execution_os: ExecutionOs,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExecutionOs {
    #[default]
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Serialize)]
struct DispatchJobResponse {
    execution_id: String,
    backend: &'static str,
    ephemeral: bool,
    raw: Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "giw_desktop_daemon=info".into()),
        )
        .init();

    let addr =
        parse_loopback_addr(env::var("GIW_DESKTOP_ADDR").as_deref().unwrap_or(DEFAULT_ADDR))?;
    let scintilla_url =
        env::var("GIW_SCINTILLA_DAEMON_URL").unwrap_or_else(|_| DEFAULT_SCINTILLA_URL.to_owned());
    require_loopback_url(&scintilla_url)?;

    let state = AppState {
        token: Arc::from(load_or_create_token(&giw_token_path()?)?),
        scintilla_token: Arc::from(read_scintilla_token(&scintilla_token_path()?)?),
        scintilla_url: Arc::from(scintilla_url),
        client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()?,
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
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    let scintilla = fetch_scintilla_status(&state).await?;
    Ok(Json(StatusResponse {
        product: "indiebuild",
        execution_backend: "scintilla-run",
        isolation: "ephemeral-per-job",
        worker_reuse: "forbidden",
        uptime_ms: state.started_at.elapsed().as_millis(),
        scintilla,
    }))
}

async fn scintilla_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    fetch_scintilla_status(&state).await.map(Json)
}

async fn dispatch_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DispatchJobRequest>,
) -> Result<Json<DispatchJobResponse>, (StatusCode, String)> {
    authorize(&headers, &state)?;
    validate_dispatch(&request)?;

    let execution_id = format!("giw-{}", Uuid::new_v4().simple());
    let payload = json!({
        "kind": "gha_indie_worker_job",
        "api_version": "indiebuild.scintilla/v1",
        "execution_id": execution_id,
        "execution_provider": "indiebuild",
        "ephemeral": true,
        "reuse_workspace": false,
        "network_mode": "restricted",
        "repository": request.repository,
        "git_ref": request.git_ref,
        "workflow": request.workflow,
        "job": request.job,
        "labels": request.labels,
        "timeout_secs": request.timeout_secs,
        "execution_os": request.execution_os,
    });

    let response = state
        .client
        .post(format!(
            "{}/v1/invoke",
            state.scintilla_url.trim_end_matches('/')
        ))
        .bearer_auth(state.scintilla_token.as_ref())
        .json(&payload)
        .send()
        .await
        .map_err(bad_gateway)?;

    let status = response.status();
    let bytes = response.bytes().await.map_err(bad_gateway)?;
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Scintilla desktop daemon returned {status}"),
        ));
    }
    let raw = serde_json::from_slice::<Value>(&bytes).map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "Scintilla desktop daemon returned non-JSON".to_owned(),
        )
    })?;

    Ok(Json(DispatchJobResponse {
        execution_id,
        backend: "scintilla-run",
        ephemeral: true,
        raw,
    }))
}

fn validate_dispatch(request: &DispatchJobRequest) -> Result<(), (StatusCode, String)> {
    validate_bounded("repository", &request.repository, MAX_REPOSITORY_CHARS)?;
    if !request.repository.contains('/') || request.repository.starts_with('/') {
        return Err((
            StatusCode::BAD_REQUEST,
            "repository must be owner/name".to_owned(),
        ));
    }
    validate_bounded("git_ref", &request.git_ref, MAX_REF_CHARS)?;
    validate_bounded("workflow", &request.workflow, MAX_JOB_NAME_CHARS)?;
    validate_bounded("job", &request.job, MAX_JOB_NAME_CHARS)?;

    if request.timeout_secs == 0 || request.timeout_secs > MAX_JOB_TIMEOUT_SECS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("timeout_secs must be 1..={MAX_JOB_TIMEOUT_SECS}"),
        ));
    }
    if request.labels.len() > 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            "at most 32 runner labels are allowed".to_owned(),
        ));
    }
    for label in &request.labels {
        validate_bounded("label", label, 64)?;
    }
    Ok(())
}

fn validate_bounded(
    name: &str,
    value: &str,
    max_chars: usize,
) -> Result<(), (StatusCode, String)> {
    if value.trim().is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{name} is empty, too long, or contains control characters"),
        ));
    }
    Ok(())
}

fn default_timeout_secs() -> u64 {
    60 * 60
}

async fn fetch_scintilla_status(state: &AppState) -> Result<Value, (StatusCode, String)> {
    let response = state
        .client
        .get(format!(
            "{}/v1/status",
            state.scintilla_url.trim_end_matches('/')
        ))
        .bearer_auth(state.scintilla_token.as_ref())
        .send()
        .await
        .map_err(bad_gateway)?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(bad_gateway)?;
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Scintilla desktop daemon returned {status}"),
        ));
    }
    serde_json::from_slice::<Value>(&bytes).map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "Scintilla desktop daemon returned non-JSON".to_owned(),
        )
    })
}

fn authorize(headers: &HeaderMap, state: &AppState) -> Result<(), (StatusCode, String)> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided == Some(state.token.as_ref()) {
        return Ok(());
    }
    Err((StatusCode::UNAUTHORIZED, "unauthorized".to_owned()))
}

fn parse_loopback_addr(value: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = value
        .parse()
        .context("GIW_DESKTOP_ADDR is not a socket address")?;
    if !addr.ip().is_loopback() {
        bail!("GIW_DESKTOP_ADDR must bind to loopback");
    }
    Ok(addr)
}

fn require_loopback_url(value: &str) -> Result<()> {
    let allowed = value.starts_with("http://127.0.0.1:")
        || value.starts_with("http://localhost:")
        || value.starts_with("http://[::1]:");
    if !allowed {
        bail!("GIW_SCINTILLA_DAEMON_URL must be loopback HTTP");
    }
    Ok(())
}

fn giw_token_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("GIW_DESKTOP_TOKEN_FILE") {
        return expand_home(Path::new(&path));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required"))?;
    Ok(PathBuf::from(home).join(".indiebuild/daemon/token"))
}

fn scintilla_token_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("GIW_SCINTILLA_TOKEN_FILE") {
        return expand_home(Path::new(&path));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required"))?;
    Ok(PathBuf::from(home).join(".scintilla/daemon/token"))
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is required"))?;
        let suffix = text.trim_start_matches('~').trim_start_matches('/');
        return Ok(PathBuf::from(home).join(suffix));
    }
    Ok(path.to_path_buf())
}

fn read_scintilla_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read Scintilla daemon token at {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 {
        bail!("Scintilla daemon token is invalid");
    }
    Ok(token.to_owned())
}

fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(token) = std::fs::read_to_string(path) {
        let token = token.trim();
        if token.len() >= 32 {
            return Ok(token.to_owned());
        }
        bail!("IndieBuild daemon token file is too short");
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("token path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let token = format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    std::fs::write(path, format!("{token}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

fn bad_gateway(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, error.to_string())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

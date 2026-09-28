mod flags;

use anyhow::{Context as _, Result, anyhow, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use uuid::Uuid;

const DEFAULT_ADDR: &str = "127.0.0.1:8770";
const DEFAULT_SCINTILLA_URL: &str = "http://127.0.0.1:8765";
const MAX_JOB_TIMEOUT_SECS: u64 = 6 * 60 * 60;
const MAX_REPOSITORY_CHARS: usize = 256;
const MAX_REF_CHARS: usize = 256;
const MAX_JOB_NAME_CHARS: usize = 128;
const MAX_UPSTREAM_BODY_BYTES: usize = 1024 * 1024;

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
    let env = flags::apply_cli_flags().map_err(anyhow::Error::msg)?;
    let log_filter = env
        .get("RUST_LOG")
        .map(String::as_str)
        .unwrap_or("giw_desktop_daemon=info");
    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .init();

    let addr = parse_loopback_addr(
        env.get("GIW_DESKTOP_ADDR")
            .map(String::as_str)
            .unwrap_or(DEFAULT_ADDR),
    )?;
    let scintilla_url = env
        .get("GIW_SCINTILLA_DAEMON_URL")
        .cloned()
        .unwrap_or_else(|| DEFAULT_SCINTILLA_URL.to_owned());
    require_loopback_url(&scintilla_url)?;

    let state = AppState {
        token: Arc::from(load_or_create_token(&giw_token_path(&env)?)?),
        scintilla_token: Arc::from(read_scintilla_token(&scintilla_token_path(&env)?)?),
        scintilla_url: Arc::from(scintilla_url),
        client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        started_at: Instant::now(),
    };

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/v1/status", get(status))
        .route("/v1/jobs/dispatch", post(dispatch_job))
        .route("/v1/scintilla/status", get(scintilla_status))
        .layer(DefaultBodyLimit::max(64 * 1024))
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
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Scintilla desktop daemon returned {status}"),
        ));
    }
    let raw = read_bounded_json(response, MAX_UPSTREAM_BODY_BYTES)
        .await
        .map_err(bad_gateway)?;

    Ok(Json(DispatchJobResponse {
        execution_id,
        backend: "scintilla-run",
        ephemeral: true,
        raw,
    }))
}

fn validate_dispatch(request: &DispatchJobRequest) -> Result<(), (StatusCode, String)> {
    validate_bounded("repository", &request.repository, MAX_REPOSITORY_CHARS)?;
    validate_repository(&request.repository)?;
    validate_git_ref(&request.git_ref)?;
    validate_pathish("workflow", &request.workflow, MAX_JOB_NAME_CHARS)?;
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
        validate_label(label)?;
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

fn validate_repository(value: &str) -> Result<(), (StatusCode, String)> {
    let mut parts = value.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || !safe_repo_segment(owner, 100)
        || !safe_repo_segment(repo, 100)
        || value.contains("..")
    {
        return Err((StatusCode::BAD_REQUEST, "repository must be a safe owner/name identifier".to_owned()));
    }
    Ok(())
}

fn safe_repo_segment(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_git_ref(value: &str) -> Result<(), (StatusCode, String)> {
    validate_bounded("git_ref", value, MAX_REF_CHARS)?;
    if value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.contains("..")
        || value.contains("@{")
        || value.contains('\\')
        || value.chars().any(char::is_whitespace)
    {
        return Err((StatusCode::BAD_REQUEST, "git_ref contains unsafe ref syntax".to_owned()));
    }
    Ok(())
}

fn validate_pathish(
    name: &str,
    value: &str,
    max_chars: usize,
) -> Result<(), (StatusCode, String)> {
    validate_bounded(name, value, max_chars)?;
    if value.starts_with('/')
        || value.contains("..")
        || value.contains('\\')
        || value.chars().any(char::is_whitespace)
    {
        return Err((StatusCode::BAD_REQUEST, format!("{name} contains unsafe path syntax")));
    }
    Ok(())
}

fn validate_label(value: &str) -> Result<(), (StatusCode, String)> {
    validate_bounded("label", value, 64)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err((StatusCode::BAD_REQUEST, "label contains unsupported characters".to_owned()));
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
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Scintilla desktop daemon returned {status}"),
        ));
    }
    read_bounded_json(response, MAX_UPSTREAM_BODY_BYTES)
        .await
        .map_err(bad_gateway)
}

fn authorize(headers: &HeaderMap, state: &AppState) -> Result<(), (StatusCode, String)> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if provided
        .is_some_and(|token| constant_time_eq(token.as_bytes(), state.token.as_bytes()))
    {
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
    let url = reqwest::Url::parse(value).context("GIW_SCINTILLA_DAEMON_URL must be a valid URL")?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("GIW_SCINTILLA_DAEMON_URL must be credential-free loopback HTTP");
    }
    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "::1") {
        bail!("GIW_SCINTILLA_DAEMON_URL must use a literal loopback address");
    }
    Ok(())
}

fn giw_token_path(env: &flags::EnvMap) -> Result<PathBuf> {
    if let Some(path) = env.get("GIW_DESKTOP_TOKEN_FILE") {
        return expand_home(Path::new(path));
    }
    Ok(home_dir()?.join(".indiebuild/daemon/token"))
}

fn scintilla_token_path(env: &flags::EnvMap) -> Result<PathBuf> {
    if let Some(path) = env.get("GIW_SCINTILLA_TOKEN_FILE") {
        return expand_home(Path::new(path));
    }
    Ok(home_dir()?.join(".scintilla/daemon/token"))
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        let suffix = text.trim_start_matches('~').trim_start_matches('/');
        return Ok(home_dir()?.join(suffix));
    }
    Ok(path.to_path_buf())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME/USERPROFILE is required"))
}

fn read_scintilla_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read Scintilla daemon token at {}", path.display()))?;
    let token = token.trim();
    if token.len() < 32 || token.len() > 4096 || token.chars().any(char::is_whitespace) {
        bail!("Scintilla daemon token is invalid");
    }
    Ok(token.to_owned())
}

fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(token) = std::fs::read_to_string(path) {
        let token = token.trim();
        if token.len() >= 32 && token.len() <= 4096 && !token.chars().any(char::is_whitespace) {
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

async fn read_bounded_json(mut response: reqwest::Response, max_bytes: usize) -> Result<Value> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!("Scintilla response exceeds {max_bytes} bytes");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            bail!("Scintilla response exceeds {max_bytes} bytes");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("Scintilla response is not JSON")
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn bad_gateway(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, error.to_string())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}


#[cfg(test)]
mod tests {
    use super::*;

    fn valid_request() -> DispatchJobRequest {
        DispatchJobRequest {
            repository: "owner/repo".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            workflow: "ci.yml".to_owned(),
            job: "test".to_owned(),
            labels: vec!["linux".to_owned()],
            timeout_secs: 3600,
            execution_os: ExecutionOs::Linux,
        }
    }

    #[test]
    fn accepts_bounded_ephemeral_job_intent() {
        assert!(validate_dispatch(&valid_request()).is_ok());
    }

    #[test]
    fn rejects_unbounded_timeout() {
        let mut request = valid_request();
        request.timeout_secs = MAX_JOB_TIMEOUT_SECS + 1;
        assert!(validate_dispatch(&request).is_err());
    }

    #[test]
    fn rejects_non_repository_identifier() {
        let mut request = valid_request();
        request.repository = "not-a-repository".to_owned();
        assert!(validate_dispatch(&request).is_err());
    }

    #[test]
    fn rejects_unsafe_repository_and_ref() {
        let mut request = valid_request();
        request.repository = "owner/repo/extra".to_owned();
        assert!(validate_dispatch(&request).is_err());

        let mut request = valid_request();
        request.git_ref = "refs/heads/../escape".to_owned();
        assert!(validate_dispatch(&request).is_err());
    }

    #[test]
    fn bearer_comparison_is_length_and_content_sensitive() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"short", b"shorter"));
    }
}

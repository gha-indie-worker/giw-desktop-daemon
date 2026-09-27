#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    sync::Mutex,
};
use uuid::Uuid;

const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_BIND: &str = "127.0.0.1:18440";
const DEFAULT_MANIFEST_FILE: &str = ".giw-desktop.yaml";

#[derive(Debug, Clone, Deserialize)]
struct DesktopManifest {
    #[serde(default = "default_manifest_version")]
    version: u32,
    #[serde(default)]
    services: Vec<ServiceSpec>,
    tunnel: Option<TunnelSpec>,
    update: Option<UpdateSpec>,
}

#[derive(Debug, Clone, Deserialize)]
struct ServiceSpec {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    working_dir: Option<PathBuf>,
    #[serde(default)]
    env_passthrough: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    autostart: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct TunnelSpec {
    name: String,
    hostname: Option<String>,
    service_url: String,
    #[serde(default)]
    autostart: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct UpdateSpec {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ProcessStatus {
    name: String,
    running: bool,
    pid: Option<u32>,
}

#[derive(Debug, Serialize)]
struct DaemonStatus {
    protocol_version: u32,
    mode: &'static str,
    bind: String,
    manifest_version: u32,
    services: Vec<ProcessStatus>,
    tunnel: Option<TunnelStatus>,
    keep_awake: bool,
}

#[derive(Debug, Serialize)]
struct TunnelStatus {
    name: String,
    hostname: Option<String>,
    service_url: String,
    running: bool,
    pid: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct KeepAwakeRequest {
    enabled: bool,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        };
    }

    fn unauthorized() -> Self {
        return Self {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid local desktop bearer token".to_string(),
        };
    }

    fn not_found(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        };
    }

    fn conflict(message: impl Into<String>) -> Self {
        return Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        };
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        return Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        };
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        return (
            self.status,
            Json(json!({
                "error": self.message,
                "protocol_version": PROTOCOL_VERSION,
            })),
        )
            .into_response();
    }
}

struct AppState {
    bind: SocketAddr,
    token: String,
    manifest: DesktopManifest,
    processes: Mutex<HashMap<String, Child>>,
    tunnel: Mutex<Option<Child>>,
    keep_awake: Mutex<Option<Child>>,
}

fn default_manifest_version() -> u32 {
    return 1;
}

fn home_dir() -> Result<PathBuf> {
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home));
    }

    if let Some(profile) = env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(profile));
    }

    bail!("HOME or USERPROFILE must be set");
}

fn default_token_path() -> Result<PathBuf> {
    return Ok(home_dir()?.join(".giw").join("desktop").join("token"));
}

fn manifest_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("GIW_DESKTOP_MANIFEST") {
        return Ok(PathBuf::from(path));
    }

    let local = PathBuf::from(DEFAULT_MANIFEST_FILE);
    if local.is_file() {
        return Ok(local);
    }

    return Ok(home_dir()?
        .join(".giw")
        .join("desktop")
        .join("manifest.yaml"));
}

fn validate_loopback_bind(raw: &str) -> Result<SocketAddr> {
    let bind: SocketAddr = raw
        .parse()
        .with_context(|| format!("GIW_DESKTOP_BIND must be a socket address; got {raw:?}"))?;

    if !bind.ip().is_loopback() {
        bail!(
            "GIW desktop daemon must bind to loopback; got {}",
            bind.ip()
        );
    }

    return Ok(bind);
}

fn validate_manifest(manifest: &DesktopManifest) -> Result<()> {
    if manifest.version != 1 {
        bail!(
            "unsupported GIW desktop manifest version {}; expected 1",
            manifest.version
        );
    }

    let mut names = std::collections::HashSet::new();
    for service in &manifest.services {
        if service.name.trim().is_empty() {
            bail!("service name may not be empty");
        }
        if service.command.trim().is_empty() {
            bail!("service {} has an empty command", service.name);
        }
        if !names.insert(service.name.clone()) {
            bail!("duplicate service name {}", service.name);
        }
    }

    if let Some(tunnel) = &manifest.tunnel {
        if tunnel.name.trim().is_empty() {
            bail!("tunnel name may not be empty");
        }
        validate_loopback_service_url(&tunnel.service_url)?;
    }

    return Ok(());
}

fn validate_loopback_service_url(raw: &str) -> Result<()> {
    let without_scheme = raw
        .strip_prefix("http://")
        .context("desktop tunnel service_url must use http://")?;
    let authority = without_scheme.split('/').next().unwrap_or_default();
    let host = authority
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_matches(['[', ']']);

    let loopback = match host {
        "localhost" => true,
        _ => host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
    };

    if !loopback {
        bail!("desktop tunnel service_url must target loopback; got {raw:?}");
    }

    return Ok(());
}

fn load_manifest(path: &Path) -> Result<DesktopManifest> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read desktop manifest at {}", path.display()))?;
    let manifest: DesktopManifest = serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse desktop manifest at {}", path.display()))?;
    validate_manifest(&manifest)?;
    return Ok(manifest);
}

fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(raw) = fs::read_to_string(path) {
        let token = raw.trim();
        if token.len() < 32 || token.chars().any(char::is_whitespace) {
            bail!(
                "existing GIW desktop token is malformed at {}",
                path.display()
            );
        }
        return Ok(token.to_string());
    }

    let parent = path
        .parent()
        .context("GIW desktop token path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let token = format!(
        "{}{}{}{}",
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4()
    );
    fs::write(path, format!("{token}\n"))
        .with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    return Ok(token);
}

fn require_auth(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    let expected = format!("Bearer {}", state.token);
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    if supplied != Some(expected.as_str()) {
        return Err(ApiError::unauthorized());
    }

    return Ok(());
}

fn service_spec<'a>(state: &'a AppState, name: &str) -> Result<&'a ServiceSpec, ApiError> {
    if let Some(service) = state
        .manifest
        .services
        .iter()
        .find(|service| service.name == name)
    {
        return Ok(service);
    }

    return Err(ApiError::not_found(format!(
        "unknown desktop service {name:?}"
    )));
}

fn command_for_service(service: &ServiceSpec) -> Command {
    let mut command = Command::new(&service.command);
    command.args(&service.args);

    if let Some(working_dir) = &service.working_dir {
        command.current_dir(working_dir);
    }

    if !service.env_passthrough.is_empty() || !service.env.is_empty() {
        command.env_clear();
        for baseline in ["PATH", "HOME", "USERPROFILE", "TMPDIR", "TEMP", "TMP"] {
            if let Some(value) = env::var_os(baseline) {
                command.env(baseline, value);
            }
        }
        for key in &service.env_passthrough {
            if let Some(value) = env::var_os(key) {
                command.env(key, value);
            }
        }
        for (key, value) in &service.env {
            command.env(key, value);
        }
    }

    command.stdin(Stdio::null());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    return command;
}

async fn process_statuses(state: &AppState) -> Vec<ProcessStatus> {
    let mut processes = state.processes.lock().await;
    let mut statuses = Vec::with_capacity(state.manifest.services.len());

    for service in &state.manifest.services {
        let (running, pid) = match processes.get_mut(&service.name) {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => (false, None),
                Ok(None) => (true, child.id()),
                Err(_) => (false, child.id()),
            },
            None => (false, None),
        };
        statuses.push(ProcessStatus {
            name: service.name.clone(),
            running,
            pid,
        });
    }

    processes.retain(|_, child| child.try_wait().ok().flatten().is_none());
    return statuses;
}

async fn start_service_internal(state: &AppState, name: &str) -> Result<ProcessStatus, ApiError> {
    let spec = service_spec(state, name)?.clone();
    let mut processes = state.processes.lock().await;

    if let Some(child) = processes.get_mut(name) {
        match child.try_wait() {
            Ok(None) => {
                return Err(ApiError::conflict(format!(
                    "service {name:?} is already running"
                )));
            }
            Ok(Some(_)) | Err(_) => {
                processes.remove(name);
            }
        }
    }

    let mut command = command_for_service(&spec);
    let child = command
        .spawn()
        .map_err(|error| ApiError::internal(format!("failed to start {name:?}: {error}")))?;
    let pid = child.id();
    processes.insert(name.to_string(), child);

    return Ok(ProcessStatus {
        name: name.to_string(),
        running: true,
        pid,
    });
}

async fn stop_service_internal(state: &AppState, name: &str) -> Result<ProcessStatus, ApiError> {
    let _ = service_spec(state, name)?;
    let mut processes = state.processes.lock().await;

    if let Some(mut child) = processes.remove(name) {
        child
            .kill()
            .await
            .map_err(|error| ApiError::internal(format!("failed to stop {name:?}: {error}")))?;
    }

    return Ok(ProcessStatus {
        name: name.to_string(),
        running: false,
        pid: None,
    });
}

async fn healthz() -> Json<Value> {
    return Json(json!({
        "ok": true,
        "service": "giw-desktop-daemon",
        "protocol_version": PROTOCOL_VERSION,
    }));
}

async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<DaemonStatus>, ApiError> {
    require_auth(&headers, &state)?;
    let services = process_statuses(&state).await;

    let (tunnel_running, tunnel_pid) = {
        let mut tunnel = state.tunnel.lock().await;
        match tunnel.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => (true, child.id()),
                Ok(Some(_)) | Err(_) => {
                    *tunnel = None;
                    (false, None)
                }
            },
            None => (false, None),
        }
    };

    let tunnel = state.manifest.tunnel.as_ref().map(|spec| TunnelStatus {
        name: spec.name.clone(),
        hostname: spec.hostname.clone(),
        service_url: spec.service_url.clone(),
        running: tunnel_running,
        pid: tunnel_pid,
    });

    let keep_awake = state.keep_awake.lock().await.is_some();
    return Ok(Json(DaemonStatus {
        protocol_version: PROTOCOL_VERSION,
        mode: "desktop-single-host",
        bind: state.bind.to_string(),
        manifest_version: state.manifest.version,
        services,
        tunnel,
        keep_awake,
    }));
}

async fn list_processes(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProcessStatus>>, ApiError> {
    require_auth(&headers, &state)?;
    return Ok(Json(process_statuses(&state).await));
}

async fn start_process(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ProcessStatus>, ApiError> {
    require_auth(&headers, &state)?;
    return Ok(Json(start_service_internal(&state, &name).await?));
}

async fn stop_process(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ProcessStatus>, ApiError> {
    require_auth(&headers, &state)?;
    return Ok(Json(stop_service_internal(&state, &name).await?));
}

async fn restart_process(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<ProcessStatus>, ApiError> {
    require_auth(&headers, &state)?;
    let _ = stop_service_internal(&state, &name).await?;
    return Ok(Json(start_service_internal(&state, &name).await?));
}

async fn start_tunnel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<TunnelStatus>, ApiError> {
    require_auth(&headers, &state)?;
    let spec = state
        .manifest
        .tunnel
        .as_ref()
        .ok_or_else(|| ApiError::bad_request("desktop manifest has no Cloudflare tunnel"))?
        .clone();

    let mut tunnel = state.tunnel.lock().await;
    if let Some(child) = tunnel.as_mut() {
        if child.try_wait().map_err(ApiError::internal)?.is_none() {
            return Err(ApiError::conflict("Cloudflare tunnel is already running"));
        }
        *tunnel = None;
    }

    let mut command = Command::new("cloudflared");
    command.args([
        "tunnel",
        "--url",
        spec.service_url.as_str(),
        "run",
        spec.name.as_str(),
    ]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    let child = command
        .spawn()
        .map_err(|error| ApiError::internal(format!("failed to start cloudflared: {error}")))?;
    let pid = child.id();
    *tunnel = Some(child);

    return Ok(Json(TunnelStatus {
        name: spec.name,
        hostname: spec.hostname,
        service_url: spec.service_url,
        running: true,
        pid,
    }));
}

async fn stop_tunnel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let mut tunnel = state.tunnel.lock().await;
    if let Some(mut child) = tunnel.take() {
        child.kill().await.map_err(ApiError::internal)?;
    }
    return Ok(Json(json!({"running": false})));
}

fn keep_awake_command() -> Result<Command, ApiError> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("caffeinate");
        command.args(["-d", "-i", "-m", "-s", "-u"]);
        return Ok(command);
    }

    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("systemd-inhibit");
        command.args([
            "--what=sleep",
            "--mode=block",
            "--why=IndieBuild desktop worker is active",
            "sleep",
            "infinity",
        ]);
        return Ok(command);
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return Err(ApiError::bad_request(
            "keep-awake is not implemented on this operating system yet",
        ));
    }
}

async fn set_keep_awake(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<KeepAwakeRequest>,
) -> Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let mut keep_awake = state.keep_awake.lock().await;

    if request.enabled {
        if keep_awake.is_none() {
            let mut command = keep_awake_command()?;
            command.stdin(Stdio::null());
            command.stdout(Stdio::null());
            command.stderr(Stdio::inherit());
            *keep_awake = Some(command.spawn().map_err(ApiError::internal)?);
        }
    } else if let Some(mut child) = keep_awake.take() {
        child.kill().await.map_err(ApiError::internal)?;
    }

    return Ok(Json(json!({"enabled": request.enabled})));
}

async fn apply_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let update = state
        .manifest
        .update
        .as_ref()
        .ok_or_else(|| ApiError::not_found("desktop manifest has no update plan"))?;

    let status = Command::new(&update.command)
        .args(&update.args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(ApiError::internal)?;

    if !status.success() {
        return Err(ApiError::internal(format!(
            "update plan exited with {status}"
        )));
    }

    return Ok(Json(json!({"updated": true})));
}

async fn reconcile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let autostart_names: Vec<String> = state
        .manifest
        .services
        .iter()
        .filter(|service| service.autostart)
        .map(|service| service.name.clone())
        .collect();

    let mut started = Vec::new();
    for name in autostart_names {
        match start_service_internal(&state, &name).await {
            Ok(status) => started.push(status.name),
            Err(error) if error.status == StatusCode::CONFLICT => {}
            Err(error) => return Err(error),
        }
    }

    let tunnel_started = if state
        .manifest
        .tunnel
        .as_ref()
        .map(|tunnel| tunnel.autostart)
        .unwrap_or(false)
    {
        match start_tunnel(State(state.clone()), headers.clone()).await {
            Ok(_) => true,
            Err(error) if error.status == StatusCode::CONFLICT => false,
            Err(error) => return Err(error),
        }
    } else {
        false
    };

    return Ok(Json(json!({
        "reconciled": true,
        "started": started,
        "tunnel_started": tunnel_started,
    })));
}

fn router(state: Arc<AppState>) -> Router {
    return Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/status", get(status))
        .route("/v1/processes", get(list_processes))
        .route("/v1/processes/{name}/start", post(start_process))
        .route("/v1/processes/{name}/stop", post(stop_process))
        .route("/v1/processes/{name}/restart", post(restart_process))
        .route("/v1/tunnel/start", post(start_tunnel))
        .route("/v1/tunnel/stop", post(stop_tunnel))
        .route("/v1/power/keep-awake", post(set_keep_awake))
        .route("/v1/updates/apply", post(apply_update))
        .route("/v1/reconcile", post(reconcile))
        .with_state(state);
}

#[tokio::main]
async fn main() -> Result<()> {
    let bind_raw = env::var("GIW_DESKTOP_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let bind = validate_loopback_bind(&bind_raw)?;
    let manifest_path = manifest_path()?;
    let manifest = load_manifest(&manifest_path)?;
    let token_path = env::var_os("GIW_DESKTOP_TOKEN_FILE")
        .map(PathBuf::from)
        .unwrap_or(default_token_path()?);
    let token = load_or_create_token(&token_path)?;

    let state = Arc::new(AppState {
        bind,
        token,
        manifest,
        processes: Mutex::new(HashMap::new()),
        tunnel: Mutex::new(None),
        keep_awake: Mutex::new(None),
    });

    eprintln!(
        "giw-desktop-daemon listening on http://{bind}; manifest={}; token_file={}",
        manifest_path.display(),
        token_path.display()
    );

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind GIW desktop daemon to {bind}"))?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("GIW desktop daemon failed")?;

    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_bind_must_be_loopback() {
        assert!(validate_loopback_bind("127.0.0.1:18440").is_ok());
        assert!(validate_loopback_bind("0.0.0.0:18440").is_err());
    }

    #[test]
    fn tunnel_origin_must_be_loopback_http() {
        assert!(validate_loopback_service_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_loopback_service_url("https://127.0.0.1:8080").is_err());
        assert!(validate_loopback_service_url("http://example.com:8080").is_err());
    }

    #[test]
    fn manifest_rejects_duplicate_service_names() {
        let service = ServiceSpec {
            name: "worker".to_string(),
            command: "worker".to_string(),
            args: Vec::new(),
            working_dir: None,
            env_passthrough: Vec::new(),
            env: BTreeMap::new(),
            autostart: false,
        };
        let manifest = DesktopManifest {
            version: 1,
            services: vec![service.clone(), service],
            tunnel: None,
            update: None,
        };
        assert!(validate_manifest(&manifest).is_err());
    }
}

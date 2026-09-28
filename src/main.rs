#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::{HeaderMap, StatusCode, Uri, header},
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
const MAX_HTTP_REQUEST_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 4096;
const MAX_TOKEN_FILE_BYTES: u64 = (MAX_TOKEN_BYTES + 1) as u64;
const MAX_SERVICES: usize = 64;
const MAX_NAME_CHARS: usize = 256;
const MAX_COMMAND_CHARS: usize = 4096;
const MAX_ARGS: usize = 128;
const MAX_ARG_CHARS: usize = 8192;
const MAX_ENV_ENTRIES: usize = 128;
const MAX_ENV_VALUE_CHARS: usize = 16_384;
const MAX_URL_CHARS: usize = 2048;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopManifest {
    #[serde(default = "default_manifest_version")]
    version: u32,
    #[serde(default)]
    services: Vec<ServiceSpec>,
    tunnel: Option<TunnelSpec>,
    update: Option<UpdateSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct TunnelSpec {
    name: String,
    hostname: Option<String>,
    service_url: String,
    #[serde(default)]
    autostart: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    if manifest.services.len() > MAX_SERVICES {
        bail!("desktop manifest may declare at most {MAX_SERVICES} services");
    }

    let mut names = HashSet::new();
    for service in &manifest.services {
        if !valid_service_name(&service.name) {
            bail!("invalid desktop service name {:?}", service.name);
        }
        validate_command(&service.command, &service.args, &format!("service {}", service.name))?;
        if let Some(working_dir) = &service.working_dir {
            let value = working_dir.to_string_lossy();
            if value.is_empty() || value.chars().count() > MAX_COMMAND_CHARS {
                bail!("service {} working_dir is invalid", service.name);
            }
        }
        validate_env_passthrough(&service.env_passthrough, &format!("service {}", service.name))?;
        if service.env.len() > MAX_ENV_ENTRIES {
            bail!("service {} declares too many literal environment entries", service.name);
        }
        for (key, value) in &service.env {
            if !valid_env_name(key) {
                bail!("service {} has invalid environment name {key:?}", service.name);
            }
            if looks_secret_bearing(key) {
                bail!(
                    "service {} environment {key:?} looks secret-bearing; use env_passthrough or the encrypted secret boundary instead of a literal",
                    service.name
                );
            }
            if value.chars().count() > MAX_ENV_VALUE_CHARS || value.chars().any(char::is_control) {
                bail!("service {} environment {key:?} is invalid or too large", service.name);
            }
        }
        if !names.insert(service.name.clone()) {
            bail!("duplicate service name {}", service.name);
        }
    }

    if let Some(tunnel) = &manifest.tunnel {
        if tunnel.name.is_empty() || tunnel.name.chars().count() > MAX_NAME_CHARS {
            bail!("tunnel name is invalid");
        }
        if tunnel
            .hostname
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.chars().count() > 253)
        {
            bail!("tunnel hostname is invalid");
        }
        validate_loopback_service_url(&tunnel.service_url)?;
    }

    if let Some(update) = &manifest.update {
        validate_command(&update.command, &update.args, "update plan")?;
    }

    return Ok(());
}

fn validate_command(command: &str, args: &[String], context: &str) -> Result<()> {
    if command.trim().is_empty()
        || command.chars().count() > MAX_COMMAND_CHARS
        || command.chars().any(char::is_control)
    {
        bail!("{context} command is invalid");
    }
    if args.len() > MAX_ARGS {
        bail!("{context} has too many arguments");
    }
    for arg in args {
        if arg.chars().count() > MAX_ARG_CHARS || arg.chars().any(|character| character == '\0') {
            bail!("{context} has an invalid or oversized argument");
        }
    }
    return Ok(());
}

fn validate_env_passthrough(values: &[String], context: &str) -> Result<()> {
    if values.len() > MAX_ENV_ENTRIES {
        bail!("{context} declares too many env_passthrough entries");
    }
    let mut seen = HashSet::new();
    for key in values {
        if !valid_env_name(key) {
            bail!("{context} has invalid env_passthrough name {key:?}");
        }
        if !seen.insert(key.as_str()) {
            bail!("{context} repeats env_passthrough name {key:?}");
        }
    }
    return Ok(());
}

fn valid_service_name(value: &str) -> bool {
    if value.is_empty() || value.chars().count() > MAX_NAME_CHARS {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    return chars
        .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || matches!(character, '.' | '_' | '-'));
}

fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !first.is_ascii_uppercase() {
        return false;
    }
    return chars.all(|character| character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit());
}

fn looks_secret_bearing(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    return [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PRIVATE",
        "CREDENTIAL",
        "API_KEY",
        "DATABASE_URL",
        "GH_PAT",
    ]
    .iter()
    .any(|marker| upper.contains(marker));
}

fn validate_loopback_service_url(raw: &str) -> Result<()> {
    if raw.chars().count() > MAX_URL_CHARS {
        bail!("desktop tunnel service_url is too large");
    }
    let uri: Uri = raw
        .parse()
        .with_context(|| format!("desktop tunnel service_url is not a valid URI: {raw:?}"))?;
    if uri.scheme_str() != Some("http") {
        bail!("desktop tunnel service_url must use http://");
    }
    let authority = uri
        .authority()
        .context("desktop tunnel service_url must include an authority")?;
    if authority.as_str().contains('@') {
        bail!("desktop tunnel service_url must not contain credentials/userinfo");
    }
    if authority.port_u16().is_none() {
        bail!("desktop tunnel service_url must include an explicit port");
    }
    if uri.query().is_some() || !matches!(uri.path(), "" | "/") {
        bail!("desktop tunnel service_url must be an unambiguous loopback origin without query or base path");
    }
    let host = uri
        .host()
        .context("desktop tunnel service_url must include a host")?;
    let ip: IpAddr = host
        .parse()
        .with_context(|| "desktop tunnel service_url host must be a literal loopback IP")?;
    if !ip.is_loopback() {
        bail!("desktop tunnel service_url must target a literal loopback IP; got {raw:?}");
    }
    return Ok(());
}

fn load_manifest(path: &Path) -> Result<DesktopManifest> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat desktop manifest at {}", path.display()))?;
    if metadata.len() > 1024 * 1024 {
        bail!("desktop manifest at {} exceeds 1 MiB", path.display());
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read desktop manifest at {}", path.display()))?;
    let manifest: DesktopManifest = serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse desktop manifest at {}", path.display()))?;
    validate_manifest(&manifest)?;
    return Ok(manifest);
}

fn read_existing_token(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to stat {}", path.display()));
        }
    };

    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("GIW desktop token path must be a regular non-symlink file: {}", path.display());
    }
    if metadata.len() == 0 || metadata.len() > MAX_TOKEN_FILE_BYTES {
        bail!("GIW desktop token file has invalid size at {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("GIW desktop token file must be owner-private (0600 or stricter): {}", path.display());
        }
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read daemon token at {}", path.display()))?;
    let token = raw.trim();
    if token.len() < 32 || token.len() > MAX_TOKEN_BYTES || token.chars().any(char::is_whitespace) {
        bail!("existing GIW desktop token is malformed at {}", path.display());
    }
    return Ok(Some(token.to_string()));
}

fn load_or_create_token(path: &Path) -> Result<String> {
    if let Some(token) = read_existing_token(path)? {
        return Ok(token);
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
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.open(path) {
        Ok(mut file) => {
            file.write_all(format!("{token}\n").as_bytes())
                .with_context(|| format!("failed to write {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to sync {}", path.display()))?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return read_existing_token(path)?.context("token path appeared concurrently but was not admissible");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to create {}", path.display()));
        }
    }

    return Ok(token);
}

fn constant_time_eq(expected: &[u8], presented: &[u8]) -> bool {
    let max = expected.len().max(presented.len());
    let mut diff = (expected.len() ^ presented.len()) as u64;
    for index in 0..max {
        let left = expected.get(index).copied().unwrap_or_default();
        let right = presented.get(index).copied().unwrap_or_default();
        diff |= u64::from(left ^ right);
    }
    return diff == 0;
}

fn require_auth(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    let expected = format!("Bearer {}", state.token);
    let supplied = headers.get(header::AUTHORIZATION).map(|value| value.as_bytes());

    if !supplied.is_some_and(|value| constant_time_eq(expected.as_bytes(), value)) {
        return Err(ApiError::unauthorized());
    }

    return Ok(());
}

fn service_spec<'a>(state: &'a AppState, name: &str) -> Result<&'a ServiceSpec, ApiError> {
    if !valid_service_name(name) {
        return Err(ApiError::bad_request("invalid desktop service name"));
    }
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

fn apply_minimal_environment(command: &mut Command) {
    command.env_clear();
    for baseline in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TEMP",
        "TMP",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(value) = env::var_os(baseline) {
            command.env(baseline, value);
        }
    }
}

fn command_for_service(service: &ServiceSpec) -> Command {
    let mut command = Command::new(&service.command);
    command.args(&service.args);

    if let Some(working_dir) = &service.working_dir {
        command.current_dir(working_dir);
    }

    apply_minimal_environment(&mut command);
    for key in &service.env_passthrough {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in &service.env {
        command.env(key, value);
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
    apply_minimal_environment(&mut command);
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
        apply_minimal_environment(&mut command);
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
        apply_minimal_environment(&mut command);
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

    let mut command = Command::new(&update.command);
    command.args(&update.args);
    apply_minimal_environment(&mut command);
    let status = command
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

async fn shutdown_children(state: &AppState) {
    let children = {
        let mut processes = state.processes.lock().await;
        processes.drain().map(|(_, child)| child).collect::<Vec<_>>()
    };
    for mut child in children {
        let _ = child.kill().await;
    }

    if let Some(mut child) = state.tunnel.lock().await.take() {
        let _ = child.kill().await;
    }
    if let Some(mut child) = state.keep_awake.lock().await.take() {
        let _ = child.kill().await;
    }
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
        .layer(DefaultBodyLimit::max(MAX_HTTP_REQUEST_BYTES))
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
    let serve_result = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    shutdown_children(&state).await;
    serve_result.context("GIW desktop daemon failed")?;

    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_bind_must_be_loopback() {
        assert!(validate_loopback_bind("127.0.0.1:18440").is_ok());
        assert!(validate_loopback_bind("[::1]:18440").is_ok());
        assert!(validate_loopback_bind("0.0.0.0:18440").is_err());
    }

    #[test]
    fn tunnel_origin_must_be_literal_loopback_http() {
        assert!(validate_loopback_service_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_loopback_service_url("http://[::1]:8080").is_ok());
        assert!(validate_loopback_service_url("https://127.0.0.1:8080").is_err());
        assert!(validate_loopback_service_url("http://localhost:8080").is_err());
        assert!(validate_loopback_service_url("http://user@127.0.0.1:8080").is_err());
        assert!(validate_loopback_service_url("http://127.0.0.1:8080/path").is_err());
        assert!(validate_loopback_service_url("http://127.0.0.1:8080?x=1").is_err());
        assert!(validate_loopback_service_url("http://127.0.0.1").is_err());
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

    #[test]
    fn manifest_rejects_secret_bearing_literal_environment() {
        let mut env = BTreeMap::new();
        env.insert("API_TOKEN".to_string(), "not-a-real-secret".to_string());
        let service = ServiceSpec {
            name: "worker".to_string(),
            command: "worker".to_string(),
            args: Vec::new(),
            working_dir: None,
            env_passthrough: Vec::new(),
            env,
            autostart: false,
        };
        let manifest = DesktopManifest {
            version: 1,
            services: vec![service],
            tunnel: None,
            update: None,
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn token_compare_rejects_mismatch_and_length_change() {
        assert!(constant_time_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer abd"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer abc0"));
    }

    #[cfg(unix)]
    #[test]
    fn created_token_is_owner_private_and_regular() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("token");
        let token = load_or_create_token(&path).expect("token creation");
        assert!(token.len() >= 32);
        let metadata = fs::symlink_metadata(&path).expect("metadata");
        assert!(metadata.file_type().is_file());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o077, 0);
        assert_eq!(load_or_create_token(&path).expect("token reload"), token);
    }
}

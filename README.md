# giw-desktop-daemon

`giw-desktop-daemon` is the machine-local control plane for IndieBuild desktop/self-hosted execution.
It is the **only** process that mutates local runtime state. `giw-desktop-cli`,
`gha-indie-worker-flutter`, and `gha-indie-worker-desktop-app.rs` are peer clients of this daemon;
they do not launch workers, `cloudflared`, keep-awake helpers, or update commands directly.

## Control-plane topology

```text
indiebuild.dev / GitHub
        |
        | cloud work / leases / webhooks
        v
+-------------------------------+
| developer laptop / workstation|
|                               |
|  Flutter UI ----+             |
|  Rust UI -------+--> daemon --+--> desktop manifest
|  giw CLI -------+             |       (desired state)
|                               |
|                 +------------+--> build server / workers
|                 +------------+--> Cloudflare Tunnel
|                 +------------+--> keep-awake helper
|                 +------------+--> pinned update plan
+-------------------------------+
```

Production `gha-indie-worker-infra` remains authoritative for hosted/cloud deployment. The desktop
family is deliberately single-host and single-tenant; it is not a miniature Kubernetes cluster.

## Security boundary

- Control binds to `127.0.0.1:18440` by default and rejects non-loopback binds.
- `GET /healthz` is unauthenticated; every `/v1/*` endpoint requires the locally generated bearer token.
- The token lives at `~/.giw/desktop/token` by default. Existing tokens must be bounded regular non-symlink
  files; Unix group/other permissions are rejected. New Unix token files are created atomically owner-only
  rather than written through a check-then-create race.
- Bearer comparison uses a fixed-shape comparison instead of ordinary early-exit string equality.
- API clients can only name bounded single-segment services declared in the local desktop manifest. They
  cannot submit shell text, executable paths, argv, environment values, Cloudflare credentials, or arbitrary
  update commands.
- Managed processes are launched directly as argv arrays; no shell is involved.
- Every managed process starts from a cleared environment. The daemon adds a small OS baseline and only the
  manifest's explicit `env_passthrough` / non-secret literal values. An empty allowlist never means “inherit
  the daemon's environment.”
- Tunnel origins are structurally parsed literal-loopback HTTP origins with an explicit port; credentials,
  query strings, base paths, hostnames, and non-loopback addresses are rejected.
- Request bodies are bounded at the HTTP router before JSON extraction.
- `cloudflared`, keep-awake helpers, and update commands receive the same minimized environment instead of
  ambient daemon secrets.
- On graceful shutdown the daemon explicitly terminates the services, tunnel, and keep-awake children it
  owns; dropping a process handle is not considered lifecycle cleanup.
- Cloudflare authentication remains in `cloudflared`'s normal local credential store. This daemon never
  accepts or persists a Cloudflare API token.
- Environment values are never returned by the control API. The manifest may name environment variables to
  pass through from the daemon process without storing their values in Git.

## Manifest

Set `GIW_DESKTOP_MANIFEST` or place `.giw-desktop.yaml` in the daemon working directory. A durable installed
copy may live at `~/.giw/desktop/manifest.yaml`.

The daemon rejects unknown manifest keys and applies deterministic bounds to services, commands, arguments,
environment declarations, and tunnel metadata. Secret-looking literal environment names are rejected; use
explicit runtime passthrough or the encrypted secret boundary instead.

The canonical example and authored config contract live in `gha-indie-worker/giw-desktop-infra`.

## API v1

- `GET /healthz`
- `GET /v1/status`
- `GET /v1/processes`
- `POST /v1/processes/{name}/start`
- `POST /v1/processes/{name}/stop`
- `POST /v1/processes/{name}/restart`
- `POST /v1/tunnel/start`
- `POST /v1/tunnel/stop`
- `POST /v1/power/keep-awake` with `{ "enabled": true|false }`
- `POST /v1/updates/apply`
- `POST /v1/reconcile`

The protocol version is returned as `protocol_version`; clients must fail closed on a newer incompatible
major protocol instead of guessing.

## Keep-awake

On macOS the daemon owns one long-lived `caffeinate` child. On Linux it owns one `systemd-inhibit` child.
The GUI checkbox and CLI command only change desired state through the daemon. Closing a GUI therefore does
not accidentally kill the underlying IndieBuild workers.

## Cloudflare / indiebuild.dev

The desktop manifest provides a named tunnel and literal loopback origin. `cloudflared tunnel run` uses its
normal local credentials. DNS/tunnel ownership remains outside the bearer-control API. This lets a developer
expose an IndieBuild service under an owned `*.indiebuild.dev` hostname while keeping the local service itself
bound to loopback.

## Updates

The API never accepts an update command from the client. An operator-reviewed update argv is declared in the
local manifest and may be invoked through `/v1/updates/apply`. Longer term this should consume signed/pinned
release metadata, but the trust boundary is already correct: update intent comes from local desired state,
not from a remote UI request body.

## Development

```bash
cargo test
cargo run --release
```

Use Rust for repository tooling and checks; do not add Python scripts.

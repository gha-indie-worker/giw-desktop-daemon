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
- The token lives at `~/.giw/desktop/token` by default with mode `0600` on Unix.
- API clients can only name services declared in the local desktop manifest. They cannot submit shell text,
  executable paths, argv, environment values, Cloudflare credentials, or arbitrary update commands.
- Managed processes are launched directly as argv arrays; no shell is involved.
- Tunnel origins must be loopback HTTP URLs.
- Cloudflare authentication remains in `cloudflared`'s normal local credential store. This daemon never
  accepts or persists a Cloudflare API token.
- Environment values are never returned by the control API. The manifest may name environment variables to
  pass through from the daemon process without storing their values in Git.

## Manifest

Set `GIW_DESKTOP_MANIFEST` or place `.giw-desktop.yaml` in the daemon working directory. A durable installed
copy may live at `~/.giw/desktop/manifest.yaml`.

The canonical example lives in `gha-indie-worker/giw-desktop-infra`.

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

The desktop manifest provides a named tunnel and loopback origin. `cloudflared tunnel run` uses its normal
local credentials. DNS/tunnel ownership remains outside the bearer-control API. This lets a developer expose
an IndieBuild service under an owned `*.indiebuild.dev` hostname while keeping the local service itself bound
to loopback.

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

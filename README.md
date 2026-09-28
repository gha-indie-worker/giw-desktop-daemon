# giw-desktop-daemon

Machine-local IndieBuild / GHA Indie Worker control plane backed by the Scintilla desktop runtime.

The daemon is deliberately a **product adapter**, not a generic local process scheduler. It accepts typed CI job intent, validates it, marks every job ephemeral with workspace reuse disabled, and forwards only that typed contract to the authenticated Scintilla loopback API.

## Security boundary

- listens only on a literal loopback address (default `127.0.0.1:8770`);
- every `/v1/*` operation requires the local GIW bearer token;
- the Scintilla URL must be credential-free literal-loopback HTTP;
- GIW and Scintilla tokens are file/runtime secrets and are not CLI flags;
- request bodies are capped at 64 KiB;
- upstream responses are capped at 1 MiB;
- redirects are disabled for Scintilla calls;
- repositories, refs, workflow names, labels, timeouts, and execution OS are bounded/validated;
- callers cannot provide commands, executable paths, mounts, container images, or Scintilla process specifications;
- desktop CI jobs are declared `ephemeral=true` and `reuse_workspace=false`.

The public product contract remains:

- `GET /healthz`
- `GET /v1/status`
- `GET /v1/scintilla/status`
- `POST /v1/jobs/dispatch`

## Configuration

Arguments are parsed through the repository's `flags-2-env` contract.

Non-secret configuration:

- `GIW_DESKTOP_ADDR=127.0.0.1:8770`
- `GIW_SCINTILLA_DAEMON_URL=http://127.0.0.1:8765`
- `RUST_LOG=giw_desktop_daemon=info`

Secrets:

- `GIW_DESKTOP_TOKEN_FILE` (default `~/.indiebuild/daemon/token`)
- `GIW_SCINTILLA_TOKEN_FILE` (default `~/.scintilla/daemon/token`)

Secrets are intentionally excluded from the CLI flag surface.

## Execution ownership

```text
GIW client / hosted control plane
          |
          v
giw-desktop-daemon
   typed CI intent only
          |
          v
scintilla-desktop-daemon
   machine/runtime lifecycle
          |
          v
ephemeral CI execution
```

GIW owns GitHub-compatible workflow/job semantics, fencing, cancellation, logs and artifacts. Scintilla owns the machine-level runtime and isolation boundary. Neither desktop UI nor GIW caller becomes an arbitrary host-command authority.

# GHA Indie Worker — giw-desktop-daemon

Canonical machine-local control plane for IndieBuild desktop/self-hosted execution.

- Rust-first; no Python tooling.
- The daemon is the sole local lifecycle writer for workers, local services, Cloudflare Tunnel, updates, and keep-awake.
- `giw-desktop-cli`, `gha-indie-worker-flutter`, and `gha-indie-worker-desktop-app.rs` are clients, not supervisors.
- Bind control APIs to loopback only.
- Do not accept arbitrary commands, shell fragments, or cloud credentials from API clients.
- Execute configured programs as argv arrays; never invoke a shell.
- Keep secrets out of repo config, logs, PRs, and command-line arguments.
- Contracts crossing runtimes belong in `gha-indie-worker-interfaces`; preserve TypeSpec/JSON Schema peer authority and TJSV parity.
- Resolve conflicts semantically; do not rebase, stash, reset, or force-push shared history.

For fleet-wide policy, also read `ORESoftware/my-ai` `AGENTS.md` and `SHARED.md`.

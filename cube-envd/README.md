# cube-envd

`cube-envd` is the Rust envd-compatible data plane that runs inside a
CubeSandbox workload container. It listens on `0.0.0.0:49983` by default.

## Build

From the repository root:

```bash
make cube-envd
```

Or from this directory:

```bash
cargo build --release
cargo test --release
```

The release binary is `target/release/cube-envd`.

## CLI

```text
--port <PORT>       Listen port; default: 49983
--isnotfc           Enable non-FC container mode
--version           Print cube-envd version
--commit            Print the embedded short Git commit
```

Legacy single-dash forms (`-port`, `-isnotfc`, `-version`, `-commit`) are
accepted for compatibility with the base-image entrypoint.

## Endpoints

All data-plane endpoints use Connect framing where noted:

| Method | Endpoint | Purpose |
| --- | --- | --- |
| GET | `/health` | Readiness probe; returns `204` |
| GET | `/status` | Service version, commit, port, and uptime |
| POST | `/init` | Apply `envVars`, `defaultUser`, and `defaultWorkdir` for later commands; returns `204` |
| GET | `/envs` | Return the environment variables previously applied by `/init` |
| POST | `/process.Process/Start` | Non-PTY command execution; Connect stream |
| POST | `/process.Process/Connect` | Reconnect to a PTY; Connect stream |
| POST | `/process.Process/SendSignal` | Signal a PTY process |
| POST | `/process.Process/SendInput` | Send PTY input |
| POST | `/process.Process/Update` | Resize a PTY |
| POST | `/filesystem.Filesystem/ListDir` | List directory entries |
| POST | `/filesystem.Filesystem/Stat` | Read file metadata |
| POST | `/filesystem.Filesystem/Remove` | Remove a file or directory |
| POST | `/filesystem.Filesystem/Move` | Rename or move an entry |
| POST | `/filesystem.Filesystem/MakeDir` | Create a directory |
| POST | `/filesystem.Filesystem/WatchDir` | Stream filesystem events |
| GET | `/files?path=...` | Read file bytes; supports Range and conditional GET |
| POST | `/files?path=...` | Write raw or multipart file bytes |

`POST /init` stores process-wide defaults. Subsequent `Process.Start` and PTY
spawns merge `envVars` (per-request values win) and fall back to
`defaultWorkdir` / `defaultUser` when the request omits a working directory or
user. This mirrors the upstream envd bootstrap contract that Cubelet uses for
`create_time_env_vars`. `GET /envs` returns the stored environment variables.

Blocking filesystem work (directory scans, `stat`, NSS user/group lookups, and
file reads/writes) runs on the tokio blocking pool, so a slow disk or NSS
lookup does not stall command or PTY streams.

## Security model

`cube-envd` does not authenticate requests itself. CubeProxy validates the
`X-Access-Token` / traffic tokens and forwards only authorized traffic; the
`Authorization: Basic` header merely selects the Unix user for a command or
file operation and must not be treated as a secret. As a result, port `49983`
must never be exposed directly — it is only safe behind the proxy.

`/files` responses include CORS headers, `Accept-Ranges`, and
`Last-Modified`. Single byte ranges return `206`; invalid or unsatisfiable
ranges return `416`; a matching `If-Modified-Since` returns `304`. Uploads
(raw and multipart) are capped at 64 MiB.

Process and PTY end events retain the upstream `exitCode`, `status`, and
`error` fields and additionally include a `termination` object when the
process ends:

```json
{
  "reason": "signal",
  "signal": 11,
  "signalName": "SIGSEGV",
  "coreDumped": true
}
```

`reason` is one of `exited`, `signal`, `timeout`, `oom`, or `unknown`.
On Linux cgroup v2, `oom` is selected when the process cgroup's
`memory.events` `oom_kill` counter increases. On cgroup v1, envd uses the
memory cgroup's `memory.oom_control` `oom_kill` counter, with
`memory.failcnt` as a best-effort fallback.

## Local smoke

The base-image smoke builds the Rust implementation, starts the container, and
checks the readiness and version contract:

```bash
make smoke-cube-base-image CUBE_BASE_PLATFORM=linux/amd64
```

For the upstream rollback implementation:

```bash
make smoke-cube-base-image CUBE_BASE_PLATFORM=linux/amd64 ENVD_IMPL=upstream-e2b
```

### Strict end-to-end smoke

[`scripts/e2e_smoke.py`](scripts/e2e_smoke.py) starts the release binary and
drives the real HTTP router, covering `/init` → process env, exit/signal/
timeout termination, `/files` (raw, multipart, Range `206`, conditional `304`,
unsatisfiable `416`), filesystem RPCs, WatchDir, PTY input/update/signal, and
24-way concurrent command + file load:

```bash
cd cube-envd
cargo build --release
python3 scripts/e2e_smoke.py
```

The template-to-sandbox validation instructions and runnable Go verifier are in
[`docs/TEMPLATE_VALIDATION.md`](docs/TEMPLATE_VALIDATION.md).

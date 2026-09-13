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

`/files` responses include CORS headers, `Accept-Ranges`, and
`Last-Modified`. Single byte ranges return `206`; invalid or unsatisfiable
ranges return `416`; a matching `If-Modified-Since` returns `304`.

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

The template-to-sandbox validation instructions and runnable Go verifier are in
[`docs/TEMPLATE_VALIDATION.md`](docs/TEMPLATE_VALIDATION.md).

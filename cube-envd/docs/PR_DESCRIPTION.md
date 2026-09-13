# Replace the base-image envd with cube-envd

## Summary

This change adds the Rust `cube-envd` data plane and switches the default
CubeSandbox base image to it. The upstream Go envd remains available through
the `ENVD_IMPL=upstream-e2b` rollback path.

## Design

- Rust + axum provides the HTTP server and low-overhead request handling.
- A dedicated Connect protocol module encodes and decodes the 5-byte envelope
  and streaming end frames.
- Process and Filesystem handlers are separate modules; commands use
  `Process.Start`, while file bytes use `/files`.
- PTY sessions are kept in a PID-indexed map and support Start, Connect,
  SendSignal, SendInput, and Update.
- WatchDir uses inotify and emits Connect-streamed filesystem events.
- `/files` now includes the complete compatibility surface required here:
  CORS, single-range `206`, unsatisfiable `416`, and conditional `304`.

## Topology

`cube-envd` runs inside the workload/template container. It is not part of the
MicroVM `cube-agent`. CubeMaster probes the workload's `49983/health` endpoint,
and sandbox data-plane requests route to the virtual host
`49983-<sandboxID>.<domain>`.

## Compatibility

Implemented endpoints are `/health`, `/status`, `/init`, all five
`/process.Process/*` operations, all six `/filesystem.Filesystem/*`
operations, and GET/POST `/files`. The `/files` handler supports raw and
multipart writes, CORS preflight, Range, `Last-Modified`, `If-Modified-Since`,
`304`, and `416` behavior. `POST /init` applies process-wide `envVars`,
`defaultUser`, and `defaultWorkdir` to later process/PTY spawns, matching the
upstream envd bootstrap path that Cubelet drives for `create_time_env_vars`.

## Known differences

- `Process.Start` currently executes with `stdin=false`; interactive input is
  provided through the PTY API.
- WatchDir covers the supported inotify create/remove/write/rename/chmod event
  mappings; filesystem event behavior remains Linux-dependent.
- PTY and inotify validation requires a Linux environment with `/dev/pts` and
  inotify support.
- The template verifier requires a reachable CubeMaster/CubeAPI cluster for
  the live BuildTemplate/Create path; without one, use the documented fallback
  and local Docker evidence.

## Validation

- `cargo test --release`: 47/47 passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `scripts/e2e_smoke.py`: all checks green (router-level HTTP + Connect:
  `/init` env, process exit/signal/timeout, `/files` raw/multipart/range/304,
  filesystem RPCs, WatchDir, PTY, 24-way concurrency).
- Docker smoke verified the real image contract, including `/health=204`,
  `/status`, request IDs, request logs, PTY, filesystem, and WatchDir behavior.
- The Rust/upstream compatibility check passed with six scenarios and a clean
  normalized diff baseline.
- Runtime smoke verified `/health=204`, `/status=200`, and JSON WARN logging
  with `ENVD_LOG_FORMAT=json ENVD_LOG_LEVEL=warn`.
- Requests can carry `X-Request-ID`; envd echoes it and includes it in logs,
  while the entrypoint reports unexpected envd exits and terminates the user
  command. The Docker smoke covered propagation, generation, and exit handling.
- Process and PTY EndEvents now include backward-compatible structured
  termination data: reason, raw signal, signal name, and core-dump state. Linux
  cgroup `memory.events` or `memory.oom_control` deltas distinguish OOM kills
  from ordinary SIGKILLs, with `memory.failcnt` as a fallback; Go SDK callers
  read it from `CommandResult.Termination` or `PtyHandle.Termination()`.
- `cube-envd/docs/TEMPLATE_VALIDATION.md`: AC-17 live-cluster procedure and
  no-cluster fallback verifier.

## Build and rollback

The default path is:

```bash
make build-cube-base-image ENVD_IMPL=cube-envd
```

It uses `docker/Dockerfile.cube-base` and compiles with Rust 1.89. To restore
the upstream implementation:

```bash
make build-cube-base-image ENVD_IMPL=upstream-e2b
```

This selects `docker/Dockerfile.cube-base-upstream`.

## Performance

The local WSL2 Docker benchmark records P50 `1.74 ms` and P99 `2.33 ms` for
100 command calls. Stability validation completes 1000 commands and 100 file
round trips without an envd PID change; the readiness endpoint remains `204`.

## ARM64 follow-up

The implementation is Linux-oriented and should be validated on a native ARM64
runner before publishing a multi-architecture base image. Verify the Rust
toolchain and all native dependencies (`portable-pty`, inotify, nix), build the
image for `linux/arm64`, and repeat PTY/inotify tests. The WSL2 evidence in this
change is `linux/amd64` only. See `spec §5.1.1` for the migration checklist.

## Attribution

Assisted-by: Codex: GPT-5
Assisted-by: opencode:deepseek-v4.1-flash

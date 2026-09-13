# docker/

Dockerfiles used by CubeSandbox CI.

## `Dockerfile.builder`

Toolchain image used to compile CubeSandbox components (Go, Rust, kernel
tooling, etc.). Published as `ghcr.io/tencentcloud/cubesandbox-builder`
by [`.github/workflows/build-builder-image.yml`](../.github/workflows/build-builder-image.yml).

## `Dockerfile.cube-base` (+ `cube-entrypoint.sh`)

Base image for user-supplied sandbox templates. It is `ubuntu:22.04`
with `envd` preinstalled on `:49983`, so any image built `FROM` it is
already ready for Cube's readiness probe. Published as
`ghcr.io/tencentcloud/cubesandbox-base` by
[`.github/workflows/build-envd-base-image.yml`](../.github/workflows/build-envd-base-image.yml),
which compiles the local Rust `cube-envd` with Rust 1.89 by default.
Use `ENVD_IMPL=upstream-e2b` to select
`Dockerfile.cube-base-upstream` and restore the upstream Go build.

Build locally from the repository root:

```bash
make build-cube-base-image
make smoke-cube-base-image
```

The entrypoint writes envd logs to `/var/log/envd.log` by default. Set
`ENVD_LOG_FILE=-` to send them to the container output, and use
`ENVD_LOG_LEVEL=warn` or `ENVD_LOG_FORMAT=json` to control verbosity and
format. The read-only `GET /status` endpoint reports readiness, version,
commit, port, and uptime; `GET /health` remains the `204` readiness probe.
Requests may provide `X-Request-ID`; envd echoes it in the response and adds
it to request logs, or generates a safe `cube-envd-<pid>-<sequence>` value.
When a user command is running, the entrypoint monitors envd and logs an
unexpected envd exit before terminating the user command.

The workflow dispatch input `envd_impl` selects the same two implementations.

Minimal consumer example:

```dockerfile
FROM ghcr.io/tencentcloud/cubesandbox-base:2026.16
RUN pip install pandas
```

Full user-facing tutorial (path A vs path B, entrypoint contract,
troubleshooting) lives in the Cube docs site:

- English: [Bring Your Own Image (envd)](../docs/guide/tutorials/bring-your-own-image.md)
- 中文：[自带镜像接入 (envd)](../docs/zh/guide/tutorials/bring-your-own-image.md)

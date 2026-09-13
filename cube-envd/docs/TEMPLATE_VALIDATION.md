# Template Validation

This guide validates the complete path from the `cube-envd` base image to a
ready template, a newly created sandbox, command execution, and file I/O.
There is no CubeMaster/CubeAPI cluster in the current WSL2 environment, so the
repository keeps a runnable SDK verifier and documents the expected cluster
requests below. The local evidence required by the fallback is already
available from the Docker smoke and end-to-end logs.

## 1. Build and publish the base image

Run these commands from the repository root in WSL2. The default implementation
is the Rust `cube-envd`; the image exposes envd on port `49983`.

```bash
make build-cube-base-image CUBE_BASE_PLATFORM=linux/amd64
docker tag cubesandbox-base:local ghcr.io/<org>/cubesandbox-base:<tag>
docker login ghcr.io
docker push ghcr.io/<org>/cubesandbox-base:<tag>
```

If the Makefile image name is overridden in the checkout, inspect it with
`make -n build-cube-base-image` and use that name in the `docker tag` command.
The image must be reachable by the node that performs `BuildTemplate`.

Before publishing, run the local fallback smoke:

```bash
make smoke-cube-base-image CUBE_BASE_PLATFORM=linux/amd64
```

The smoke must report `health=204` and `cube-envd` version/commit values. Do
not use `Dockerfile.cube-base-upstream` unless intentionally testing the
rollback implementation:

```bash
make smoke-cube-base-image CUBE_BASE_PLATFORM=linux/amd64 ENVD_IMPL=upstream-e2b
```

## 2. BuildTemplate

The repository's complete Go verifier is
`sdk/go/examples/template-validation/main.go`. It uses the SDK's
`BuildTemplate` call and sends this request to `POST /templates`:

```json
{
  "image": "ghcr.io/<org>/cubesandbox-base:<tag>",
  "name": "cube-envd-verify",
  "instanceType": "cubebox",
  "exposedPorts": [49983],
  "probePort": 49983,
  "probePath": "/health",
  "cpu": 1000,
  "memory": 1024
}
```

The probe values are intentional: `ProbePort=49983` and `ProbePath=/health`
match the envd listener and CubeSandbox template readiness semantics.

Expected `202 Accepted` response:

```json
{
  "jobID": "job-01J...",
  "templateID": "tpl-01J...",
  "status": "building",
  "phase": "pull",
  "progress": 0,
  "errorMessage": ""
}
```

With curl, the equivalent request is:

```bash
export CUBE_API_URL=http://<cube-api>:3000
export CUBE_API_KEY=<api-key>
export CUBE_ENVD_BASE_IMAGE=ghcr.io/<org>/cubesandbox-base:<tag>

curl -sS -X POST "$CUBE_API_URL/templates" \
  -H "Authorization: Bearer $CUBE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"image\":\"$CUBE_ENVD_BASE_IMAGE\",\"name\":\"cube-envd-verify\",\"instanceType\":\"cubebox\",\"exposedPorts\":[49983],\"probePort\":49983,\"probePath\":\"/health\",\"cpu\":1000,\"memory\":1024}"
```

Save `jobID` and `templateID` from the response. Authentication may be
configured differently by an installation; use the same header accepted by
the local CubeAPI deployment.

## 3. Poll build readiness

The verifier polls `GET /templates/<templateID>/builds/<jobID>/status` every
five seconds until the status is `success`, `ready`, `succeeded`, or
`completed`. A successful response looks like:

```json
{
  "buildID": "job-01J...",
  "templateID": "tpl-01J...",
  "status": "success",
  "progress": 100,
  "message": "template is ready"
}
```

Equivalent curl polling command:

```bash
curl -sS "$CUBE_API_URL/templates/$TEMPLATE_ID/builds/$JOB_ID/status" \
  -H "Authorization: Bearer $CUBE_API_KEY"
```

Stop and inspect CubeMaster/Cubelet and image-pull logs when `message` or
`errorMessage` is non-empty. Do not create a sandbox until the status is
successful.

## 4. Create a sandbox

After the build succeeds, call `POST /sandboxes` with a three-minute timeout:

```json
{
  "templateID": "tpl-01J...",
  "timeout": 180
}
```

Expected `201 Created` (or `200 OK`) response:

```json
{
  "templateID": "tpl-01J...",
  "sandboxID": "sb-01J...",
  "clientID": "client-01J...",
  "envdVersion": "cube-envd 0.1.0 ...",
  "envdAccessToken": "<token>",
  "trafficAccessToken": "<token>",
  "domain": "cube.app"
}
```

The verifier asserts that `sandboxID` and `envdVersion` are non-empty. Keep
the returned tokens private and use the returned sandbox ID in data-plane
requests.

## 5. Commands and Files

The verifier runs the following SDK calls against the envd data plane on
`49983`:

```go
command, _ := sandbox.Commands().Run(ctx,
    "echo -n cube-envd-template; whoami", cubesandbox.CommandOptions{})
_ = sandbox.Files().Write(ctx, "/tmp/cube-envd.txt",
    []byte("hello from cube-envd template"))
content, _ := sandbox.Files().Read(ctx, "/tmp/cube-envd.txt")
```

The SDK maps these to `POST /process.Process/Start`, `POST /files`, and
`GET /files` on the virtual host
`49983-<sandboxID>.<domain>`. Commands use the Connect streaming protocol;
the SDK normalizes the stream to:

```json
{
  "stdout": "cube-envd-templateroot\n",
  "stderr": "",
  "exitCode": 0
}
```

The file write returns any successful 2xx response (normally no body), and
the read returns the raw file bytes:

```text
hello from cube-envd template
```

The verifier checks the command prefix, a non-empty `whoami` result, exit code
zero, exact file content, and finally calls `DELETE /sandboxes/<sandboxID>`
to clean up.

Run the complete verifier from the SDK module directory after setting the
control-plane and data-plane variables:

```bash
cd sdk/go
export CUBE_API_URL=http://<cube-api>:3000
export CUBE_API_KEY=<api-key>
export CUBE_ENVD_BASE_IMAGE=ghcr.io/<org>/cubesandbox-base:<tag>
export CUBE_PROXY_NODE_IP=<cube-proxy-node>
export CUBE_PROXY_PORT_HTTP=80
export CUBE_PROXY_SCHEME=http
export CUBE_SANDBOX_DOMAIN=cube.app
go run ./examples/template-validation
```

Expected final line:

```text
PASS: template ready, sandbox create, commands, and files
```

## Local fallback verification

The no-cluster fallback can be reproduced locally with the checked-in build and
smoke commands. Generated logs and benchmark reports are intentionally not
committed:

- `make smoke-cube-base-image CUBE_BASE_PLATFORM=linux/amd64` builds and runs
  the image; envd `/health` returns `204`, and version/commit are non-empty.
- `cargo test --release` covers Connect framing, process, PTY, filesystem,
  WatchDir, request IDs, status behavior, and `/init` defaults.

## Troubleshooting

### Probe returns 404 or times out

- Confirm the template uses `probePort: 49983` and `probePath: "/health"`.
- Run `docker run --rm -p 49983:49983 <image>` and check
  `curl -i http://127.0.0.1:49983/health`; the expected status is `204`.
- Check that `/usr/bin/envd` is listening and that the container entrypoint
  remains PID 1. The entrypoint must start envd before any user command.
- Verify the cluster can route the probe to the sandbox's `49983` port.

### Entrypoint did not start envd

- Inspect container logs and verify `docker/cube-entrypoint.sh` has Unix LF
  line endings and executable permissions.
- Check `ENVD_PORT` (default `49983`) and run `/usr/bin/envd -version` inside
  the image.
- Avoid replacing the entrypoint with an application command that bypasses
  `/usr/bin/tini` and `cube-entrypoint.sh`.

### Image pull fails

- Confirm the registry tag exists and the cluster node has network access.
- Configure the template's registry credentials when the image is private.
- Check the image architecture matches the node (`linux/amd64` in the local
  WSL2 validation) and inspect Cubelet image-pull events.

### Commands or Files return 404

- Use the data-plane virtual host with port `49983`, not the Jupyter port
  `49999` and not the control-plane API host.
- Preserve `envdAccessToken` and `trafficAccessToken` headers returned by
  `Create`; the Go verifier configures them through the SDK.

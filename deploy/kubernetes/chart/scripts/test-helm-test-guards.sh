#!/bin/sh
# Guard: helm test pods schedule across topologies, proxy probes admin
# healthz, DNS uses getent ahostsv4, and node-runtime-test runs as root
# against read-only hostPaths.
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname "$0")" && pwd)"
CHART_DIR="$(dirname "$SCRIPT_DIR")"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

COMMON_SETS="--set-string mysql.password=test --set-string mysql.rootPassword=test --set-string redis.password=test"

render() {
  output="$1"
  shift
  helm template helm-guard "$CHART_DIR" $COMMON_SETS "$@" > "$output"
}

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

python_check() {
  python3 - "$1" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text()
docs = text.split("\n---\n")


def pod_doc(name, required=True):
    for d in docs:
        if re.search(r"^kind: Pod$", d, re.M) and re.search(
            rf"^  name: {re.escape(name)}$", d, re.M
        ):
            return d
    if required:
        raise SystemExit(f"pod {name} not found")
    return None


def has_node_selector_label(doc, label):
    return bool(
        re.search(rf"(?m)^  nodeSelector:\n(?:    .+\n)*    {re.escape(label)}:", doc)
    )


def uncommented_has(doc, needle):
    return any(
        needle in line and not line.lstrip().startswith("#")
        for line in doc.splitlines()
    )


def check_test_placement(name):
    d = pod_doc(name)
    if has_node_selector_label(d, "cube.tencent.com/cube-node"):
        raise SystemExit(f"{name}: must not pin compute nodeSelector")
    if has_node_selector_label(d, "cube.tencent.com/cube-control"):
        raise SystemExit(f"{name}: must not pin control-plane nodeSelector")
    if "cube.tencent.com/control" not in d:
        raise SystemExit(f"{name}: missing control taint toleration")
    if "cube.tencent.com/compute" not in d:
        raise SystemExit(f"{name}: missing compute taint toleration")
    print(f"OK {name} (testPlacement)")


def check_compute_placement(name):
    d = pod_doc(name)
    if not has_node_selector_label(d, "cube.tencent.com/cube-node"):
        raise SystemExit(f"{name}: expected computePlacement nodeSelector")
    if has_node_selector_label(d, "cube.tencent.com/cube-control"):
        raise SystemExit(f"{name}: computePlacement must not pin cube-control")
    print(f"OK {name} (computePlacement)")


def check_image(name, container, needle):
    d = pod_doc(name)
    m = re.search(rf"- name: {container}\s*\n\s+image: (\S+)", d)
    if not m:
        raise SystemExit(f"{name}: container {container} missing")
    if needle not in m.group(1):
        raise SystemExit(
            f"{name}: container {container} image {m.group(1)} "
            f"does not contain {needle}"
        )


mode = pathlib.Path(sys.argv[1]).name

if mode == "default.yaml":
    for name in (
        "helm-guard-cube-health-test",
        "helm-guard-cube-cubemastercli-test",
        "helm-guard-cube-mysql-test",
        "helm-guard-cube-redis-test",
        "helm-guard-cube-proxy-control-test",
        "helm-guard-cube-dns-test",
        "helm-guard-cube-node-image-test",
    ):
        check_test_placement(name)
    check_compute_placement("helm-guard-cube-node-runtime-test")

    check_image("helm-guard-cube-health-test", "curl", "curlimages/curl")
    check_image("helm-guard-cube-dns-test", "dns", "curlimages/curl")
    check_image("helm-guard-cube-node-runtime-test", "node-runtime", "busybox")
    check_image("helm-guard-cube-proxy-control-test", "proxy", "curlimages/curl")

    d = pod_doc("helm-guard-cube-proxy-control-test")
    if "/admin/healthz" not in d:
        raise SystemExit("proxy-control-test does not probe admin healthz")
    if "CUBE_PROXY_ADMIN_TOKEN" not in d or "secretKeyRef" not in d or "cube-admin-token" not in d:
        raise SystemExit("proxy-control-test does not source the admin token from the release Secret")
    if uncommented_has(d, "--retry-all-errors"):
        raise SystemExit("proxy-control-test must not pass --retry-all-errors (retries HTTP 4xx)")

    d = pod_doc("helm-guard-cube-dns-test")
    if "getent ahostsv4" not in d:
        raise SystemExit("dns-test does not use getent ahostsv4")
    if uncommented_has(d, "nslookup"):
        raise SystemExit("dns-test still uses nslookup")

    d = pod_doc("helm-guard-cube-health-test")
    if "command curl -4" not in d:
        raise SystemExit("health-test must wrap curl with -4")

    d = pod_doc("helm-guard-cube-node-image-test")
    if "command curl -4" not in d:
        raise SystemExit("node-image-test must wrap curl with -4")

    d = pod_doc("helm-guard-cube-node-runtime-test")
    if d.count("readOnly: true") < 3:
        raise SystemExit("node-runtime-test hostPath mounts are not all read-only")
    if "runAsUser: 0" not in d or "runAsGroup: 0" not in d:
        raise SystemExit("node-runtime-test must pin runAsUser/runAsGroup 0")
    if "allowPrivilegeEscalation: false" not in d or "drop:" not in d:
        raise SystemExit("node-runtime-test securityContext missing privilege hardening")

    print("helm test default placement/image/probe guard passed")

elif mode == "control-only.yaml":
    check_test_placement("helm-guard-cube-health-test")
    if pod_doc("helm-guard-cube-node-runtime-test", required=False) is not None:
        raise SystemExit("node-runtime-test must be omitted when cubeNode.enabled=false")
    if pod_doc("helm-guard-cube-node-image-test", required=False) is not None:
        raise SystemExit("node-image-test must be omitted when cubeNode.enabled=false")
    print("helm test control-only placement guard passed")

elif mode == "compute-only.yaml":
    check_test_placement("helm-guard-cube-health-test")
    check_test_placement("helm-guard-cube-cubemastercli-test")
    if pod_doc("helm-guard-cube-mysql-test", required=False) is not None:
        raise SystemExit("mysql-test must be omitted on compute-only")
    print("helm test compute-only placement guard passed")

elif mode == "single-node.yaml":
    check_test_placement("helm-guard-cube-health-test")
    check_compute_placement("helm-guard-cube-node-runtime-test")
    print("helm test single-node placement guard passed")

else:
    raise SystemExit(f"unknown render {mode}")
PY
}

render "$TMP_DIR/default.yaml"
python_check "$TMP_DIR/default.yaml"

render "$TMP_DIR/control-only.yaml" --set cubeNode.enabled=false
python_check "$TMP_DIR/control-only.yaml"

render "$TMP_DIR/compute-only.yaml" \
  --set controlPlane.enabled=false \
  --set externalControlPlane.enabled=true \
  --set-string externalControlPlane.masterEndpoint=http://10.0.0.1:8080 \
  --set mysql.enabled=false \
  --set redis.enabled=false
python_check "$TMP_DIR/compute-only.yaml"

render "$TMP_DIR/single-node.yaml" -f "$CHART_DIR/values-single-node.yaml"
python_check "$TMP_DIR/single-node.yaml"

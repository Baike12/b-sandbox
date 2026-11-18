#!/usr/bin/env bash
# Deploy b-sandbox to a server as a container.
#
# Same pipeline as ../b-superagent: cross-compile locally, ship over ssh,
# build the image on the server, start the compose project, verify through
# nginx. The server needs no Rust toolchain; the transfer size is the binary.
#
# Cross-compilation uses cargo-zigbuild (static musl, no system linker):
#   brew install zig && cargo install cargo-zigbuild
#   rustup target add x86_64-unknown-linux-musl
#
# The deployment target has no default on purpose: a host address stored in
# version control publishes the server to every reader of the repository.
# The operator names the target through B_SANDBOX_HOST or --host.
#
# Usage:
#   B_SANDBOX_HOST=user@host bash scripts/deploy.sh
#   bash scripts/deploy.sh --host user@host --tag 20261115-1900
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${B_SANDBOX_HOST:-}"
TAG="${B_SANDBOX_TAG:-$(date -u +%Y%m%d-%H%M)}"
APP_DIR="${B_SANDBOX_APP_DIR:-/opt/b-sandbox}"
TARGET_TRIPLE="x86_64-unknown-linux-musl"
BASE_PATH="${B_SANDBOX_BASE_PATH:-/b-sandbox}"
BINARY="dist/b-sandbox-linux-amd64"
SKIP_BUILD=0

usage() {
  cat <<EOF
Usage: bash scripts/deploy.sh [options]

Options:
  --host HOST      ssh target, including the user       (required)
  --tag TAG        image tag and build version          (default: ${TAG})
  --app-dir DIR    application directory on the server  (default: ${APP_DIR})
  --base-path PATH path the app is served under         (default: ${BASE_PATH})
  --skip-build     ship the binary that is already in dist/
  -h, --help       show this help

Environment: B_SANDBOX_HOST, B_SANDBOX_TAG, B_SANDBOX_APP_DIR, B_SANDBOX_BASE_PATH
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --tag) TAG="$2"; shift 2 ;;
    --app-dir) APP_DIR="$2"; shift 2 ;;
    --base-path) BASE_PATH="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [ -z "${HOST}" ]; then
  echo "no deployment target: set B_SANDBOX_HOST or pass --host" >&2
  usage >&2
  exit 2
fi

if [ "${SKIP_BUILD}" -eq 0 ]; then
  echo ">>> cross-compile ${TARGET_TRIPLE} (release)"
  cargo zigbuild --release --target "${TARGET_TRIPLE}"
  mkdir -p dist
  cp "target/${TARGET_TRIPLE}/release/sandbox-api" "${BINARY}"
else
  echo ">>> reusing existing ${BINARY}"
fi

if [ ! -f "${BINARY}" ]; then
  echo "${BINARY} does not exist" >&2
  exit 1
fi

echo ">>> ship artifacts to ${HOST}"
retry() {
  local attempt=0
  until "$@"; do
    attempt=$((attempt + 1))
    if [ "${attempt}" -ge 6 ]; then
      echo "giving up after ${attempt} attempts: $*" >&2
      exit 1
    fi
    echo "retry ${attempt}: $*" >&2
    sleep 5
  done
}

retry ssh -o BatchMode=yes -o ConnectTimeout=20 "${HOST}" "mkdir -p '${APP_DIR}/dist' '${APP_DIR}/nginx'"
retry scp -o BatchMode=yes -o ConnectTimeout=20 \
  deploy/Dockerfile deploy/compose.yaml deploy/server_setup.sh deploy/remote_run.sh \
  "${HOST}:${APP_DIR}/"
retry scp -o BatchMode=yes -o ConnectTimeout=20 \
  deploy/nginx/b-sandbox.conf.template "${HOST}:${APP_DIR}/nginx/"
retry scp -o BatchMode=yes -o ConnectTimeout=20 \
  "${BINARY}" "${HOST}:${APP_DIR}/dist/b-sandbox-linux-amd64"

# Ship the console build when one exists (Dockerfile copies console/ into
# the image; without it the console routes answer 400 with a build hint).
if [ -d web/dist ]; then
  echo ">>> ship the web console build"
  retry ssh -o BatchMode=yes -o ConnectTimeout=20 "${HOST}" "mkdir -p '${APP_DIR}/console'"
  retry scp -o BatchMode=yes -o ConnectTimeout=20 -r web/dist/. "${HOST}:${APP_DIR}/console/"
fi

echo ">>> prepare the server and start the compose project"
retry ssh -o BatchMode=yes -o ConnectTimeout=20 "${HOST}" \
  "cd '${APP_DIR}' && B_SANDBOX_BASE_PATH='${BASE_PATH}' bash server_setup.sh && TAG='${TAG}' bash remote_run.sh"

echo ">>> verify through the nginx route"
retry ssh -o BatchMode=yes -o ConnectTimeout=20 "${HOST}" "curl -fsS 'http://127.0.0.1${BASE_PATH}/health'"
echo
echo "Deployed ${TAG}. Health: http://${HOST##*@}${BASE_PATH}/health"

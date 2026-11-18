#!/usr/bin/env bash
# Build the runtime image on the server and (re)start the compose project.
# Called on the server by the local scripts/deploy.sh.
#
# The image is built on the server rather than loaded from a tarball: the
# build context is one static binary, the base image is already in place,
# and the server needs no registry access.
set -euo pipefail

: "${TAG:?TAG is required}"

APP_DIR="${APP_DIR:-/opt/b-sandbox}"
BASE_IMAGE="${B_SANDBOX_BASE_IMAGE:-alpine:3.20}"
IMAGE="b-sandbox:${TAG}"
ENV_FILE="${APP_DIR}/.env"

cd "${APP_DIR}"

if [ ! -f dist/b-sandbox-linux-amd64 ]; then
  echo "${APP_DIR}/dist/b-sandbox-linux-amd64 does not exist" >&2
  exit 1
fi

echo ">>> build image ${IMAGE}"
docker build --build-arg "BASE_IMAGE=${BASE_IMAGE}" -t "${IMAGE}" "${APP_DIR}"

echo ">>> point the compose project at that tag (keeping the auth token)"
# The compose file refuses to start without B_SANDBOX_AUTH_TOKEN. Keep an
# existing token across deploys; generate one on first deploy. The file is
# 0600 and lives outside the repo and the image.
if [ -f "${ENV_FILE}" ] && grep -q '^B_SANDBOX_AUTH_TOKEN=' "${ENV_FILE}"; then
  TOKEN_LINE="$(grep '^B_SANDBOX_AUTH_TOKEN=' "${ENV_FILE}")"
else
  TOKEN_LINE="B_SANDBOX_AUTH_TOKEN=$(openssl rand -hex 32 2>/dev/null \
    || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  echo ">>> generated a new B_SANDBOX_AUTH_TOKEN in ${ENV_FILE} (give it to API callers)"
fi
printf 'B_SANDBOX_TAG=%s\n%s\n' "${TAG}" "${TOKEN_LINE}" > "${ENV_FILE}"
chmod 600 "${ENV_FILE}"

echo ">>> start the compose project"
docker compose --project-directory "${APP_DIR}" up -d --force-recreate

echo ">>> keep the last 3 image tags for rollback"
docker images --format '{{.Repository}}:{{.Tag}} {{.CreatedAt}}' b-sandbox \
  | grep -v ':latest ' \
  | sort -k2 -r \
  | awk 'NR>3 {print $1}' \
  | xargs -r docker rmi -f >/dev/null 2>&1 || true

sleep 2
docker ps --filter name=b-sandbox --format 'container: {{.Names}} | {{.Status}} | image: {{.Image}}'

#!/usr/bin/env bash
# One-time server preparation for b-sandbox. Idempotent: rerunning on an
# already-prepared machine makes no change.
#
#   1. Create the application and data directories
#   2. Hand the data directory to the container's unprivileged user
#   3. Ensure the runtime image's base image exists (the server cannot reach
#      Docker Hub; pull through a mirror and retag)
#   4. Install the nginx snippet routing {{BASE_PATH}} to the container
#
# The script does not touch the firewall: the service is reached through the
# host nginx already serving port 80.
set -euo pipefail

APP_DIR="${APP_DIR:-/opt/b-sandbox}"
DATA_DIR="${B_SANDBOX_DATA_DIR:-${APP_DIR}/data}"
CONTAINER_UID="${B_SANDBOX_UID:-10001}"
BASE_IMAGE="${B_SANDBOX_BASE_IMAGE:-alpine:3.20}"
MIRROR_IMAGE="${B_SANDBOX_MIRROR_IMAGE:-docker.m.daocloud.io/library/alpine:3.20}"
BASE_PATH="${B_SANDBOX_BASE_PATH:-/b-sandbox}"
NGINX_SNIPPET="/etc/nginx/snippets/b-sandbox.conf"
NGINX_TEMPLATE="${APP_DIR}/nginx/b-sandbox.conf.template"
ITEACH_CONF="/etc/nginx/sites-available/iteach.conf"

echo ">>> [1/4] create application directories"
mkdir -p "${APP_DIR}/dist" "${DATA_DIR}"

echo ">>> [2/4] hand ${DATA_DIR} to container user (uid ${CONTAINER_UID})"
# Workload rootfs and event logs are written under the data root.
chown -R "${CONTAINER_UID}:${CONTAINER_UID}" "${DATA_DIR}"
chmod 755 "${DATA_DIR}"

echo ">>> [3/4] prepare base image ${BASE_IMAGE}"
if docker image inspect "${BASE_IMAGE}" >/dev/null 2>&1; then
  echo "    ${BASE_IMAGE} already present"
else
  if ! docker image inspect "${MIRROR_IMAGE}" >/dev/null 2>&1; then
    docker pull "${MIRROR_IMAGE}"
  fi
  docker tag "${MIRROR_IMAGE}" "${BASE_IMAGE}"
  echo "    tagged ${MIRROR_IMAGE} as ${BASE_IMAGE}"
fi

echo ">>> [4/4] install nginx route ${BASE_PATH}"
if [ ! -f "${NGINX_TEMPLATE}" ]; then
  echo "    ${NGINX_TEMPLATE} does not exist" >&2
  exit 1
fi
mkdir -p /etc/nginx/snippets
sed "s|{{BASE_PATH}}|${BASE_PATH}|g" "${NGINX_TEMPLATE}" > "${NGINX_SNIPPET}"

if ! grep -q 'snippets/b-sandbox.conf' "${ITEACH_CONF}"; then
  cp "${ITEACH_CONF}" "${ITEACH_CONF}.bak.$(date -u +%Y%m%d%H%M%S)"
  # Insert the include just before the last closing brace (end of the server
  # block); the rest of the site configuration stays untouched.
  awk -v snippet="    include ${NGINX_SNIPPET};" '
    { lines[NR] = $0 }
    END {
      last = NR
      while (last > 0 && lines[last] ~ /^[[:space:]]*$/) last--
      for (i = 1; i <= NR; i++) {
        if (i == last && lines[i] ~ /^[[:space:]]*}/) {
          print snippet
        }
        print lines[i]
      }
    }
  ' "${ITEACH_CONF}" > "${ITEACH_CONF}.tmp"
  mv "${ITEACH_CONF}.tmp" "${ITEACH_CONF}"
  echo "    include added to ${ITEACH_CONF}"
else
  echo "    include already present"
fi

# One broken nginx config takes every co-tenant down, so reload only after
# the parser accepts the new configuration; roll back otherwise.
if ! nginx -t; then
  echo "    nginx rejected the configuration; restoring backup" >&2
  cp "${ITEACH_CONF}.bak."* "${ITEACH_CONF}"
  nginx -t
  exit 1
fi
systemctl reload nginx
echo "    route live: ${BASE_PATH}/"

echo
echo "Prepared ${APP_DIR}. Next: TAG=<tag> bash ${APP_DIR}/remote_run.sh"

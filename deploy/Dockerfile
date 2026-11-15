# b-sandbox runtime image.
#
# The image carries only the cross-compiled static binary plus the userspace
# tool its isolation backend needs. Built on the server (deploy/remote_run.sh):
# the build context is one binary, and the base image is mirrored + tagged by
# server_setup.sh, so the server never needs registry or toolchain access.
ARG BASE_IMAGE=alpine:3.20
FROM ${BASE_IMAGE}

# bubblewrap is the entrypoint of every workload on this tier; the rest of
# the container stays minimal on purpose — it is not a general-purpose host.
RUN apk add --no-cache ca-certificates tzdata bubblewrap \
 && adduser -D -u 10001 sandbox

WORKDIR /app
COPY dist/b-sandbox-linux-amd64 /app/sandbox-api
# RUN chmod is the backstop: scp does not preserve the exec bit.
RUN chmod +x /app/sandbox-api

USER 10001
ENV TZ=Asia/Shanghai
EXPOSE 8094

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s \
  CMD wget -qO- http://127.0.0.1:8094/health || exit 1

# The container must listen on 0.0.0.0: a published port forwards to the
# container IP, not its loopback. The host-side publish address stays
# 127.0.0.1 (compose), so nginx remains the only facade.
ENTRYPOINT ["/app/sandbox-api", "--data-root", "/app/data", "--backend", "bwrap", \
            "--bind", "0.0.0.0", "--port", "8094"]

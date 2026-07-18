# syntax=docker/dockerfile:1
#
# Runtime image assembled from prebuilt release binaries — NO compilation.
# Built by .github/workflows/docker-publish.yml for linux/amd64 + linux/arm64
# using the signed tarballs that release.yml already cross-compiles.
#
# The build context must contain the matching binary at:
#   dist/linux/<arch>/ironcrew     (arch = amd64 | arm64)
#
# Mirrors the base of the from-source ./Dockerfile (debian:13-slim +
# ca-certificates) so the published image behaves identically.
FROM debian:13-slim

# Provided automatically by buildx per target platform (amd64 / arm64).
ARG TARGETARCH

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -o 10001 -g 0 -m 0770 /app /data /data/outputs \
    && install -d -o 0 -g 0 -m 0555 /flows

# --chmod avoids a separate (emulated) RUN just to set the executable bit.
COPY --chmod=0755 dist/linux/${TARGETARCH}/ironcrew /usr/local/bin/ironcrew

ENV HOME=/tmp \
    IRONCREW_HOST=0.0.0.0 \
    IRONCREW_FILE_WRITE_ROOT=/data/outputs \
    IRONCREW_MCP_ALLOWED_COMMANDS=__disabled__ \
    IRONCREW_MCP_ALLOWED_HTTP_HOSTS=__disabled__ \
    PATH="/usr/local/bin:${PATH}"

USER 10001:0

WORKDIR /app

EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/ironcrew"]
CMD ["serve", "--flows-dir", "/flows"]

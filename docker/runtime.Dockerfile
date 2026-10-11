# Runtime image assembled from prebuilt release binaries — NO compilation.
# release.yml builds one signed linux/amd64 + linux/arm64 OCI archive from the
# release tag. Registry publication promotes that immutable archive.
#
# The build context must contain the matching binary at:
#   dist/linux/<arch>/ironcrew     (arch = amd64 | arm64)
#
# This multi-architecture Wolfi index includes glibc, OpenSSL, and the Mozilla
# CA bundle needed by the GNU release binaries. Keep it pinned by index digest:
# historical tag builds must never resolve a moving base or package index.
FROM cgr.dev/chainguard/wolfi-base@sha256:1c451d46a0d5c4e9f2b38e0e8d3e299564a1aa95c21973efcc1980a9d1d2e73e

# Provided automatically by buildx per target platform (amd64 / arm64).
ARG TARGETARCH

RUN test -s /etc/ssl/certs/ca-certificates.crt \
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

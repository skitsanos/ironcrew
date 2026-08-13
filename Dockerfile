# Keep the builder aligned with `package.rust-version` in Cargo.toml. Using an
# exact toolchain tag prevents a future `rust:latest` release from changing the
# build underneath us.
FROM rust:1.97.1-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
COPY tests ./tests

RUN cargo build --release --locked

FROM debian:13-slim AS runtime

WORKDIR /app

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && install -d -o 10001 -g 0 -m 0770 /app /data /data/outputs \
    && install -d -o 0 -g 0 -m 0555 /flows

COPY --from=builder --chmod=0755 /app/target/release/ironcrew /usr/local/bin/ironcrew

ENV HOME=/tmp \
    IRONCREW_HOST=0.0.0.0 \
    IRONCREW_FILE_WRITE_ROOT=/data/outputs \
    IRONCREW_MCP_ALLOWED_COMMANDS=__disabled__ \
    IRONCREW_MCP_ALLOWED_HTTP_HOSTS=__disabled__ \
    PATH="/usr/local/bin:${PATH}"

USER 10001:0

EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/ironcrew"]
CMD ["serve", "--flows-dir", "/flows"]

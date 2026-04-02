FROM rust:latest AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
COPY tests ./tests

RUN cargo build --release

FROM debian:13-slim AS runtime

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ironcrew /usr/local/bin/ironcrew

ENV PATH="/usr/local/bin:${PATH}"

ENTRYPOINT ["/usr/local/bin/ironcrew"]

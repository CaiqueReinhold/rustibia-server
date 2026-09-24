FROM rust:1.95-slim-trixie AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rustibia-target,target=/app/target,sharing=locked \
    cargo build --release --locked -p rustibia-site \
    && cp target/release/rustibia-site /rustibia-site

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home rustibia
WORKDIR /app
COPY --from=builder /rustibia-site ./rustibia-site
COPY crates/site/config.yaml ./config.yaml
COPY crates/site/static/ static/
USER rustibia
ENV BIND_ADDRESS=0.0.0.0:8080 \
    INTERNAL_BIND_ADDRESS=0.0.0.0:8443
EXPOSE 8080 8443
CMD ["./rustibia-site"]

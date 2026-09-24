FROM rust:1.95-slim-trixie AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rustibia-target,target=/app/target,sharing=locked \
    cargo build --profile production --locked -p rustibia-server --bin server \
    && cp target/production/server /server

FROM debian:trixie-slim
RUN useradd --system --uid 10001 --no-create-home rustibia
WORKDIR /app
COPY --from=builder /server ./server
COPY crates/server/assets/ assets/
USER rustibia
EXPOSE 5555
CMD ["./server"]

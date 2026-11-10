FROM rust:1.99-slim
WORKDIR /app

# Build context is the workspace root, so both manifests and the shared lockfile are
# visible. The site's manifest is required for the workspace to resolve at all.
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates

RUN cargo build
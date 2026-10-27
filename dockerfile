# Multi-stage Docker build for Offisde Bot Rust Tokio API
# Stage 1: Build binary
FROM rust:bookworm AS builder

WORKDIR /usr/src/offside-bot

# Cache dependencies
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs
RUN cargo build --release || true
RUN rm -rf src

# Copy real source files
COPY src/ ./src/
COPY tests/ ./tests/

# Compile production binary
RUN touch src/main.rs src/lib.rs && cargo build --release

# Stage 2: Minimal runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /usr/src/offside-bot/target/release/offside-bot /app/offside-bot

CMD ["/app/offside-bot"]
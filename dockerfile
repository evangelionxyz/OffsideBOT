# Multi-stage Docker build for Offside Bot (Rust + Tokio + Songbird)
# Stage 1: Build binary
FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    pkg-config \
    libopus-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/offside-bot

# Copy Cargo config and manifests to cache dependencies
COPY .cargo/ ./.cargo/
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release && rm -rf src target/release/deps/offside_bot* target/release/offside-bot*

# Copy real source files and compile production binary
COPY src/ ./src/
RUN cargo build --release

# Stage 2: Minimal runtime with yt-dlp, ffmpeg, and deno
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    ffmpeg \
    libopus0 \
    libssl3 \
    python3 \
    unzip \
    && curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o /usr/local/bin/yt-dlp \
    && chmod a+rx /usr/local/bin/yt-dlp \
    && curl -fsSL https://deno.land/install.sh | DENO_INSTALL=/usr/local sh \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /usr/src/offside-bot/target/release/offside-bot /app/offside-bot

CMD ["/app/offside-bot"]

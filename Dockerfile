# syntax=docker/dockerfile:1
# Multi-stage build: Vue 3 SPA (@nuvek/ui) + Rust/axum backend (nuvek-web).
# Serves the web UI and REST API for autonomous agent teams and human operators.
# The image holds only the board; hive-mcp and hive-gardener ship as release binaries.

# --- 1. Build the Vue SPA (pulls @nuvek/ui from its public git tag) ---
FROM node:22-trixie-slim AS frontend
WORKDIR /app/frontend
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
ARG VITE_BASE_PATH
RUN npm run build

# --- 2. Build the Rust backend (pulls its git dependencies from their public tags) ---
FROM rust:1-slim-trixie AS backend
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates build-essential \
  && rm -rf /var/lib/apt/lists/*
# The whole workspace: Cargo resolves every member, even when building one package.
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
# Migrations are embedded at compile time (sqlx::migrate!); no database is needed to build.
COPY migrations/ ./migrations/
COPY crates/hive-mcp/Cargo.toml ./crates/hive-mcp/Cargo.toml
COPY crates/hive-mcp/src/ ./crates/hive-mcp/src/
COPY crates/hive-gardener/Cargo.toml ./crates/hive-gardener/Cargo.toml
COPY crates/hive-gardener/src/ ./crates/hive-gardener/src/
# `import` adds the one-shot `hive-board import-sqlite` subcommand (bundled SQLite).
RUN cargo build --release --locked -p hive-board --features import

# --- 3. Runtime ---
FROM debian:trixie-slim
LABEL org.opencontainers.image.source="https://github.com/jean1880/nuvek-hive-board" \
      org.opencontainers.image.licenses="MIT"
RUN useradd --system --uid 10001 --user-group --no-create-home appuser \
  && mkdir -p /data && chown -R 10001:10001 /data
WORKDIR /app
COPY --from=backend /app/target/release/hive-board /usr/local/bin/hive-board
COPY --from=frontend /app/frontend/dist /app/dist
USER 10001:10001
# /data is only the mount point for the legacy SQLite source of `import-sqlite`.
# Required at runtime: HIVE_DATABASE_URL, HIVE_API_TOKEN. Health: `hive-board healthcheck`.
# BIND=0.0.0.0 here because a container is only reachable through its published port; the
# binary's own default is 127.0.0.1. Publish the port on 127.0.0.1 unless a reverse proxy
# fronts it, and keep the read gate (HIVE_AUTH_READS) on.
ENV BIND=0.0.0.0 \
    PORT=8080 \
    DIST_DIR=/app/dist
EXPOSE 8080
ENTRYPOINT ["hive-board"]

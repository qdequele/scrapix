# =============================================================================
# Scrapix Multi-Stage Dockerfile
# =============================================================================
# Builds all Rust binaries with dependency caching for fast rebuilds.
#
# Layer strategy:
#   1. Copy Cargo manifests + lock file, create stub sources
#   2. `cargo build --release` to compile all dependencies (cached)
#   3. Copy real source, build again (only recompiles workspace crates)
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1: Builder - Build all binaries
# -----------------------------------------------------------------------------
FROM rust:1.93-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    libssl-dev \
    libsasl2-dev \
    libclang-dev \
    libcurl4-openssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace manifest and lock file
COPY Cargo.toml Cargo.lock ./

# Copy all crate manifests
COPY crates/scrapix-core/Cargo.toml crates/scrapix-core/Cargo.toml
COPY crates/scrapix-frontier/Cargo.toml crates/scrapix-frontier/Cargo.toml
COPY crates/scrapix-crawler/Cargo.toml crates/scrapix-crawler/Cargo.toml
COPY crates/scrapix-parser/Cargo.toml crates/scrapix-parser/Cargo.toml
COPY crates/scrapix-extractor/Cargo.toml crates/scrapix-extractor/Cargo.toml
COPY crates/scrapix-ai/Cargo.toml crates/scrapix-ai/Cargo.toml
COPY crates/scrapix-storage/Cargo.toml crates/scrapix-storage/Cargo.toml
COPY crates/scrapix-queue/Cargo.toml crates/scrapix-queue/Cargo.toml
COPY crates/scrapix-ocr/Cargo.toml crates/scrapix-ocr/Cargo.toml
COPY bins/scrapix-api/Cargo.toml bins/scrapix-api/Cargo.toml
COPY bins/scrapix-worker-crawler/Cargo.toml bins/scrapix-worker-crawler/Cargo.toml
COPY bins/scrapix-worker-content/Cargo.toml bins/scrapix-worker-content/Cargo.toml
COPY bins/scrapix-frontier-service/Cargo.toml bins/scrapix-frontier-service/Cargo.toml
COPY bins/scrapix-cli/Cargo.toml bins/scrapix-cli/Cargo.toml
COPY bins/scrapix/Cargo.toml bins/scrapix/Cargo.toml
COPY benches/Cargo.toml benches/Cargo.toml
COPY tests/Cargo.toml tests/Cargo.toml

# Create stub source files so cargo can resolve the workspace and compile deps
RUN mkdir -p crates/scrapix-core/src && echo "" > crates/scrapix-core/src/lib.rs \
    && mkdir -p crates/scrapix-frontier/src && echo "" > crates/scrapix-frontier/src/lib.rs \
    && mkdir -p crates/scrapix-crawler/src && echo "" > crates/scrapix-crawler/src/lib.rs \
    && mkdir -p crates/scrapix-parser/src && echo "" > crates/scrapix-parser/src/lib.rs \
    && mkdir -p crates/scrapix-extractor/src && echo "" > crates/scrapix-extractor/src/lib.rs \
    && mkdir -p crates/scrapix-ai/src && echo "" > crates/scrapix-ai/src/lib.rs \
    && mkdir -p crates/scrapix-storage/src && echo "" > crates/scrapix-storage/src/lib.rs \
    && mkdir -p crates/scrapix-queue/src && echo "" > crates/scrapix-queue/src/lib.rs \
    && mkdir -p crates/scrapix-ocr/src && echo "" > crates/scrapix-ocr/src/lib.rs \
    && mkdir -p bins/scrapix-api/src && echo "" > bins/scrapix-api/src/lib.rs && echo "fn main() {}" > bins/scrapix-api/src/main.rs \
    && mkdir -p bins/scrapix-worker-crawler/src && echo "" > bins/scrapix-worker-crawler/src/lib.rs && echo "fn main() {}" > bins/scrapix-worker-crawler/src/main.rs \
    && mkdir -p bins/scrapix-worker-content/src && echo "" > bins/scrapix-worker-content/src/lib.rs && echo "fn main() {}" > bins/scrapix-worker-content/src/main.rs \
    && mkdir -p bins/scrapix-frontier-service/src && echo "" > bins/scrapix-frontier-service/src/lib.rs && echo "fn main() {}" > bins/scrapix-frontier-service/src/main.rs \
    && mkdir -p bins/scrapix-cli/src && echo "" > bins/scrapix-cli/src/lib.rs && echo "fn main() {}" > bins/scrapix-cli/src/main.rs \
    && mkdir -p bins/scrapix/src && echo "fn main() {}" > bins/scrapix/src/main.rs \
    && mkdir -p benches/src && echo "" > benches/src/lib.rs \
    && mkdir -p tests/src && echo "" > tests/src/lib.rs

# Build dependencies only (this layer is cached until Cargo.toml/Cargo.lock change)
RUN cargo build --release --workspace 2>&1 || true

# Remove stub artifacts so cargo detects the real source as changed
RUN find target/release/.fingerprint \
    -name "scrapix-*" -type d -exec rm -rf {} + 2>/dev/null || true

# Copy real source
COPY crates/ crates/
COPY bins/ bins/
COPY benches/ benches/
COPY tests/ tests/

# Build workspace (only recompiles workspace crates, deps are cached)
RUN cargo build --release --workspace

# -----------------------------------------------------------------------------
# PDFium — rasterizes scanned PDF pages for OCR (SCR-86). Loaded at runtime by
# scrapix-ocr (PDFIUM_LIB_PATH); never linked at build time. Pinned to the
# bblanchon/pdfium-binaries release firecrawl-pdfium 0.1.0 is tested against;
# the checksums come from its pdfium.lock.json and must change with the tag.
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS pdfium
ARG TARGETARCH
ARG PDFIUM_RELEASE=chromium/7988
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) asset=pdfium-linux-x64.tgz; sha=7358c15e26a746cd67854887ea11b3b807c436056788eee9294fb972b8f8e0be ;; \
      arm64) asset=pdfium-linux-arm64.tgz; sha=a2926203456881efa8feca7e0e409de78a4471b9b72e62a74c820faebb3e4551 ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/pdfium.tgz \
      "https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_RELEASE}/${asset}"; \
    echo "${sha}  /tmp/pdfium.tgz" | sha256sum -c -; \
    mkdir -p /opt/pdfium; \
    tar -xzf /tmp/pdfium.tgz -C /opt/pdfium; \
    rm /tmp/pdfium.tgz; \
    test -f /opt/pdfium/lib/libpdfium.so

# -----------------------------------------------------------------------------
# Stage 2: Runtime base - Minimal runtime image
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime-base

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    libsasl2-2 \
    libcurl4 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 -s /bin/bash scrapix
USER scrapix
WORKDIR /app

# -----------------------------------------------------------------------------
# Stage 2b: Runtime with document parsing / OCR support (API, content worker)
# -----------------------------------------------------------------------------
FROM runtime-base AS runtime-documents
# Documents & OCR: Tesseract (local OCR backend, no API key needed) and the
# pinned PDFium build (page rasterization).
USER root
RUN apt-get update && apt-get install -y --no-install-recommends \
    tesseract-ocr \
    tesseract-ocr-eng \
    && rm -rf /var/lib/apt/lists/*
COPY --from=pdfium /opt/pdfium /opt/pdfium
ENV PDFIUM_LIB_PATH=/opt/pdfium/lib
USER scrapix

# -----------------------------------------------------------------------------
# Stage 3a: API Service
# -----------------------------------------------------------------------------
FROM runtime-documents AS scrapix-api
COPY --from=builder --chown=scrapix:scrapix /app/target/release/scrapix /app/scrapix
EXPOSE 8080
ENV RUST_LOG=info
ENTRYPOINT ["/app/scrapix", "api"]

# -----------------------------------------------------------------------------
# Stage 3b: Frontier Service
# -----------------------------------------------------------------------------
FROM runtime-base AS scrapix-frontier-service
COPY --from=builder --chown=scrapix:scrapix /app/target/release/scrapix /app/scrapix
ENV RUST_LOG=info
ENTRYPOINT ["/app/scrapix", "frontier"]

# -----------------------------------------------------------------------------
# Stage 3c: Crawler Worker
# -----------------------------------------------------------------------------
FROM runtime-base AS scrapix-worker-crawler
COPY --from=builder --chown=scrapix:scrapix /app/target/release/scrapix /app/scrapix
ENV RUST_LOG=info
ENTRYPOINT ["/app/scrapix", "crawler"]

# -----------------------------------------------------------------------------
# Stage 3d: Content Worker
# -----------------------------------------------------------------------------
FROM runtime-documents AS scrapix-worker-content
COPY --from=builder --chown=scrapix:scrapix /app/target/release/scrapix /app/scrapix
ENV RUST_LOG=info
ENTRYPOINT ["/app/scrapix", "content"]

# -----------------------------------------------------------------------------
# Stage 3e: CLI
# -----------------------------------------------------------------------------
FROM runtime-base AS scrapix-cli
COPY --from=builder --chown=scrapix:scrapix /app/target/release/scrapix /app/scrapix
ENTRYPOINT ["/app/scrapix"]

# -----------------------------------------------------------------------------
# Default target: Single unified binary (runs all services or any subcommand)
# -----------------------------------------------------------------------------
FROM runtime-documents AS scrapix-all
COPY --from=builder --chown=scrapix:scrapix /app/target/release/scrapix /app/scrapix
ENV RUST_LOG=info
ENTRYPOINT ["/app/scrapix", "all"]

# syntax=docker/dockerfile:1
# Single image for every service (mcp + agent + codex + workers).
# docker-compose picks the command per service. Two stages: the Rust
# binaries (crates/mcp, crates/agent, crates/codex, crates/evals), and a
# runtime that carries them plus the Codex CLI, which is an npm package — the
# only reason the runtime is a node image.
#
# Built on a laptop and shipped to the droplet (scripts/deploy.sh): the
# droplet is 1 vCPU / 2 GB and cannot compile the crates. The image targets
# linux/amd64 while the laptop is arm64, so:
#   - the Rust stage runs on the BUILD platform and cross-compiles — native
#     speed instead of compiling under QEMU emulation;
#   - cargo's registry and target dir live in BuildKit cache mounts, so a
#     rebuild only recompiles what changed.

# Every binary of the workspace. Bookworm matches the runtime's glibc.
FROM --platform=$BUILDPLATFORM rust:1.90-bookworm AS mcp-build
ARG TARGETARCH
# Debian's triplet-prefixed gcc exists natively for the build arch; the other
# arch needs the cross package. cmake/clang cover aws-lc-rs (rustls' crypto
# backend).
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu; gnu=x86_64-linux-gnu; cross="gcc-x86-64-linux-gnu g++-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; cross="gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$(dpkg --print-architecture)" = "$TARGETARCH" ]; then cross=""; fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends cmake clang $cross; \
    rm -rf /var/lib/apt/lists/*; \
    rustup target add "$triple"; \
    { echo "export TRIPLE=$triple"; \
      upper=$(echo "$triple" | tr 'a-z-' 'A-Z_'); lower=$(echo "$triple" | tr '-' '_'); \
      echo "export CARGO_TARGET_${upper}_LINKER=${gnu}-gcc"; \
      echo "export CC_${lower}=${gnu}-gcc CXX_${lower}=${gnu}-g++ AR_${lower}=${gnu}-ar"; \
    } > /cross-env
# Two dozen binaries link at once otherwise, and each release link of this
# crate takes ~0.5 GB: an 8 GB Docker VM ran out (`ld … Killed`). ARGs reach
# the RUN below as env, which cargo reads.
ARG CARGO_BUILD_JOBS=4
# Native deployments select just the affected binaries; the full runtime
# image still builds every CLI when BINARIES is empty.
ARG BINARIES=""
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eu; . /cross-env; \
    if [ -n "$BINARIES" ]; then \
      set --; for binary in $BINARIES; do set -- "$@" --bin "$binary"; done; \
      cargo build --release --locked --target "$TRIPLE" "$@"; \
    else cargo build --release --locked --bins --target "$TRIPLE"; fi; \
    mkdir -p /out; \
    if [ -n "$BINARIES" ]; then \
      for binary in $BINARIES; do cp "target/$TRIPLE/release/$binary" /out/; done; \
    else find "target/$TRIPLE/release" -maxdepth 1 -type f -perm -u+x -exec cp {} /out/ \;; fi

# Export files, without a runtime image or docker save/load round trip.
FROM scratch AS native
COPY --from=mcp-build /out/ /

FROM node:22-bookworm-slim AS runtime
# ca-certificates for TLS; python3 + pandas/openpyxl give the codex `code_agent`
# sandbox a spreadsheet/data toolchain (xlsx/csv parsing, aggregation) — Codex
# can't pip-install at run time inside its sandbox, so the libs must be baked in.
# (Shared image across all services; only the codex container exercises these.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates python3 python3-pandas python3-openpyxl \
 && rm -rf /var/lib/apt/lists/*
# Codex CLI runtime for the generic codex service. Auth is persisted by mounting
# CODEX_HOME in docker-compose.
RUN npm install -g @openai/codex
WORKDIR /app

# mcp-tools, agent-supervisor, codex-service, judge-worker, gmail-auth, …
COPY --from=mcp-build /out/ /usr/local/bin/

# Source. .dockerignore strips data/, storage/, .env*, target/, and
# skills/ (local-dev live overlay). The shipped skills baseline lives in
# skills.default/ which IS copied — the agent reads it as a fallback when
# the live overlay (mounted volume) has no entry for a given skill.
COPY . .

EXPOSE 3000

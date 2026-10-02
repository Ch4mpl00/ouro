# syntax=docker/dockerfile:1
# Single image for every service (mcp + agent + workers). docker-compose picks
# the command per service. Two build stages: the Rust MCP binaries, and the
# agent's node_modules (better-sqlite3 needs a native compile at install
# time). The runtime stage copies only their outputs + source.
#
# Built on a laptop and shipped to the droplet (scripts/deploy.sh): the
# droplet is 1 vCPU / 2 GB and cannot compile the crate. The image targets
# linux/amd64 while the laptop is arm64, so:
#   - the Rust stage runs on the BUILD platform and cross-compiles — native
#     speed instead of compiling under QEMU emulation;
#   - cargo's registry and target dir live in BuildKit cache mounts, so a
#     rebuild only recompiles what changed (seconds, not the 3-minute cold
#     build of ~400 crates).

# MCP server and its CLIs (crates/mcp). Bookworm matches the runtime's glibc.
FROM --platform=$BUILDPLATFORM rust:1.90-bookworm AS mcp-build
ARG TARGETARCH
# Debian's triplet-prefixed gcc (x86_64-linux-gnu-gcc, aarch64-linux-gnu-gcc)
# exists natively for the build arch; the other arch needs the cross package.
# cmake/clang cover aws-lc-rs (rustls' crypto backend).
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
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eu; . /cross-env; \
    cargo build --release --locked --bins --target "$TRIPLE"; \
    mkdir -p /out; \
    find "target/$TRIPLE/release" -maxdepth 1 -type f -perm -u+x -exec cp {} /out/ \;

FROM node:22-bookworm-slim AS deps
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 build-essential ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g pnpm@10
WORKDIR /app

# Cache deps separately from source — manifests first.
COPY pnpm-workspace.yaml pnpm-lock.yaml package.json ./
COPY packages/agent/package.json ./packages/agent/
COPY packages/codex/package.json ./packages/codex/
RUN pnpm install --frozen-lockfile

FROM node:22-bookworm-slim AS runtime
# ca-certificates for TLS; python3 + pandas/openpyxl give the codex `code_agent`
# sandbox a spreadsheet/data toolchain (xlsx/csv parsing, aggregation) — Codex
# can't pip-install at run time inside its sandbox, so the libs must be baked in.
# (Shared image across all services; only the codex container exercises these.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates python3 python3-pandas python3-openpyxl \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g pnpm@10
# Codex CLI runtime for the generic codex service. Auth is persisted by mounting
# CODEX_HOME in docker-compose.
RUN npm install -g @openai/codex
WORKDIR /app

COPY --from=deps /app/node_modules ./node_modules
COPY --from=deps /app/packages/agent/node_modules ./packages/agent/node_modules

# mcp-tools (the server), mcp-setup, gmail-auth, userbot-auth, embed-backfill, …
COPY --from=mcp-build /out/ /usr/local/bin/

# Source. .dockerignore strips data/, storage/, .env*, node_modules, and
# skills/ (local-dev live overlay). The shipped skills baseline lives in
# skills.default/ which IS copied — the agent reads it as a fallback when
# the live overlay (mounted volume) has no entry for a given skill.
COPY . .

EXPOSE 3000

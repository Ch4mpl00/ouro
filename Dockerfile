# Single image for every service (mcp + agent + workers). docker-compose picks
# the command per service. Two build stages: the Rust MCP binaries, and the
# agent's node_modules (better-sqlite3 needs a native compile at install
# time). The runtime stage copies only their outputs + source.

# MCP server and its CLIs (crates/mcp). Bookworm matches the runtime's glibc.
# cmake/clang cover aws-lc-rs (rustls' crypto backend) on any arch.
FROM rust:1.90-bookworm AS mcp-build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
RUN cargo build --release --locked --bins \
 && mkdir -p /out \
 && find target/release -maxdepth 1 -type f -perm -u+x -exec cp {} /out/ \;

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

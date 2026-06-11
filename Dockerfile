# Single image for both services (mcp + agent). docker-compose picks the
# command per service. better-sqlite3 needs a native compile step at install
# time, so we ship the build toolchain in the build stage and copy only the
# resolved node_modules + source into the runtime stage.

FROM node:22-bookworm-slim AS deps
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 build-essential ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g pnpm@10
WORKDIR /app

# Cache deps separately from source — manifests first.
COPY pnpm-workspace.yaml pnpm-lock.yaml package.json ./
COPY packages/mcp/package.json ./packages/mcp/
COPY packages/agent/package.json ./packages/agent/
COPY packages/codex/package.json ./packages/codex/
RUN pnpm install --frozen-lockfile

FROM node:22-bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g pnpm@10
# Codex CLI runtime for the generic codex service. Auth is persisted by mounting
# CODEX_HOME in docker-compose.
RUN npm install -g @openai/codex
WORKDIR /app

COPY --from=deps /app/node_modules ./node_modules
COPY --from=deps /app/packages/mcp/node_modules ./packages/mcp/node_modules
COPY --from=deps /app/packages/agent/node_modules ./packages/agent/node_modules

# Source. .dockerignore strips data/, storage/, .env*, node_modules, and
# skills/ (local-dev live overlay). The shipped skills baseline lives in
# skills.default/ which IS copied — the agent reads it as a fallback when
# the live overlay (mounted volume) has no entry for a given skill.
COPY . .

EXPOSE 3000

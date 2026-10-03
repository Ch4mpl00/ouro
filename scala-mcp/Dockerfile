# syntax=docker/dockerfile:1
# The Scala MCP server: a fat jar built once on the build machine's own
# platform (bytecode is portable — no cross-compiling), run on a JRE.
#
#   docker buildx build --platform linux/amd64 -f scala-mcp/Dockerfile -t mcp-scala:latest scala-mcp

FROM --platform=$BUILDPLATFORM eclipse-temurin:25-jdk AS build
RUN apt-get update && apt-get install -y --no-install-recommends curl && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY mill .mill-version .mill-jvm-opts build.mill ./
# Dependencies first, so a code-only change reuses this layer's caches.
RUN --mount=type=cache,target=/root/.cache \
    ./mill --no-daemon resolvedMvnDeps > /dev/null
COPY resources resources
COPY src src
RUN --mount=type=cache,target=/root/.cache \
    --mount=type=cache,target=/src/out \
    ./mill --no-daemon assembly && cp out/assembly.dest/out.jar /mcp.jar

FROM eclipse-temurin:25-jre
WORKDIR /app
COPY --from=build /mcp.jar /app/mcp.jar
# Small and steady on a 1 vCPU / 2 GB droplet: the serial collector, a capped
# heap, small thread stacks. Logs go to stderr (stdout is the stdio channel).
ENV JAVA_TOOL_OPTIONS="-XX:+UseSerialGC -Xmx256m -Xss512k -XX:+ExitOnOutOfMemoryError"
# The same command names as the Rust image, so compose and runbooks don't
# change: `mcp-tools` is the server, the rest are the CLIs in mcp.bin.
RUN set -e; \
    wrap() { printf '#!/bin/sh\nexec java -cp /app/mcp.jar %s "$@"\n' "$2" > /usr/local/bin/$1; chmod +x /usr/local/bin/$1; }; \
    wrap mcp-tools mcp.Main; \
    wrap mcp-setup mcp.bin.McpSetup; \
    wrap gmail-auth mcp.bin.GmailAuth; \
    wrap gmail-list-unread mcp.bin.GmailListUnread; \
    wrap telegram-get-chat-id mcp.bin.TelegramGetChatId; \
    wrap userbot-auth mcp.bin.UserbotAuth; \
    wrap monobank-txns mcp.bin.MonobankTxns; \
    wrap embed-backfill mcp.bin.EmbedBackfill; \
    wrap memory-import-notes mcp.bin.MemoryImportNotes; \
    wrap eval-rag mcp.bin.EvalRag; \
    wrap eval-inspect mcp.bin.EvalInspect; \
    wrap eval-snapshot mcp.bin.EvalSnapshot
CMD ["mcp-tools"]

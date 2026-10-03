# scala-mcp

The MCP server (`crates/mcp`) ported to Scala 3 + ZIO, as an experiment in
which language suits LLM-driven development better. Same tools, same
databases, same command names; findings in `.claude/tasks/rust-vs-scala.md`.

Stack: Scala 3.9, JDK 25, Mill · ZIO 2, ZIO HTTP + Tapir (Streamable HTTP
transport), zio-json · Quill + raw JDBC on Hikari, Flyway · OpenTelemetry ·
ZIO Test + Testcontainers. One domain, one file (`src/mcp/*.scala`); CLIs in
`src/mcp/bin/`.

```bash
./mill compile            # Mill fetches JDK 25 itself (//| mill-jvm-version)
./mill test               # starts a pgvector container (Testcontainers)
TEST_DATABASE_URL=postgres://… ./mill test   # or use an existing throwaway DB
MTPROTO_LIVE=1 ./mill test.testOnly mcp.MtprotoSpec   # + anonymous handshake with Telegram DC 2
./mill reformat && ./mill fix                # scalafmt, scalafix
./mill mill.bsp.BSP/install                  # Metals / IntelliJ

docker buildx build --platform linux/amd64 -f Dockerfile -t mcp-scala:latest --load .
docker compose -f ../docker-compose.yml -f ../docker-compose.scala.yml up -d --no-build mcp mcp-tunnel
```

Differences from the Rust server:

- **Migrations** run through Flyway (`resources/db/{news,state}`, the same SQL
  files). A database the drizzle/Rust migrators own is adopted by baselining
  at their history length; nothing re-runs.
- **MCP protocol** is implemented in `Server.scala` (no ZIO MCP SDK exists).
  POST replies are `application/json`, not SSE; GET (server stream) is 405.
- **MTProto** (`Mtproto.scala`) is our own client — no maintained JVM library
  exists. Proven against Telegram with an anonymous key; the userbot's reads
  over a real account session and the `userbot-auth` login have not been run
  live yet.
- **Not ported**: the one-shot sqlite importers (`import-sqlite-state`,
  `migrate-channel-posts`) — that migration is done.

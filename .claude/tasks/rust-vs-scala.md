# Rust vs Scala 3 for LLM-driven development — the MCP server, twice

**Status:** done (experiment; Scala port on `feat/scala-rewrite`, not deployed)
**Priority:** P3
**Area:** `crates/mcp` (Rust) vs `scala-mcp/` (Scala 3 + ZIO)
**Created:** 2026-10-03

## Context

The MCP server was ported from TypeScript to Rust (`feat/rust-rewrite`,
PR #25), then — by the same model, in one session — to Scala 3 + ZIO
(`feat/scala-rewrite`). The question: which language makes LLM-written code
cheaper to get right? Both ports keep one domain per file and the same tool
surface (37 default tools, the tunnel subset pinned by tests), databases and
command names.

**Caveat on fairness.** Rust went first: it paid for discovering every
behaviour (the TS quirks, the sqlite → Postgres move, deploy). Scala was a
second port with a working reference to read. Elapsed time is therefore not
comparable; the per-file error counts below are.

## Numbers

| | Rust (`crates/mcp`) | Scala (`scala-mcp`) |
|---|---|---|
| Production code (lines, incl. CLIs) | ~10 200 | ~8 100 — of which 1 070 is a hand-written MTProto client |
| Same scope minus MTProto/sqlite importers | ~9 400 | ~7 000 (≈25 % less) |
| Tests | 100 (PG tests opt-in via `TEST_DATABASE_URL`) | 111 (PG via Testcontainers, on by default) + 1 live Telegram |
| Libraries we had to write | — (rmcp = official MCP SDK, grammers = MTProto) | MCP server/client protocol (~450 lines), MTProto + TL + DH + SRP (~1 070) |
| Clean compile | cold Docker build ~4 min | 20 s (`mill compile`, deps cached); cold Docker build 2 m 36 s |
| Code-only image rebuild | ~35 s | ~7 s (bytecode: no cross-compiling) |
| Image | 1.49 GB combined with the node agent | 462 MB (JRE + 117 MB fat jar), MCP only |
| Ready after start | 0.44 s (amd64 under Rosetta) | 6.5 s under Rosetta, 1.9 s native arm64 |
| RSS idle → after 1 000 calls | 15 MB → 39 MB | 300 MB → 360 MB native (`-Xmx256m`, SerialGC); 364 → 488 MB emulated |
| 1 000 mixed tool calls, 8 clients | 185 rps, p50 3.6 ms | 178 rps, p50 3.7 ms native (124 rps emulated) |

## What the compilers caught

**Rust session** (from the PR history): crate conflicts at link time
(`sqlx` vs `rusqlite` on `libsqlite3-sys`; `libsql` vs bundled sqlite as
duplicate symbols — only in the Linux build), a pre-release transitive
(`glass_pumpkin` rc1) breaking the build, `.await` inside a `tracing` macro
making a future non-`Send`, a closure lifetime error, clippy rounds. Each was
real friction; several needed reading generated code or dependency trees.

**Scala session**: ~12 compile rounds total. Typical failures were cheap and
local — a guessed API (`Header.UserAgent.Custom`, `Quill.Postgres(...)`
returning a narrower type), a name shadowed by a wildcard import
(`zio.Scheduler`, `zio.System`), inference picking `Nothing` for an
`if/else` with a failing branch (→ "ambiguous given JsonEncoder"), an enum
field clashing with a method. **Memory.scala (1 480 lines), News, Gmail,
Knowledge, Skills, Gateway and Main compiled on the first attempt.**

## What the compilers did not catch (and what did)

| Bug | Found by |
|---|---|
| Quill's `lift(opt).forall(...)` binds an untyped `? IS NULL`; Postgres refuses to plan it | Testcontainers test |
| Quill wraps a raw `UPDATE … RETURNING` in `SELECT … FROM (…)` | compile-time SQL log |
| Fat jar overwrote `META-INF/services` → Flyway "unsupported database" | first smoke run |
| MTProto `msg_id` computed with silent `Long` overflow | reasoning while debugging the live handshake |
| TL `string` decoded as UTF-8 corrupted binary fields (pq, DH prime) | live handshake against DC 2 |
| RSA modulus typed from memory was wrong (a hallucinated constant) | fingerprint test; replaced by parsing the PEM |
| `PBEKeySpec` re-encodes a binary password as UTF-8 chars | reading the JDK docs before trusting it |
| Rust: `dotenvy` walking up to a parent `.env` with real credentials | a smoke test that touched the real Telegram account |

Neither type system caught protocol or data-semantics bugs. **Tests that
exercise the real thing did** — Testcontainers Postgres, a live anonymous
MTProto handshake, a fingerprint check against a known constant.

## Findings

1. **Iteration cost favours Scala.** Errors were local and fast to fix,
   incremental compiles take seconds, and ~25 % less code for the same
   behaviour. Rust's failures cost more per round (link-time conflicts,
   `Send`/lifetime puzzles, slow release builds) — though each one it caught
   is a class of bug Scala would ship (data races, use-after-move).
2. **The ecosystem decided more than the language.** Rust had an official MCP
   SDK and a working MTProto client; on the JVM both had to be written (≈1 500
   lines), and the MTProto one carried three of the subtlest bugs. An LLM can
   write a protocol client, but it will misremember constants and test vectors
   — every one of them needs an external check.
3. **Runtime footprint favours Rust by an order of magnitude** (15–40 MB vs
   300–360 MB). On the 1 vCPU / 2 GB droplet, which also runs Postgres and the
   node agent, that is the difference between "free" and "the size of the old
   TS server".
4. **Test infrastructure mattered more than either compiler.** Testcontainers
   made every Postgres test run by default; it caught a bug in the first run.
   The Rust suite skips those tests unless `TEST_DATABASE_URL` is set, so an
   agent iterating locally sees green without exercising SQL.
5. **FP without heavy types worked.** Plain `Task` + one typed domain error
   (`MemoryError`), `Ref`/`Semaphore` for state, fibers in the app scope, no
   tagless-final, one `provide` in `Main`. Tool definitions are values, which
   made the toolset-surface tests trivial.

## Recommendation

Keep Rust for this deployment: it is already ported, ships the official MCP
SDK and an MTProto client, and costs a tenth of the memory on a 2 GB box.
Take two lessons into the Rust codebase regardless of language:

- Run the Postgres tests by default (a Testcontainers-equivalent, e.g. the
  `testcontainers` crate), so "green" means the SQL ran.
- For anything an LLM recalls from memory — constants, keys, test vectors,
  wire formats — derive or verify it in a test instead of trusting the recall.

If memory were not a constraint (or GraalVM native-image were adopted),
Scala 3 + ZIO would be the faster language to *write* this kind of I/O glue
with an LLM.

## Notes

- Scala unknowns: the userbot's reads over a real account session and the
  `userbot-auth` login are written but not yet run against the account.
- Emulated numbers are amd64 images under Rosetta on an M-series Mac; native
  Scala numbers are from the same jar on arm64. Rust was not rebuilt natively.

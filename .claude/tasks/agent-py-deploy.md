# Deploy the LangGraph (Python) agent alongside the TS supervisor

**Status:** in-progress
**Priority:** P1
**Area:** agent-py / deploy
**Created:** 2026-06-04

## Context

`agent-py/` is the Python + LangGraph/LangChain port of `packages/agent`
(see `agent-py/COMPARISON.md`). Step 1 of migrating the live agent onto the
framework: ship it as its own container in the existing compose, talking to
the same MCP. Decisions taken with the user:

- **One consumer at a time.** `get_next_signal` is a destructive pop, so two
  live supervisors would split the signal stream. On the droplet the Python
  agent runs; the TS `agent` stays in the repo but is parked behind the
  compose `legacy-ts` profile (a plain `docker compose up` skips it). No
  shadow/fanout in step 1.
- **Vendored into the monorepo** as top-level `agent-py/` (not under
  `packages/*`, so pnpm-workspace ignores it) → ships with the same
  `git pull && docker compose up -d --build`.
- **uv** for Python packaging; committed `uv.lock` pins langchain 1.x /
  langgraph 1.x (the set the port was actually tested on).

## Done in this step

- Vendored `/Users/dimas/Code/langgraph` → `agent-py/` (no `.venv`, db, or
  live `skills/`).
- `pyproject.toml`: corrected the stale "0.3.x/0.2.x" comment to 1.x, set
  `<2` ceilings on the langchain/langgraph family, added an explicit
  `[tool.hatch.build.targets.wheel]` (`agent_py`). Generated `uv.lock`
  (resolves to the exact tested versions: langchain 1.3.4, langgraph 1.2.4,
  pydantic 2.13.4, …).
- `agent-py/Dockerfile` (uv image, two-stage `uv sync --frozen`) +
  `.dockerignore`.
- `docker-compose.yml`: new `agent-py` service (http → `mcp:3000`, reuses
  `.env.agent`, volumes `agent-py-data` + `agent-py-skills`); TS `agent`
  moved behind `profiles: ["legacy-ts"]`.
- Hardened `agent_py/mcp_client.py`: `connect_mcp` retries and then **raises**
  instead of degrading to a stand-in (a silent stand-in in prod = a signal
  black hole). Transports: `http` (default) | `stdio`.

## Cleanup pass (prod-readiness)

Stripped the side-by-side-study surface so the package reads as a real
component: removed `COMPARISON.md`, the offline mock MCP + `scripts/dry_run.py`,
and all "vs TS / port of src/*.ts / паритет" comparison comments; rewrote
docstrings in English (matching the TS half of the repo); deleted dead code
(`Session.close`, `VariableStore.has/.snapshot/.set`, `is_preset_name`,
`SYNTHETIC_TOOL_NAMES`, `_sub_counter`); modernized typing (`Optional[X]` →
`X | None`); `requires-python` → 3.12; `main()` now hard-fails if LLM keys are
missing (no mock to fall back to). Lint/format with ruff (`E,F,I,UP`), types
with pyright.

## Acceptance (remaining for "step 1 fully live")

- On the droplet: `docker compose down` (to stop any running TS `agent`),
  then `docker compose up -d --build` → postgres + mcp + `agent-py` up, TS
  `agent` not running. Confirm `agent-py` logs `mcp tools: …` (real HTTP
  connect, not the mock-fallback message — which can no longer happen
  silently).
- A real signal (e.g. a Telegram message) is consumed by `agent-py` end to
  end with live LLM keys.

## Notes / follow-ups (not in step 1)

- **Live-LLM path unproven.** Per COMPARISON §5, `llm_compose` / `llm_agent`
  were never run against real provider keys. First real signals are the test;
  watch the compiler (planner.md structured output) and sub-agent sessions.
- **Model ids.** `.env.agent` sets only the keys, so the code defaults
  (`gpt-5.4-mini`, `deepseek-v4-pro`, `gpt-5.4`) apply to BOTH agents. If
  those aren't the intended live ids, set `AGENT_*_MODEL` in `.env.agent`.
- **Skills overlay carry-over.** `agent-py` starts from a fresh
  `agent-py-skills` volume → falls back to `skills.default/`. Any
  dreaming-evolved skills in the TS `agent-skills` volume do NOT carry over.
  Decide later whether to copy them in or share one volume.
- **Cutover / rollback.** Live one is chosen by which service compose starts:
  `agent-py` by default; TS via `docker compose --profile legacy-ts up -d agent`
  (stop the other first — never both).
- **Shadow mode** (both agents see every signal for a true A/B) would need a
  peek/fanout in the MCP signals queue — out of scope here.

# tasks/

Tech-debt and planned work that we agreed on but haven't picked up yet.
One markdown file per task. Files stay here while pending or in-progress;
move to `tasks/done/` when finished (or delete — git history keeps the
record).

## File format

```markdown
# <short title>

**Status:** pending | in-progress | done
**Priority:** P0 | P1 | P2 | P3
**Area:** <module / concern>
**Created:** YYYY-MM-DD

## Context
Why this matters. What problem it solves, what it unblocks, what
incident or design discussion it came from.

## Acceptance
What "done" looks like — concrete, checkable.

## Notes
Optional: alternatives considered, links, open questions.
```

Keep priorities honest:

- **P0** — blocking or actively burning ($-leak, prod outage path).
- **P1** — important architectural debt that gates further growth.
- **P2** — quality-of-life / nice to have.
- **P3** — write it down so we don't forget; no commitment to do it.

module versions {
  import types.* from "./types"
  pure def activateVersion(c: Catalog, v: int): Catalog =
    if (v == c.active) c else { ...c, active: v, previous: c.active, faults: 0 }
  pure def breaker(c: Catalog, version: int, status: int): Catalog =
    if (version != c.active) c
    else if (status == 4) { ...c, faults: 0 }
    else if (status != 5) c
    else if (c.faults + 1 < threshold) { ...c, faults: c.faults + 1 }
    else { ...c, active: c.previous, previous: 0, faults: 0 }
  pure def breakerCorrect(before: Catalog, after: Catalog, version: int, status: int): bool =
    if (version != before.active or (status != 4 and status != 5)) after == before
    else if (status == 4) after.active == before.active and after.previous == before.previous and after.faults == 0
    else if (before.faults == threshold - 1)
      after.active == before.previous and after.previous == 0 and after.faults == 0
    else after.active == before.active and after.previous == before.previous and after.faults == before.faults + 1
}

module version_check {
  import types.* from "./types"
  import versions.* from "./versions"
  var catalog: Catalog
  var pinned: List[int]
  var finished: List[bool]
  var correct: bool
  // Reachable by save/activate v1, admit r0, save/activate v2, admit r1/r2.
  action init = all {
    catalog' = { count: 2, active: 2, previous: 1, faults: 0 },
    pinned' = [1,2,2], finished' = [false,false,false], correct' = true
  }
  action finish = {
    nondet r = RUNS.oneOf()
    nondet status = Set(4,5,6).oneOf()
    nondet fault = Set(0,1,2).oneOf()
    val updated = if (fault == 1) catalog else breaker(catalog, pinned.nth(r), status)
    all {
      not(finished.nth(r)),
      catalog' = updated, pinned' = pinned,
      finished' = if (fault == 1) finished else finished.replaceAt(r, true),
      correct' = (correct and (fault == 1 or breakerCorrect(catalog, updated, pinned.nth(r), status)))
    }
  }
  action activate = {
    nondet v = Set(1,2).oneOf()
    all { catalog' = activateVersion(catalog, v), pinned' = pinned, finished' = finished, correct' = correct }
  }
  action step = any { finish, activate }
  val safety = correct and pinned == [1,2,2] and catalog.faults < threshold
}

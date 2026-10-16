Feature: Project documents in shared memory
  Several agents (the supervisor, Claude Code, ChatGPT) edit the same markdown
  documents. Edits quote exact text instead of line numbers, every write is
  versioned, and nothing an agent does can silently destroy someone else's text.

  Background:
    Given a project "graphs" titled "Graphs for interviews"
    And "roadmap.md" in "graphs" reads:
      """
      # Roadmap

      - [ ] BFS
      - [ ] Dijkstra
      """

  Scenario: Appending needs no version and never touches existing text
    When I call "append_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "text": "- [ ] A*" }
      """
    Then the result is:
      | field   | value |
      | ok      | true  |
      | version | 2     |
    And "roadmap.md" in "graphs" reads:
      """
      # Roadmap

      - [ ] BFS
      - [ ] Dijkstra

      - [ ] A*
      """

  Scenario: A patch replaces exactly the quoted text
    When I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] BFS", "new": "- [x] BFS" }] }
      """
    Then the result is:
      | field   | value |
      | ok      | true  |
      | version | 2     |
    And "roadmap.md" in "graphs" reads:
      """
      # Roadmap

      - [x] BFS
      - [ ] Dijkstra
      """

  Scenario: A patch based on a stale read is refused and changes nothing
    Given I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] BFS", "new": "- [x] BFS" }] }
      """
    When I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] Dijkstra", "new": "- [x] Dijkstra" }] }
      """
    Then the result is:
      | field          | value              |
      | ok             | false              |
      | error          | "version_conflict" |
      | currentVersion | 2                  |
    And "roadmap.md" in "graphs" is at version 2

  Scenario: When a quote does not match, the agent is shown what nearly matched
    When I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] dijkstra", "new": "- [x] Dijkstra" }] }
      """
    Then the result includes:
      """
      { "ok": false, "error": "edit_failed", "applied": false,
        "failures": [{ "reason": "not_found", "suggestions": ["- [ ] Dijkstra"] }] }
      """
    And "roadmap.md" in "graphs" is at version 1

  Scenario: All edits in one patch apply together or not at all
    When I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] BFS", "new": "- [x] BFS" },
                  { "old": "- [ ] Floyd", "new": "- [x] Floyd" }] }
      """
    Then the result is:
      | field            | value         |
      | error            | "edit_failed" |
      | failures.0.index | 1             |
    And "roadmap.md" in "graphs" is at version 1

  Scenario: Overwriting a whole document requires having read it
    When I call "write_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "body": "gone\n" }
      """
    Then the result is:
      | field          | value              |
      | error          | "version_required" |
      | currentVersion | 1                  |
    And "roadmap.md" in "graphs" is at version 1

  Scenario: The newest change can always be undone exactly
    Given I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] BFS", "new": "- [x] BFS" }] }
      """
    And I remember "patchId" as "bfs"
    When I call "revert_patch" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "patch_id": "{{bfs}}" }
      """
    Then the result is:
      | field   | value |
      | ok      | true  |
      | version | 3     |
    And "roadmap.md" in "graphs" reads:
      """
      # Roadmap

      - [ ] BFS
      - [ ] Dijkstra
      """

  Scenario: An older change is undone in place when later edits only extended its text
    Given I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] Dijkstra", "new": "- [x] Dijkstra" }] }
      """
    And I remember "patchId" as "done"
    And I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 2,
        "edits": [{ "old": "- [x] Dijkstra", "new": "- [x] Dijkstra (revisit)" }] }
      """
    When I call "revert_patch" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "patch_id": "{{done}}" }
      """
    Then "roadmap.md" in "graphs" reads:
      """
      # Roadmap

      - [ ] BFS
      - [ ] Dijkstra (revisit)
      """

  Scenario: An older change whose text was rewritten later can only be rolled back
    Given I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 1,
        "edits": [{ "old": "- [ ] Dijkstra", "new": "- [x] Dijkstra" }] }
      """
    And I remember "patchId" as "done"
    And I call "patch_doc" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "expected_version": 2,
        "edits": [{ "old": "- [x] Dijkstra", "new": "- [x] Shortest paths" }] }
      """
    When I call "revert_patch" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "patch_id": "{{done}}" }
      """
    Then the result is:
      | field             | value             |
      | error             | "revert_conflict" |
      | rollbackToVersion | 1                 |
    When I call "revert_patch" with:
      """
      { "project": "graphs", "doc": "roadmap.md", "patch_id": "{{done}}", "rollback": true }
      """
    Then "roadmap.md" in "graphs" reads:
      """
      # Roadmap

      - [ ] BFS
      - [ ] Dijkstra
      """

  Scenario: Unknown names are answered with what does exist
    When I call "read_doc" with:
      """
      { "project": "graphs", "doc": "notes.md" }
      """
    Then the result includes:
      """
      { "ok": false, "error": "doc_not_found", "docs": ["roadmap.md"] }
      """

Feature: The journal
  A journal stream is a named category of operational records — the last
  digests, a run log — that agents append to and read back by recency. It is
  not knowledge: entries are never edited, never searched by meaning and keep
  no history. A stream holds at most its capacity; the oldest entries leave
  first.

  Scenario: The newest entries come back first
    Given I call "journal_append" with:
      """
      { "stream": "news-digest", "body": "Monday digest" }
      """
    And I call "journal_append" with:
      """
      { "stream": "news-digest", "body": "Tuesday digest" }
      """
    When I call "journal_tail" with:
      """
      { "stream": "news-digest", "n": 2 }
      """
    Then the result is:
      | field          | value            |
      | ok             | true             |
      | entries.0.body | "Tuesday digest" |
      | entries.1.body | "Monday digest"  |

  Scenario: Metadata is kept with the entry, and the same metadata twice is two entries
    Given I call "journal_append" with:
      """
      { "stream": "ouro/activity", "body": "Sent the digest", "metadata": { "trace_id": "telegram:7" } }
      """
    And I call "journal_append" with:
      """
      { "stream": "ouro/activity", "body": "Sent the digest", "metadata": { "trace_id": "telegram:7" } }
      """
    When I call "journal_tail" with:
      """
      { "stream": "ouro/activity", "n": 10 }
      """
    Then the result is:
      | field                       | value         |
      | ok                          | true          |
      | entries.0.metadata.trace_id | "telegram:7"  |
      | entries.1.metadata.trace_id | "telegram:7"  |

  Scenario: A stream nobody wrote to reads as empty
    When I call "journal_tail" with:
      """
      { "stream": "never-written" }
      """
    Then the result includes:
      """
      { "ok": true, "entries": [] }
      """

  Scenario: A stream keeps only its capacity, dropping the oldest
    Given I call "journal_append" with:
      """
      { "stream": "digest/news", "body": "first", "capacity": 2 }
      """
    And I call "journal_append" with:
      """
      { "stream": "digest/news", "body": "second" }
      """
    And I call "journal_append" with:
      """
      { "stream": "digest/news", "body": "third" }
      """
    When I call "journal_tail" with:
      """
      { "stream": "digest/news", "n": 10 }
      """
    Then the result includes:
      """
      { "ok": true, "entries": [{ "body": "third" }, { "body": "second" }] }
      """

  Scenario: Streams are listed with their capacity and size
    Given I call "journal_append" with:
      """
      { "stream": "news-digest", "body": "Monday digest", "capacity": 2 }
      """
    When I call "list_memory"
    Then the result includes:
      """
      { "ok": true, "journals": [{ "name": "news-digest", "capacity": 2, "size": 1 }] }
      """

  Scenario: Journal entries are not found by recall
    Given I call "journal_append" with:
      """
      { "stream": "news-digest", "body": "Power outages in Odesa until Friday" }
      """
    When I call "recall" with:
      """
      { "query": "power outages in Odesa" }
      """
    Then the result includes:
      """
      { "ok": true, "hits": [] }
      """

  Scenario: A malformed stream name is refused and nothing is written
    When I call "journal_append" with:
      """
      { "stream": "News//Digest", "body": "x" }
      """
    Then the result includes:
      """
      { "ok": false, "error": "invalid_stream" }
      """
    When I call "list_memory"
    Then the result includes:
      """
      { "ok": true, "journals": [] }
      """

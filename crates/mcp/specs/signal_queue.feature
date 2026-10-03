Feature: The signal queue
  Pollers turn external events into signals; the agent takes them one at a
  time with get_next_signal. However many agents ask at once, every signal is
  handed out exactly once, oldest first.

  Scenario: Signals come out oldest first
    Given the signal queue holds:
      | source    | content |
      | telegram  | first   |
      | gmail     | second  |
    When I call "get_next_signal"
    Then the result is:
      | field              | value      |
      | signal.content     | "first"    |
      | signal.source      | "telegram" |
      | pendingAfter       | 1          |

  Scenario: An empty queue says so
    When I call "get_next_signal"
    Then the result is:
      | field        | value |
      | signal       | null  |
      | pendingAfter | 0     |

  Scenario: Many agents at once never get the same signal twice
    Given the signal queue holds 200 signals
    When 10 agents take signals at the same time until the queue is empty
    Then every signal was delivered exactly once

  Scenario: Listing past signals never consumes them
    Given the signal queue holds:
      | source   | content |
      | telegram | a       |
      | gmail    | b       |
    When I call "list_signals" with:
      """
      { "source": "gmail" }
      """
    Then the result includes:
      """
      { "count": 1, "signals": [{ "content": "b", "consumed_at": null }] }
      """
    And 2 signals are waiting

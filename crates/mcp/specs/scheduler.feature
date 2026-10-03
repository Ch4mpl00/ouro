Feature: Scheduled tasks
  A task is a cron expression on the user's wall clock. When its next slot has
  passed, one signal is queued for that slot — never twice, even after a
  restart or a late tick.

  Scenario Outline: A task fires once its slot has passed, in the user's time zone
    Given the user's time zone is "<zone>"
    And a recurring task "<cron>" saying "tick" created at "<created>"
    When the scheduler ticks at "<now>"
    Then 1 signal is waiting
    And the next signal is a "scheduler" signal for the slot "<slot>"

    Examples:
      | zone        | cron        | created              | now                  | slot                     |
      | UTC         | 0 9 * * *   | 2026-07-01T00:00:00Z | 2026-07-01T09:01:00Z | 2026-07-01T09:00:00.000Z |
      | Europe/Kiev | 0 9 * * *   | 2026-07-01T00:00:00Z | 2026-07-01T06:01:00Z | 2026-07-01T06:00:00.000Z |
      | UTC         | 0 0 1 * 1   | 2026-10-02T00:00:00Z | 2026-10-05T00:00:30Z | 2026-10-05T00:00:00.000Z |

  Scenario Outline: A task does not fire before its slot
    Given the user's time zone is "<zone>"
    And a recurring task "<cron>" saying "tick" created at "<created>"
    When the scheduler ticks at "<now>"
    Then 0 signals are waiting

    Examples:
      | zone        | cron      | created              | now                  |
      | UTC         | 0 9 * * * | 2026-07-01T00:00:00Z | 2026-07-01T08:59:00Z |
      | Europe/Kiev | 0 9 * * * | 2026-07-01T00:00:00Z | 2026-07-01T05:59:00Z |

  Scenario: A late tick catches up one slot at a time
    Given the user's time zone is "UTC"
    And a recurring task "0 * * * *" saying "hourly" created at "2026-10-01T00:00:00Z"
    And it last fired for the slot "2026-10-02T10:00:00Z"
    When the scheduler ticks at "2026-10-02T13:05:00Z"
    Then 1 signal is waiting
    And the next signal is a "scheduler" signal for the slot "2026-10-02T11:00:00.000Z"

  Scenario: A one-shot task fires once and is gone
    Given the user's time zone is "UTC"
    And a one-shot task "30 14 2 10 *" saying "take pills" created at "2026-10-01T00:00:00Z"
    When the scheduler ticks at "2026-10-02T14:31:00Z"
    And the scheduler ticks at "2026-10-02T14:40:00Z"
    Then 1 signal is waiting
    When I call "list_scheduled_tasks"
    Then the result is:
      | field | value |
      | count | 0     |

  Scenario: An invalid cron expression is refused with a reason
    When I call "schedule_task" with:
      """
      { "cron_expr": "every day at nine", "recurring": true, "prompt": "x" }
      """
    Then the result is:
      | field | value |
      | ok    | false |

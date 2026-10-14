Feature: Standalone facts and recall
  A fact is one self-contained statement. Recall searches facts and document
  fragments by meaning and returns references to load; archiving takes a fact
  out of ordinary recall without deleting it.

  Scenario: A remembered fact is found by meaning
    Given I call "remember" with:
      """
      { "body": "Alex pays for the internet on the first of the month", "tags": ["internet"] }
      """
    And I remember "fact.id" as "fact"
    When I call "recall" with:
      """
      { "query": "when does Alex pay for internet" }
      """
    Then the result is:
      | field      | value         |
      | ok         | true          |
      | hits.0.ref | fact:{{fact}} |

  Scenario: An archived fact drops out of recall but can still be read
    Given I call "remember" with:
      """
      { "body": "The router password is on the sticker underneath" }
      """
    And I remember "fact.id" as "fact"
    And I call "update_fact" with:
      """
      { "id": {{fact}}, "state": "archived" }
      """
    When I call "recall" with:
      """
      { "query": "router password sticker" }
      """
    Then the result includes:
      """
      { "ok": true, "hits": [] }
      """
    When I call "get_fact" with:
      """
      { "id": {{fact}} }
      """
    Then the result is:
      | field      | value      |
      | fact.state | "archived" |

  Scenario: Recall finds a document fragment, not just facts
    Given a project "trips" titled "Trips"
    And "plan.md" in "trips" reads:
      """
      ## Lisbon

      Book the tram 28 tickets before the trip.
      """
    When I call "recall" with:
      """
      { "query": "Lisbon tram tickets" }
      """
    Then the result is:
      | field      | value                |
      | hits.0.ref | doc:trips/plan.md#0  |

  Scenario: A fact needs a body
    When I call "remember" with:
      """
      { "body": "" }
      """
    Then the call is rejected as invalid parameters

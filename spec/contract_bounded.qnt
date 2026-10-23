// Reachable call fixture; uses the normative transition and shadow unchanged.
module contract_bounded {
  import contract.* from "./contract"
  export contract.*
  pure val admittedFixture = transition(initial,"admit",0,0,0,1)
  pure val fixture = transition(admittedFixture,"claim",0,0,0,0)
  action callsInit = all {
    m'=fixture,
    ideal'=fixture.observations.foldl(emptyIdeal,(a,e) => idealEvent(a,e))
  }
  action callsStep = {
    nondet op = Set("intent","dispatch","result","recover","checkpoint","crash","tick","claim").oneOf()
    nondet w = Set(0,1).oneOf()
    nondet fault = Set(0,1,2).oneOf()
    execute(op,0,w,0,0,fault)
  }
  val combinedCheck = (if (checked) 1 else 0) == 1
}

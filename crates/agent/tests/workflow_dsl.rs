use agent::workflow::*;
use serde_json::{Value, json};
#[test]
fn ts_schema_cases_accept_and_reject_the_same_plans() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/workflow-schema.json")).unwrap();
    let names = |key: &str| fixture[key].as_array().unwrap().iter().map(|v| v.as_str().unwrap().into()).collect();
    let schema = WorkflowSchema::new(names("tools"), names("skills")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let seeds = case
            .get("seeds")
            .and_then(Value::as_array)
            .map(|s| s.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>());
        let result = schema.parse(case["input"].clone(), seeds.as_deref());
        assert_eq!(result.is_ok(), case["expected"]["ok"], "{}: {:?}", case["name"], result);
        if case.get("seeds").is_some()
            && let Err(errors) = &result
        {
            assert_eq!(json!(errors), case["expected"]["errors"], "{}", case["name"]);
        }
        if let Ok(workflow) = result {
            assert_eq!(serde_json::to_value(workflow).unwrap(), case["expected"]["workflow"], "{}", case["name"]);
        }
    }
}
#[test]
fn ts_variable_cases_preserve_types_and_own_properties() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/workflow-variables.json")).unwrap();
    let store = VariableStore::new(fixture["initial"].as_object().unwrap().clone());
    for case in fixture["cases"].as_array().unwrap() {
        let result = substitute(&case["value"], &store);
        assert_eq!(result.is_ok(), case["ok"], "{}", case["value"]);
        match result {
            Ok(result) => assert_eq!(result, case["result"]),
            Err(error) => assert_eq!(error.to_string(), case["error"].as_str().unwrap()),
        }
    }
    assert!(store.set("env", json!(0)).is_err());
    let mut snapshot = store.snapshot();
    snapshot.clear();
    assert!(store.has("env"));
}
#[test]
fn schema_requires_nonempty_registries_and_binding_errors_are_classified() {
    assert!(WorkflowSchema::new(vec![], vec!["s".into()]).is_err());
    assert!(WorkflowSchema::new(vec!["t".into()], vec![]).is_err());
    assert_eq!(classify_error(&WorkflowError::Missing("a".into()).into()), "missing_binding");
    assert_eq!(classify_error(&WorkflowError::Duplicate("a".into()).into()), "duplicate_binding");
    assert_eq!(classify_error(&anyhow::anyhow!("other")), "step_failed");
}

// Port of src/workflow/dsl.test.ts, one test per case. `WorkflowSchema.parse`
// is both the TS `WorkflowSchema.safeParse` and `parseWorkflow`: it returns
// the formatted errors (`formatWorkflowErrors`) and always runs the
// post-checks. The TS workflowToJsonSchema export was inspection-only and has
// no Rust twin. The fixture tests at the end ("Rust-only") replay cases
// captured from the TS implementation itself.
use agent::workflow::*;
use serde_json::{Value, json};

const FAKE_TOOLS: [&str; 6] =
    ["list_news", "send_telegram_message", "set_memory", "get_telegram_chat_history", "search_news", "start_typing"];
const FAKE_SKILLS: [&str; 4] = ["news-digest", "tech-digest", "news-query", "telegram"];

fn schema() -> WorkflowSchema {
    WorkflowSchema::new(FAKE_TOOLS.map(String::from).to_vec(), FAKE_SKILLS.map(String::from).to_vec()).unwrap()
}

fn parse(input: Value) -> Result<Workflow, Vec<String>> {
    schema().parse(input, None)
}

fn errors(input: Value) -> Vec<String> {
    parse(input).expect_err("the plan should be rejected")
}

fn plan(steps: Value) -> Value {
    json!({"version": 1, "steps": steps})
}

// ── createWorkflowSchema ────────────────────────────────────────────────────

#[test]
fn rejects_empty_known_tools_or_known_skills_at_factory_time() {
    assert!(WorkflowSchema::new(vec![], vec!["x".into()]).err().unwrap().to_string().contains("knownTools"));
    assert!(WorkflowSchema::new(vec!["x".into()], vec![]).err().unwrap().to_string().contains("knownSkills"));
}

// ── plan validation — happy path per step kind ──────────────────────────────

#[test]
fn accepts_a_tool_step() {
    let p = plan(json!([
        {"kind":"tool","tool":"send_telegram_message","args":{"chatId":285083560,"text":"hi"},"bind":"sent"},
        {"kind":"terminal"},
    ]));
    assert!(parse(p).is_ok());
}

#[test]
fn accepts_llm_compose_with_skill_only() {
    let p = plan(json!([
        {"kind":"llm_compose","preset":"smart","skill":"news-digest","input":{"posts":"${posts}"},"bind":"digest"},
        {"kind":"terminal"},
    ]));
    assert!(parse(p).is_ok());
}

#[test]
fn accepts_llm_compose_with_prompt_only() {
    let p = plan(json!([
        {"kind":"llm_compose","preset":"base","prompt":"Summarize in one sentence: ${text}","input":{"text":"${some_input}"},"bind":"summary"},
        {"kind":"terminal"},
    ]));
    assert!(parse(p).is_ok());
}

#[test]
fn rejects_llm_compose_with_neither_skill_nor_prompt() {
    let msgs = errors(plan(json!([
        {"kind":"llm_compose","preset":"base","input":{},"bind":"out"},
        {"kind":"terminal"},
    ])))
    .join(" | ");
    assert!(regex::Regex::new("skill.*prompt|prompt.*skill").unwrap().is_match(&msgs), "{msgs}");
}

#[test]
fn post_check_walks_into_parallel_steps() {
    let errs = errors(plan(json!([
        {"kind":"parallel","steps":[
            {"kind":"tool","tool":"list_news","args":{},"bind":"a"},
            {"kind":"llm_compose","preset":"base","input":{},"bind":"b"},
        ]},
        {"kind":"terminal"},
    ])));
    assert!(errs.iter().any(|e| e.contains("steps[0].steps[1]")), "{errs:?}");
}

#[test]
fn accepts_llm_agent_with_bounded_tools_and_iterations() {
    let p = plan(json!([
        {"kind":"llm_agent","preset":"smart","skill":"news-query","prompt":"${signal.body}","tools":["search_news","list_news"],"maxIterations":5,"bind":"answer"},
        {"kind":"terminal"},
    ]));
    assert!(parse(p).is_ok());
}

#[test]
fn accepts_parallel_with_leaf_steps_inside() {
    let p = plan(json!([
        {"kind":"parallel","steps":[
            {"kind":"tool","tool":"list_news","args":{"source":"channel"},"bind":"posts"},
            {"kind":"tool","tool":"get_telegram_chat_history","args":{"chatId":1,"limit":5},"bind":"history"},
        ]},
        {"kind":"terminal"},
    ]));
    assert!(parse(p).is_ok());
}

#[test]
fn accepts_a_bare_terminal_step() {
    assert!(parse(plan(json!([{"kind":"terminal"}]))).is_ok());
}

#[test]
fn accepts_a_replan_step_with_context_and_note() {
    let p = plan(json!([
        {"kind":"tool","tool":"get_telegram_chat_history","args":{"chatId":1,"limit":10},"bind":"history"},
        {"kind":"replan","context":["history"],"note":"decide what to continue"},
    ]));
    assert!(parse(p).is_ok());
}

#[test]
fn accepts_a_replan_step_without_a_note() {
    let p = plan(json!([
        {"kind":"tool","tool":"list_news","args":{},"bind":"n"},
        {"kind":"replan","context":["n"]},
    ]));
    assert!(parse(p).is_ok());
}

// ── plan validation — decide step ───────────────────────────────────────────

fn decide(extra: Value) -> Value {
    let mut step = json!({"kind":"decide","preset":"filter","items":"${posts.items}","prompt":"rules","bind":"kept"});
    for (k, v) in extra.as_object().unwrap() {
        if v.is_null() {
            step.as_object_mut().unwrap().remove(k);
        } else {
            step[k] = v.clone();
        }
    }
    plan(json!([step, {"kind":"terminal"}]))
}

#[test]
fn accepts_a_filter_step_with_or_without_a_threshold_also_inside_parallel() {
    assert!(parse(decide(json!({}))).is_ok());
    assert!(parse(decide(json!({"threshold":0.4}))).is_ok());
    let step = json!({"kind":"decide","preset":"filter","items":"${a}","prompt":"r","bind":"x"});
    let mut other = step.clone();
    other["items"] = json!("${b}");
    other["bind"] = json!("y");
    assert!(parse(plan(json!([{"kind":"parallel","steps":[step, other]}, {"kind":"terminal"}]))).is_ok());
}

#[test]
fn rejects_an_unknown_preset_a_threshold_outside_0_1_a_missing_prompt_and_a_skill() {
    assert!(parse(decide(json!({"preset":"classify"}))).is_err());
    assert!(parse(decide(json!({"threshold":1}))).is_err());
    assert!(parse(decide(json!({"threshold":0}))).is_err());
    assert!(parse(decide(json!({"prompt":null}))).is_err());
    assert!(parse(decide(json!({"skill":"news-digest"}))).is_err());
}

// ── plan validation — rejections ────────────────────────────────────────────

#[test]
fn rejects_an_unknown_tool_name() {
    let msgs = errors(plan(json!([{"kind":"tool","tool":"list_things","args":{},"bind":"x"}, {"kind":"terminal"}])))
        .join(" | ");
    assert!(regex::Regex::new("(?i)list_things|Invalid enum").unwrap().is_match(&msgs), "{msgs}");
}

#[test]
fn rejects_an_unknown_skill_name_on_llm_agent() {
    assert!(parse(plan(json!([
        {"kind":"llm_agent","preset":"smart","skill":"ghost-skill","prompt":"x","tools":["search_news"],"maxIterations":3,"bind":"y"},
        {"kind":"terminal"},
    ])))
    .is_err());
}

#[test]
fn rejects_an_unknown_preset() {
    assert!(
        parse(plan(json!([
            {"kind":"llm_compose","preset":"genius","prompt":"x","input":{},"bind":"y"},
            {"kind":"terminal"},
        ])))
        .is_err()
    );
}

#[test]
fn rejects_nested_parallel() {
    assert!(
        parse(plan(json!([
            {"kind":"parallel","steps":[
                {"kind":"parallel","steps":[
                    {"kind":"tool","tool":"list_news","args":{},"bind":"a"},
                    {"kind":"tool","tool":"search_news","args":{},"bind":"b"},
                ]},
                {"kind":"tool","tool":"set_memory","args":{},"bind":"c"},
            ]},
            {"kind":"terminal"},
        ])))
        .is_err()
    );
}

#[test]
fn rejects_parallel_with_a_single_step() {
    assert!(
        parse(plan(json!([
            {"kind":"parallel","steps":[{"kind":"tool","tool":"list_news","args":{},"bind":"a"}]},
            {"kind":"terminal"},
        ])))
        .is_err()
    );
}

#[test]
fn rejects_a_replan_step_with_an_empty_context() {
    assert!(
        parse(plan(json!([
            {"kind":"tool","tool":"list_news","args":{},"bind":"n"},
            {"kind":"replan","context":[]},
        ])))
        .is_err()
    );
}

#[test]
fn rejects_replan_nested_inside_parallel() {
    assert!(
        parse(plan(json!([
            {"kind":"parallel","steps":[
                {"kind":"tool","tool":"list_news","args":{},"bind":"a"},
                {"kind":"replan","context":["a"]},
            ]},
        ])))
        .is_err()
    );
}

#[test]
fn rejects_terminal_nested_inside_parallel() {
    // The runtime ignores a stop signal coming from a parallel branch, so the
    // schema must not let the compiler emit a silently-dead step.
    assert!(
        parse(plan(json!([
            {"kind":"parallel","steps":[
                {"kind":"tool","tool":"list_news","args":{},"bind":"a"},
                {"kind":"terminal"},
            ]},
        ])))
        .is_err()
    );
}

#[test]
fn rejects_unknown_step_kind() {
    assert!(parse(plan(json!([{"kind":"branch","if":"x"}, {"kind":"terminal"}]))).is_err());
}

#[test]
fn rejects_unknown_fields_on_a_step() {
    assert!(
        parse(plan(json!([
            {"kind":"tool","tool":"list_news","args":{},"bind":"x","extra_field":"oops"},
            {"kind":"terminal"},
        ])))
        .is_err()
    );
}

#[test]
fn rejects_version_other_than_1() {
    assert!(parse(json!({"version":2,"steps":[{"kind":"terminal"}]})).is_err());
}

#[test]
fn rejects_empty_plan() {
    assert!(parse(json!({"version":1,"steps":[]})).is_err());
}

#[test]
fn rejects_llm_agent_max_iterations_out_of_bounds() {
    assert!(parse(plan(json!([
        {"kind":"llm_agent","preset":"smart","skill":"news-query","prompt":"x","tools":["search_news"],"maxIterations":50,"bind":"y"},
        {"kind":"terminal"},
    ])))
    .is_err());
}

#[test]
fn rejects_llm_agent_with_empty_tools_whitelist() {
    assert!(parse(plan(json!([
        {"kind":"llm_agent","preset":"smart","skill":"news-query","prompt":"x","tools":[],"maxIterations":3,"bind":"y"},
        {"kind":"terminal"},
    ])))
    .is_err());
}

// ── parseWorkflow ───────────────────────────────────────────────────────────

#[test]
fn returns_the_parsed_plan_on_success() {
    let workflow = parse(plan(json!([{"kind":"terminal"}]))).unwrap();
    assert_eq!(workflow.steps[0].kind(), "terminal");
}

#[test]
fn returns_human_readable_errors_on_failure() {
    let errs = errors(plan(json!([{"kind":"tool","tool":"unknown_one","args":{},"bind":"a"}, {"kind":"terminal"}])));
    assert!(!errs.is_empty());
    // Path-formatted: "steps[0].tool: …"
    assert!(errs.iter().any(|e| e.contains("steps[0].tool")), "{errs:?}");
}

// ── formatWorkflowErrors ────────────────────────────────────────────────────

#[test]
fn renders_path_with_brackets_for_array_indices_and_dots_for_keys() {
    let errs = errors(plan(json!([
        {"kind":"llm_agent","preset":"smart","skill":"news-query","prompt":"x","tools":["ghost_tool"],"maxIterations":3,"bind":"y"},
        {"kind":"terminal"},
    ])));
    assert!(errs.iter().any(|m| m.contains("steps[0].tools[0]")), "{errs:?}");
}

#[test]
fn uses_at_workflow_root_when_path_is_empty() {
    let errs = errors(json!("not an object"));
    assert!(errs.iter().any(|m| m.starts_with("at workflow root")), "{errs:?}");
}

#[test]
fn formats_an_invalid_enum_value_with_the_offending_value() {
    let errs = errors(plan(json!([{"kind":"tool","tool":"list_things","args":{},"bind":"a"}, {"kind":"terminal"}])));
    let line = errs.iter().find(|m| m.contains("steps[0].tool")).expect("a steps[0].tool line");
    assert!(line.contains("list_things"), "{line}");
}

#[test]
fn formats_unrecognized_keys_with_the_offending_key_name() {
    let msgs = errors(plan(json!([
        {"kind":"tool","tool":"list_news","args":{},"bind":"a","extra_field":"oops"},
        {"kind":"terminal"},
    ])))
    .join(" | ");
    assert!(msgs.contains("extra_field"), "{msgs}");
}

#[test]
fn formats_invalid_type_number_where_string() {
    let errs = errors(plan(json!([{"kind":"tool","tool":"list_news","args":{},"bind":42}, {"kind":"terminal"}])));
    let line = errs.iter().find(|m| m.contains("steps[0].bind")).expect("a steps[0].bind line");
    assert!(line.to_lowercase().contains("string"), "{line}");
}

#[test]
fn formats_invalid_literal_wrong_version() {
    let errs = errors(json!({"version":7,"steps":[{"kind":"terminal"}]}));
    assert!(errs.iter().any(|m| m.contains("version")), "{errs:?}");
}

#[test]
fn formats_too_small_max_iterations_below_1() {
    let errs = errors(plan(json!([
        {"kind":"llm_agent","preset":"smart","skill":"news-query","prompt":"x","tools":["search_news"],"maxIterations":0,"bind":"y"},
        {"kind":"terminal"},
    ])));
    assert!(errs.iter().any(|m| m.contains("steps[0].maxIterations")), "{errs:?}");
}

#[test]
fn emits_one_line_per_issue_when_a_parse_produces_multiple_errors() {
    let errs = errors(plan(json!([
        {"kind":"tool","tool":"bogus_tool_one","args":{},"bind":"a"},
        {"kind":"tool","tool":"bogus_tool_two","args":{},"bind":"b"},
    ])));
    assert!(errs.iter().any(|m| m.contains("steps[0].tool")), "{errs:?}");
    assert!(errs.iter().any(|m| m.contains("steps[1].tool")), "{errs:?}");
    assert!(errs.len() >= 2);
}

#[test]
fn formats_discriminator_mismatch_unknown_step_kind() {
    let errs = errors(plan(json!([{"kind":"loop"}, {"kind":"terminal"}])));
    assert!(errs.iter().any(|m| m.contains("steps[0]")), "{errs:?}");
}

// ── example fixture matches schema ──────────────────────────────────────────

#[test]
fn loads_and_validates_news_digest_example_json() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/news-digest.example.json");
    let parsed: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let result = parse(parsed);
    assert!(result.is_ok(), "{:?}", result.err());
}

// ── Rust-only: cases captured from the TS implementation ────────────────────

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

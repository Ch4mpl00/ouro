// The specification's own examples (tests/spec-examples, see schema/README.md):
// each one is valid by the bundled schema, and our parser and validation
// either accept it or reject it only for a construct outside the core: a
// protocol call, a process, authentication or a catalog, all of which are
// host functions or packs. Any other rejection means we disagree with the
// specification.
use owf::model::{Invalid, conforms, parse};
use std::path::PathBuf;

/// What the parser rejects outside the core (see model.rs, section 2).
const OUT_OF_CORE: &[&str] = &["authentication is not supported", "external catalogs are not supported"];

/// Protocol calls: host functions or packs, not core.
const PROTOCOLS: &[&str] = &["http", "openapi", "grpc", "asyncapi", "a2a", "mcp"];

fn examples() -> Vec<(String, serde_json::Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/spec-examples");
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                serde_yaml_ng::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap(),
            )
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn every_example_conforms_to_the_bundled_schema() {
    for (name, doc) in examples() {
        conforms(&doc).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn the_parser_accepts_examples_or_names_the_unsupported_construct() {
    let mut accepted = Vec::new();
    for (name, doc) in examples() {
        match parse(doc) {
            Ok(_) => accepted.push(name),
            Err(Invalid(problems)) => {
                for p in &problems {
                    let m = p.to_string();
                    assert!(OUT_OF_CORE.iter().any(|u| m.contains(u)), "{name}: unexpected rejection {m}");
                }
            }
        }
    }
    // The supported core of the DSL is exercised by real examples.
    for expected in [
        "do-multiple.yaml",
        "for-inline-array.yaml",
        "fork.yaml",
        "conditional-task.yaml",
        "raise-inline.yaml",
        "try-catch.yaml",
        "switch-then-string.yaml",
        "wait-duration-inline.yaml",
    ] {
        assert!(accepted.iter().any(|a| a == expected), "{expected} should parse; accepted: {accepted:?}");
    }
}

/// A host with every capability the core defines and no protocol function,
/// process runner or catalog: what remains unknown is out of the core.
struct Core;
impl owf::model::Catalog for Core {
    fn has(&self, _: &str) -> bool {
        false
    }
    fn events(&self) -> bool {
        true
    }
    fn emits(&self) -> bool {
        true
    }
    fn children(&self) -> bool {
        true
    }
    fn secrets(&self) -> bool {
        true
    }
    fn resources(&self) -> bool {
        true
    }
}

/// Why validation on a core-only host rejects an example, or `None` when
/// a problem is not explained by a protocol, a process or a catalog. A
/// protocol call's own arguments belong to the protocol (an AsyncAPI
/// subscription binds `$message` for its `foreach`).
fn out_of_core(problems: &[owf::model::Problem]) -> Option<&'static str> {
    let mut reason = None;
    let mut protocol_tasks = Vec::new();
    for p in problems {
        if let Some(function) = p.message.strip_prefix("unknown function `").and_then(|m| m.strip_suffix('`')) {
            reason = Some(if PROTOCOLS.contains(&function) {
                protocol_tasks.push(p.at.trim_end_matches("/call").to_string());
                "protocol"
            } else {
                "catalog"
            });
        } else if p.message.contains("needs a host process runner") {
            reason = Some("process");
        }
    }
    for p in problems {
        let explained = p.message.starts_with("unknown function `")
            || p.message.contains("needs a host process runner")
            || protocol_tasks.iter().any(|t| p.at.starts_with(&format!("{t}/")));
        if !explained {
            return None;
        }
    }
    reason
}

#[test]
fn every_example_is_accepted_by_the_core_or_needs_a_protocol_process_auth_or_catalog() {
    let mut verdicts = std::collections::BTreeMap::new();
    for (name, doc) in examples() {
        let verdict = match parse(doc) {
            Err(Invalid(problems)) => {
                assert!(
                    problems.iter().all(|p| OUT_OF_CORE.iter().any(|u| p.message.contains(u))),
                    "{name}: {problems:?}"
                );
                "auth/catalog"
            }
            Ok(wf) => match owf::model::validate(&wf, &Core) {
                Ok(()) => "accepted",
                Err(Invalid(problems)) => {
                    out_of_core(&problems).unwrap_or_else(|| panic!("{name}: rejected inside the core: {problems:?}"))
                }
            },
        };
        verdicts.insert(name, verdict);
    }
    for name in [
        "emit.yaml",
        "listen-to-all.yaml",
        "listen-to-all read-envelope.yaml",
        "listen-to-any.yaml",
        "listen-to-any-filter.yaml",
        "listen-to-any-until-condition.yaml",
        "listen-to-any-until-consumed.yaml",
        "run-subflow.yaml",
        "set.yaml",
        "switch-then-string.yaml",
        "wait-duration-iso8601.yaml",
    ] {
        assert_eq!(verdicts.get(name).copied(), Some("accepted"), "{name}");
    }
    eprintln!("{verdicts:#?}");
}

#[test]
fn the_schema_rejects_what_the_parser_would_otherwise_miss() {
    let doc: serde_json::Value = serde_yaml_ng::from_str(
        "document: { dsl: '1.0.4', namespace: t, name: w, version: 'not-semver' }\ndo: [ { a: { set: { x: 1 } } } ]",
    )
    .unwrap();
    assert!(conforms(&doc).is_err(), "the schema constrains document.version to semver");
}

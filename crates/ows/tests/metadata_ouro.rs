//! The engine's extensions live under `metadata.ouro`; everything else in
//! `metadata` belongs to the host and is never read (design 01).

use ows::{
    durable::Delivery,
    model::{self, Breaker, Catchup, Kind, Overlap, Workflow},
};

fn document(metadata: &str, schedule: &str) -> Result<Workflow, String> {
    model::parse_yaml(&format!(
        "document: {{ dsl: '1.0.4', namespace: t, name: w, version: '1.0.0', metadata: {metadata} }}\n{schedule}\ndo: [ {{ a: {{ set: {{ ok: true }} }} }} ]"
    ))
    .map_err(|e| e.to_string())
}

fn task(body: &str) -> Result<Workflow, String> {
    model::parse_yaml(&format!(
        "document: {{ dsl: '1.0.4', namespace: t, name: w, version: '1.0.0' }}\ndo: [ {{ x: {body} }} ]"
    ))
    .map_err(|e| e.to_string())
}

const CRON: &str = "schedule: { cron: '0 * * * *' }";
const EVERY: &str = "schedule: { every: PT1M }";

#[test]
fn document_extensions_are_honoured_under_ouro() {
    let wf = document("{ ouro: { limits: { max_jumps: 7, on_exceeded: end } } }", "").unwrap();
    assert_eq!(wf.budget.max_jumps, Some(7));
    assert!(matches!(wf.budget.on_exceeded, model::Exceeded::End));

    let at = document("{ ouro: { at: '1970-01-01T00:00:05Z' } }", "").unwrap();
    assert_eq!(at.schedule.unwrap().at, Some(5000));

    let zone = document("{ ouro: { timezone: Europe/Berlin } }", CRON).unwrap();
    assert_eq!(zone.schedule.unwrap().timezone, Some(chrono_tz::Europe::Berlin));

    let catchup = document("{ ouro: { catchup: none } }", EVERY).unwrap();
    assert_eq!(catchup.schedule.unwrap().catchup, Catchup::None);

    let overlap = document("{ ouro: { overlap: queue } }", EVERY).unwrap();
    assert_eq!(overlap.schedule.unwrap().overlap, Overlap::Queue);
}

#[test]
fn task_extensions_are_honoured_under_ouro() {
    let wf = task("{ call: echo, metadata: { ouro: { delivery: at_most_once } } }").unwrap();
    assert_eq!(wf.tasks[0].1.delivery, Delivery::AtMostOnce);

    let wf =
        task("{ for: { in: [1] }, do: [ { e: { call: echo } } ], metadata: { ouro: { concurrency: 3 } } }").unwrap();
    assert!(matches!(wf.tasks[0].1.kind, Kind::For { concurrency: Some(3), .. }));

    let wf = task("{ listen: { to: { one: { with: { type: x } } } }, metadata: { ouro: { hold: PT5S } } }").unwrap();
    assert_eq!(wf.tasks[0].1.hold, std::time::Duration::from_secs(5));
}

#[test]
fn host_keys_with_the_old_names_are_ignored() {
    // Each would be rejected (or honoured) as an engine extension today.
    let wf = document("{ at: anything, timezone: 42, catchup: x, overlap: y, limits: 3 }", EVERY).unwrap();
    let s = wf.schedule.unwrap();
    assert_eq!((s.at, s.timezone, s.catchup, s.overlap), (None, None, Catchup::Latest, Overlap::Skip));
    assert_eq!(wf.budget.max_jumps, None);
    assert!(document("{ at: anything }", "").unwrap().schedule.is_none());

    let wf = task("{ call: echo, metadata: { delivery: x, hold: y, concurrency: z } }").unwrap();
    assert_eq!(wf.tasks[0].1.delivery, Delivery::AtLeastOnce);
    let wf = task("{ for: { in: [1] }, do: [ { e: { call: echo } } ], metadata: { concurrency: 0 } }").unwrap();
    assert!(matches!(wf.tasks[0].1.kind, Kind::For { concurrency: None, .. }));
    // Host data of any shape, alongside the engine's namespace.
    assert!(document("{ loop: { a: 1 }, route: [ 1 ], Ouro: 1, OURO: 2, ouro: {} }", "").is_ok());
}

#[test]
fn unknown_keys_inside_ouro_list_the_known_ones() {
    for (metadata, schedule, at, known) in [
        (
            "{ ouro: { overlpa: queue } }",
            EVERY,
            "/document/metadata/ouro/overlpa",
            "limits, at, timezone, catchup, overlap",
        ),
        (
            "{ ouro: { limits: { max_jumps: 1 }, delivery: at_most_once } }",
            "",
            "/document/metadata/ouro/delivery",
            "limits",
        ),
    ] {
        let e = document(metadata, schedule).unwrap_err();
        assert!(e.contains(at) && e.contains("unknown property") && e.contains(known), "{e}");
    }
    let e = task("{ call: echo, metadata: { ouro: { retries: 3 } } }").unwrap_err();
    assert!(e.contains("/do/0/x/metadata/ouro/retries") && e.contains("delivery, concurrency, hold"), "{e}");
}

#[test]
fn ouro_must_be_an_object() {
    for value in ["null", "3", "[ 1 ]", "x"] {
        let e = document(&format!("{{ ouro: {value} }}"), "").unwrap_err();
        assert!(e.contains("/document/metadata/ouro") && e.contains("expected an object"), "{value}: {e}");
        let e = task(&format!("{{ call: echo, metadata: {{ ouro: {value} }} }}")).unwrap_err();
        assert!(e.contains("/do/0/x/metadata/ouro") && e.contains("expected an object"), "{value}: {e}");
    }
    assert!(document("{ ouro: {} }", "").is_ok());
    assert!(task("{ call: echo, metadata: { ouro: {} } }").is_ok());
}

#[test]
fn errors_point_under_ouro() {
    let at = |metadata: &str, schedule: &str| document(metadata, schedule).unwrap_err();
    assert!(at("{ ouro: { overlap: sometimes } }", EVERY).contains("/document/metadata/ouro/overlap"));
    assert!(at("{ ouro: { overlap: queue } }", "").contains("/document/metadata/ouro/overlap"));
    assert!(at("{ ouro: { limits: { max_jumps: -1 } } }", "").contains("/document/metadata/ouro/limits/max_jumps"));
    assert!(at("{ ouro: { limits: { on_exceeded: 3 } } }", "").contains("/document/metadata/ouro/limits/on_exceeded"));
    assert!(at("{ ouro: { timezone: Europe/Berlin } }", EVERY).contains("/document/metadata/ouro/timezone"));
    assert!(at("{ ouro: { catchup: x } }", EVERY).contains("/document/metadata/ouro/catchup"));
    assert!(at("{ ouro: { at: nope } }", "").contains("/document/metadata/ouro/at"));
}

#[test]
fn a_known_key_on_the_wrong_task_kind_says_where_it_belongs() {
    let e = task("{ set: { a: 1 }, metadata: { ouro: { delivery: at_most_once } } }").unwrap_err();
    assert!(e.contains("/do/0/x/metadata/ouro/delivery") && e.contains("call task"), "{e}");
    let e = task("{ call: echo, metadata: { ouro: { hold: PT1S } } }").unwrap_err();
    assert!(e.contains("/do/0/x/metadata/ouro/hold") && e.contains("hold belongs on a listen task"), "{e}");
    let e = task("{ call: echo, metadata: { ouro: { concurrency: 2 } } }").unwrap_err();
    assert!(e.contains("/do/0/x/metadata/ouro/concurrency") && e.contains("concurrency belongs on a for task"), "{e}");
}

#[test]
fn the_breaker_policy_is_off_or_a_threshold() {
    let parse = |value: &str| document(&format!("{{ ouro: {{ breaker: {value} }} }}"), "").map(|w| w.breaker);
    assert_eq!(document("{}", "").unwrap().breaker, Breaker::Inherit, "absent: the host's default");
    assert_eq!(document("{ ouro: {} }", "").unwrap().breaker, Breaker::Inherit);
    assert_eq!(parse("off"), Ok(Breaker::Off));
    assert_eq!(parse("{ threshold: 1 }"), Ok(Breaker::Threshold(1)));
    assert_eq!(parse("{ threshold: 7 }"), Ok(Breaker::Threshold(7)));
    assert_eq!(parse("{ threshold: 1000 }"), Ok(Breaker::Threshold(1000)));
    // Any other value is a validation error at the policy's pointer.
    for bad in [
        "0",
        "3",
        "true",
        "null",
        "on",
        "OFF",
        "[ 1 ]",
        "{}",
        "{ threshold: 0 }",
        "{ threshold: 1001 }",
        "{ threshold: -1 }",
        "{ threshold: 1.5 }",
        "{ threshold: '3' }",
        "{ threshold: off }",
        "{ threshold: 2, window: 5 }",
        "{ limit: 2 }",
    ] {
        let e = parse(bad).unwrap_err();
        assert!(e.contains("/document/metadata/ouro/breaker"), "{bad}: {e}");
    }
    let e = parse("{ threshold: 0 }").unwrap_err();
    assert!(e.contains("/document/metadata/ouro/breaker/threshold"), "{e}");
    // Host keys of the same name are not read.
    assert_eq!(document("{ breaker: off }", "").unwrap().breaker, Breaker::Inherit);
}

#[test]
fn the_breaker_policy_belongs_to_the_document() {
    let e = task("{ call: echo, metadata: { ouro: { breaker: off } } }").unwrap_err();
    assert!(e.contains("/do/0/x/metadata/ouro/breaker") && e.contains("unknown property"), "{e}");
}

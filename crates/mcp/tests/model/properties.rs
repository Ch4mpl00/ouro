// What must hold of the model, for every world the calls can reach and every
// call — the six theorems the Lean model proved, here checked by proptest on
// random worlds. A port of packages/mcp/model/properties.test.ts. A failure
// prints the smallest call sequence that breaks one.
//
//   1. a failed call changes nothing
//   2. version n means exactly n recorded patches; patch i moved the
//      document from version i to i+1
//   3. reverting the newest patch restores the body it replaced
//   4. a patch followed by its revert is a no-op on the text
//   5. append keeps the old text and contains the new
//   6. "the newest patch" and "the patch that produced the current version"
//      are the same patch, so an implementation may decide either way

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use crate::calls::{DOCS, Step, concretise, step as step_strategy, text};
use crate::model::{Cmd, Reply, World, chars, is_error, step, str_of, trim, trim_end};

fn runs() -> u32 {
    std::env::var("MODEL_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(1000)
}

// Every world reachable from no documents: replay a random call sequence.
fn reachable() -> impl Strategy<Value = Vec<Step>> {
    proptest::collection::vec(step_strategy(DOCS.len()), 0..=25)
}

fn reach(steps: &[Step]) -> World {
    let mut w = World::new();
    for s in steps {
        w = step(&w, &concretise(s, &w, None).cmd).0;
    }
    w
}

// fast-check allows 100 skipped cases (fc.pre) per run; so does this.
fn check<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>)
where
    S::Value: std::fmt::Debug,
{
    let config = Config { max_global_rejects: runs() * 100, ..Config::with_cases(runs()) };
    TestRunner::new(config).run(&strategy, test).unwrap();
}

#[test]
fn p1_a_failed_call_changes_nothing() {
    check((reachable(), step_strategy(DOCS.len())), |(steps, s)| {
        let w = reach(&steps);
        let (after, reply) = step(&w, &concretise(&s, &w, None).cmd);
        if is_error(&reply) {
            prop_assert_eq!(after, w);
        }
        Ok(())
    });
}

#[test]
fn p2_versions_match_history() {
    check(reachable(), |steps| {
        for d in reach(&steps).values() {
            prop_assert_eq!(d.version, d.history.len() as i64);
            for (i, p) in d.history.iter().enumerate() {
                prop_assert_eq!((p.version_before, p.version_after), (i as i64, i as i64 + 1));
            }
        }
        Ok(())
    });
}

#[test]
fn p3_reverting_the_newest_patch_restores_the_body_it_replaced() {
    check((reachable(), any::<bool>()), |(steps, rollback)| {
        let w = reach(&steps);
        for (name, d) in &w {
            let newest = d.history.last().expect("a document has a patch");
            let (after, _) = step(&w, &Cmd::Revert { doc: name.clone(), index: d.history.len() - 1, rollback });
            prop_assert_eq!(&after[name].body, &newest.body_before);
        }
        Ok(())
    });
}

#[test]
fn p4_a_patch_followed_by_its_revert_is_a_no_op_on_the_text() {
    let patch_step = step_strategy(DOCS.len()).prop_filter("a patch", |s| matches!(s, Step::Patch { .. }));
    check((reachable(), patch_step), |(steps, s)| {
        let w = reach(&steps);
        let call = concretise(&s, &w, None);
        let doc = call.cmd.doc().to_owned();
        let before = w.get(&doc).cloned();
        let (patched, reply) = step(&w, &call.cmd);
        let Some(before) = before else { return Err(TestCaseError::reject("no document yet")) };
        if !matches!(reply, Reply::Written { .. }) {
            return Err(TestCaseError::reject("the patch did not apply"));
        }
        let (reverted, _) =
            step(&patched, &Cmd::Revert { doc: doc.clone(), index: before.history.len(), rollback: false });
        prop_assert_eq!(&reverted[&doc].body, &before.body);
        Ok(())
    });
}

#[test]
fn p5_append_keeps_the_old_text_and_contains_the_new() {
    check((reachable(), text(6).prop_filter("non-empty", |t| !t.is_empty())), |(steps, t)| {
        let w = reach(&steps);
        for (name, d) in &w {
            let (after, reply) = step(&w, &Cmd::Append { doc: name.clone(), text: t.clone() });
            let written = matches!(reply, Reply::Written { .. });
            prop_assert!(written, "append answered {:?}", reply);
            let body = &after[name].body;
            prop_assert!(body.starts_with(&str_of(&trim_end(&chars(&d.body)))));
            prop_assert!(body.contains(&str_of(&trim(&chars(&t)))));
        }
        Ok(())
    });
}

#[test]
fn p6_the_newest_patch_is_the_one_that_produced_the_current_version() {
    check(reachable(), |steps| {
        for d in reach(&steps).values() {
            for (i, p) in d.history.iter().enumerate() {
                prop_assert_eq!(i + 1 == d.history.len(), p.version_after == d.version);
            }
        }
        Ok(())
    });
}

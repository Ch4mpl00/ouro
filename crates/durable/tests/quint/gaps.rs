//! Regressions for the contract gaps #3 and #4 closed: each failed on the API
//! before them.
use super::{Adapter, StoreDriver};
use ouro_durable::{Rearm, Trigger, conformance::admission};
use ows::run::{Status, WorkflowError};
use serde_json::json;

fn adapters(test: impl Fn(StoreDriver)) {
    for adapter in [Adapter::Memory, Adapter::Sqlite] {
        test(StoreDriver::new(adapter).unwrap());
    }
}
#[test]
fn w2_conflicting_event_identity_is_an_error() {
    adapters(|d| {
        d.block(d.store.admit(&admission("r"), 0)).unwrap();
        let first = json!({"source":"test","id":"1","type":"event","data":1});
        assert_eq!(d.block(d.store.publish(&first, Some("r"), 0)).unwrap(), vec!["r"]);
        let mut changed = first.clone();
        changed["data"] = json!(2);
        assert!(d.block(d.store.publish(&changed, Some("r"), 1)).is_err(), "conflict was silently accepted");
    });
}
#[test]
fn w3_mailbox_expiry_is_observable() {
    adapters(|d| {
        d.block(d.store.admit(&admission("r"), 0)).unwrap();
        d.block(d.store.publish(&json!({"source":"test","id":"1","type":"event","data":1}), Some("r"), 0)).unwrap();
        d.block(d.store.sweep(86_400_001, 0)).unwrap();
        let snapshot = d.block(d.store.inspect("r")).unwrap().unwrap();
        assert!(!snapshot.events.iter().any(|e| e.consumed_key.is_none()), "expired mailbox copy still deliverable");
        assert!(!snapshot.records.is_empty(), "expiry must leave observable evidence");
    });
}
#[test]
fn t2_old_completion_cannot_rearm_new_epoch() {
    adapters(|d| {
        let old = Trigger { at_ms: None, types: vec![], state: json!({"epoch":1}) };
        let new = Trigger { at_ms: None, types: vec![], state: json!({"epoch":2}) };
        let rearm = Rearm { trigger: "schedule".into(), delay_ms: 10 };
        assert!(
            d.block(d.store.swap_trigger("schedule", None, Some(&old), &[admission("old")], Some(&rearm), 0)).unwrap()
        );
        assert!(d.block(d.store.swap_trigger("schedule", Some(&old), Some(&new), &[], None, 0)).unwrap());
        let fence = d.block(d.store.claim("old", 0)).unwrap().unwrap();
        d.block(d.store.finish(&fence, &Status::Completed(json!(0)), 1, 3)).unwrap();
        assert_eq!(d.block(d.store.trigger("schedule")).unwrap(), Some(new));
    });
}
#[test]
fn v2_old_epoch_fault_does_not_count_after_reactivation() {
    adapters(|d| {
        let version = d.block(d.store.save_definition("model", "definition")).unwrap();
        d.block(d.store.activate("model", version.version, 0)).unwrap();
        let mut a = admission("old");
        a.name = "model".into();
        a.version = version.version;
        let fence = d.block(d.store.start(&a, 0)).unwrap().unwrap();
        d.block(d.store.activate("model", version.version, 0)).unwrap();
        d.block(d.store.finish(&fence, &Status::Faulted(WorkflowError::runtime("fault")), 1, 3)).unwrap();
        assert_eq!(d.block(d.store.definitions("model")).unwrap().consecutive_faults, 0);
    });
}

// The bridge command carries a declared worker version, passed to the #3
// public operation; no test-side policy guard.
async fn claim_as(
    store: &dyn ouro_durable::RunStore,
    id: &str,
    worker_version: &str,
) -> anyhow::Result<Option<ouro_durable::Fence>> {
    let worker = ouro_durable::Compatibility { version: Some(worker_version.into()), accepts: vec![] };
    store.claim_as(id, 0, &worker).await
}
#[test]
fn v3_incompatible_worker_cannot_claim() {
    adapters(|d| {
        let mut a = admission("versioned");
        a.runtime = json!({"application_version":"one"});
        d.block(d.store.admit(&a, 0)).unwrap();
        assert!(d.block(claim_as(d.store.as_ref(), "versioned", "two")).unwrap().is_none());
        assert!(d.block(d.store.get("versioned")).unwrap().is_some());
    });
}

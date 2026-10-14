// Port of test/agent-db.test.ts: generated operations on a real sqlite file,
// reopen, immutable migration bytes, judge filters and simultaneous boot.
use agent::db::*;
use proptest::prelude::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
#[test]
fn migration_files_and_journal_match_typescript_byte_for_byte() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rust = root.join("crates/agent/migrations");
    let ts = root.join("packages/agent/src/db/migrations");
    assert_eq!(
        std::fs::read(rust.join("meta/_journal.json")).unwrap(),
        std::fs::read(ts.join("meta/_journal.json")).unwrap()
    );
    let migrations = migrations().unwrap();
    for migration in &migrations {
        let sql = std::fs::read(ts.join(format!("{}.sql", migration.tag))).unwrap();
        assert_eq!(std::fs::read(rust.join(format!("{}.sql", migration.tag))).unwrap(), sql);
        assert_eq!(migration.hash, hex::encode(Sha256::digest(sql)));
    }
    let dir = tempfile::tempdir().unwrap();
    let db = AgentDb::open(dir.path().join("agent.db")).unwrap();
    let rows: Vec<(String, i64)> = db
        .conn()
        .prepare("SELECT hash,created_at FROM __drizzle_migrations ORDER BY created_at")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, migrations.into_iter().map(|m| (m.hash, m.when)).collect::<Vec<_>>());
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]
    #[test]
    fn kv_last_write_wins_across_reopen(ops in prop::collection::vec((0u8..3,0usize..5,".{0,80}"),1..31)){
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("agent.db");let mut db=AgentDb::open(&path).unwrap();
        let keys=["news_digest.last_read_at","tech_digest.last_read_at","ключ","__proto__","a"];
        let mut model=HashMap::new();
        for (kind,index,value) in ops{
            let key=keys[index];match kind{
                0=>{db.set(key,&value).unwrap();model.insert(key.to_owned(),value);},
                1=>prop_assert_eq!(db.get(key).unwrap(),model.get(key).cloned()),
                _=>{drop(db);db=AgentDb::open(&path).unwrap();},
            }
        }
        for (key,value) in model{prop_assert_eq!(db.get(&key).unwrap(),Some(value));}
    }
}
fn trace(id: &str, at: &str) -> StoredTrace {
    StoredTrace {
        id: id.into(),
        name: format!("run:{id}"),
        source: Some("telegram".into()),
        skill: None,
        session_id: Some(id.into()),
        tags: vec!["telegram".into()],
        input: Some(json!({"user":"Привет 🦊"})),
        output: None,
        metadata: Some(json!({"signal_source":"telegram"}).as_object().unwrap().clone()),
        observations: Vec::new(),
        started_at: at.into(),
    }
}
#[test]
fn trace_upserts_and_judgements_filter_by_provider_version_and_skill() {
    let dir = tempfile::tempdir().unwrap();
    let db = AgentDb::open(dir.path().join("agent.db")).unwrap();
    db.write_trace(&trace("old", "2026-10-01T00:00:00Z")).unwrap();
    db.write_trace(&trace("new", "2026-10-02T00:00:00Z")).unwrap();
    let mut replacement = trace("old", "2026-10-03T00:00:00Z");
    replacement.output = Some(json!([false, 42, null]));
    db.write_trace(&replacement).unwrap();
    let (back, _) = db.get_trace("old").unwrap().unwrap();
    assert_eq!(back.input, json!({"user":"Привет 🦊"}));
    assert_eq!(back.output, json!([false, 42, null]));
    assert_eq!(db.list_recent(1, None).unwrap()[0].id, "old");
    assert!(db.get_trace("missing").unwrap().is_none());
    let mut judgement = JudgementInput {
        trace_id: "old".into(),
        observation_id: "g1".into(),
        provider: "codex".into(),
        prompt_version: "v1".into(),
        node_kind: "compose".into(),
        skill: "digest".into(),
        scores: Scores { faithfulness: Some(1.0), ..Default::default() },
        detail: json!({"note":"bad"}),
    };
    db.write_judgement(&judgement).unwrap();
    judgement.scores.faithfulness = Some(5.0);
    judgement.detail = json!({"note":"fixed"});
    db.write_judgement(&judgement).unwrap();
    let rows =
        db.list_judgements(&JudgementFilter { skill: "digest", provider: "codex", prompt_version: "v1" }).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].scores.faithfulness, Some(5.0));
    assert_eq!(rows[0].detail, json!({"note":"fixed"}));
    assert_eq!(
        db.list_recent(10, Some(("codex", "v1"))).unwrap().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        ["new"]
    );
    assert_eq!(db.list_recent(10, Some(("codex", "v2"))).unwrap().len(), 2);
    assert!(
        db.list_judgements(&JudgementFilter { skill: "other", provider: "codex", prompt_version: "v1" })
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.list_judged_skills("codex", "v1").unwrap(), ["digest"]);
    judgement.trace_id = "no-such-trace".into();
    assert!(db.write_judgement(&judgement).is_err());
}
#[test]
fn concurrent_boots_migrate_once_and_preserve_all_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    let start = std::sync::Arc::new(std::sync::Barrier::new(8));
    let tasks: Vec<_> = (0..8)
        .map(|n| {
            let path = path.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                let db = AgentDb::open(path).unwrap();
                db.set(&format!("key{n}"), "value").unwrap();
            })
        })
        .collect();
    for task in tasks {
        task.join().unwrap();
    }
    let db = AgentDb::open(path).unwrap();
    let count: i64 = db.conn().query_row("SELECT count(*) FROM __drizzle_migrations", [], |r| r.get(0)).unwrap();
    assert_eq!(count, migrations().unwrap().len() as i64);
    for n in 0..8 {
        assert_eq!(db.get(&format!("key{n}")).unwrap().as_deref(), Some("value"));
    }
}

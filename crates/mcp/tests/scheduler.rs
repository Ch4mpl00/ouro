// The scheduler, judged from the outside: tasks are created, listed and
// cancelled over MCP, the time zone is set over MCP, and the poller's tick
// runs on a controlled clock. Every step is checked against the rows Postgres
// holds before and after, and every fire against what the cron means on the
// user's wall clock (support/wall_cron.rs — not croner). A port of
// packages/mcp/test/scheduler.test.ts.
//
// Rules, as in signal_queue.rs: outcome (from request + pre-state), effect
// (the reply and the changed rows), frame (what must not change).
//
// PROPERTY_RUNS raises the number of generated sequences.

mod support;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use mcp_tools::scheduler::{self, SignalSink, TaskRow, TaskStore};
use mcp_tools::settings::Settings;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{Value, json};
use support::wall_cron::{days_exist, first_owed, legitimate_slot, parse_wall_cron};
use support::{Client, Outcome, Server, TestDatabase, call, connect, property_runs, runtime, start_test_server};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;

// ── observation ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct Task {
    id: i64,
    cron: String,
    recurring: bool,
    prompt: String,
    source: Option<String>,
    last_run_at: Option<i64>, // unix seconds of the slot last fired for
    created_ms: i64,
}

#[derive(Debug, Clone)]
struct World {
    tz: String,
    tasks: BTreeMap<i64, Task>,
    signals: BTreeMap<i64, (String, String)>,
}

async fn observe(server: &Server) -> World {
    let client = server.db.client().await.unwrap();
    let tz = client
        .query_opt("SELECT value FROM settings WHERE key = 'timezone'", &[])
        .await
        .unwrap()
        .map_or("UTC".to_owned(), |r| r.get(0));
    let tasks = client
        .query(
            "SELECT id, cron_expr, recurring, prompt, source, last_run_at,
                    (extract(epoch FROM created_at) * 1000)::bigint AS created_ms
               FROM scheduled_tasks",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let t = Task {
                id: r.get(0),
                cron: r.get(1),
                recurring: r.get(2),
                prompt: r.get(3),
                source: r.get(4),
                last_run_at: r.get(5),
                created_ms: r.get(6),
            };
            (t.id, t)
        })
        .collect();
    let signals = client
        .query("SELECT id, source, content FROM signals", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), (r.get(1), r.get(2))))
        .collect();
    World { tz, tasks, signals }
}

fn active(t: &Task) -> bool {
    t.recurring || t.last_run_at.is_none()
}

// What the next fire is counted from: the slot last fired for, or creation.
fn anchor_ms(t: &Task) -> i64 {
    t.last_run_at.map_or(t.created_ms, |s| s * 1000)
}

fn iso(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn tz_of(name: &str) -> Tz {
    name.parse().expect("a known zone")
}

// ── a fired signal, read back ────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Fire {
    source: String,
    task_id: i64,
    cron: String,
    slot_ms: i64,
    now_iso: String,
    previous: String,
    recurring: String,
    body: String,
}

fn parse_fire(signal_id: i64, source: &str, content: &str) -> Fire {
    let lines: Vec<&str> = content.split('\n').collect();
    let header = |i: usize, label: &str| -> String {
        let line = lines.get(i).copied().unwrap_or_default();
        line.strip_prefix(label)
            .unwrap_or_else(|| panic!("signal {signal_id}: line {i} is not {label:?}…: {line:?}"))
            .to_owned()
    };
    let id = regex::Regex::new(r"^Scheduled task #(\d+) fired\.$").unwrap();
    let caps = id.captures(lines.first().copied().unwrap_or_default()).filter(|_| lines.get(6) == Some(&""));
    let caps = caps.unwrap_or_else(|| panic!("signal {signal_id}: not a scheduler signal: {:.200}", content));
    Fire {
        source: source.to_owned(),
        task_id: caps[1].parse().unwrap(),
        cron: header(1, "Cron: "),
        slot_ms: DateTime::parse_from_rfc3339(&header(2, "Slot: ")).expect("a slot").timestamp_millis(),
        now_iso: header(3, "Now: "),
        previous: header(4, "Previous fire: "),
        recurring: header(5, "Recurring: "),
        body: lines[7..].join("\n"),
    }
}

// ── operations ───────────────────────────────────────────────────────────────

// Not Australia/Lord_Howe: its 30-minute DST shifts were mishandled by the
// TS server's cron library (see F5 in the ledger).
const ZONES: [&str; 4] = ["UTC", "Europe/Kiev", "America/New_York", "Asia/Kolkata"];
const BAD_ZONES: [&str; 3] = ["Mars/Olympus_Mons", "Europe/Kyiv_", "UTC+3"];

// Realistic agent-written crons, plus a few the tool must refuse.
fn cron() -> impl Strategy<Value = String> {
    (
        proptest::sample::select(vec!["0", "15", "30", "59", "*/15", "*/5", "*", "0,30"]),
        proptest::sample::select(vec!["*", "0", "2", "3", "9", "23", "*/6", "9,18", "1-3"]),
        proptest::sample::select(vec!["*", "*", "*", "1", "15", "29", "31"]),
        proptest::sample::select(vec!["*", "*", "*", "3", "10", "11"]),
        proptest::sample::select(vec!["*", "*", "*", "1-5", "0", "6", "7"]),
    )
        .prop_map(|(m, h, dom, mon, dow)| format!("{m} {h} {dom} {mon} {dow}"))
}

fn bad_cron() -> impl Strategy<Value = String> {
    proptest::sample::select(vec![
        "every day at nine",
        "61 * * * *",
        "0 25 * * *",
        "* * * *",
        "30 9 * * * *",
        "0 9 * * * * * *",
        "@daily",
    ])
    .prop_map(str::to_owned)
}

fn ms(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
}

// Clock starts near the year's DST transitions (Kiev, New York) or anywhere.
fn start() -> impl Strategy<Value = i64> {
    prop_oneof![
        proptest::sample::select(vec![
            ms("2026-03-07T20:00:00Z"), // New York springs forward Mar 8
            ms("2026-03-28T20:00:00Z"), // Kiev springs forward Mar 29
            ms("2026-10-24T20:00:00Z"), // Kiev falls back Oct 25
            ms("2026-10-31T20:00:00Z"), // New York falls back Nov 1
        ]),
        ms("2026-01-01T00:00:00Z")..=ms("2026-12-31T00:00:00Z"),
    ]
}

#[derive(Debug, Clone)]
enum Op {
    Schedule { cron: String, recurring: bool, prompt: String },
    Seed { cron: String, source: &'static str, prompt: String },
    Cancel { pick: usize, exists: bool },
    Timezone { tz: &'static str },
    List,
    Advance { ms: i64 },
    AdvanceToFire { pick: usize, offset_ms: i64 },
    Tick,
}

fn prompt() -> impl Strategy<Value = String> {
    "\\PC{1,40}"
}

fn nat() -> impl Strategy<Value = usize> {
    0usize..(i32::MAX as usize)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (prop_oneof![9 => cron(), 1 => bad_cron()], any::<bool>(), prompt())
            .prop_map(|(cron, recurring, prompt)| Op::Schedule { cron, recurring, prompt }),
        3 => (cron(), proptest::sample::select(vec!["dreaming", "news-digest"]), prompt())
            .prop_map(|(cron, source, prompt)| Op::Seed { cron, source, prompt }),
        2 => (nat(), any::<bool>()).prop_map(|(pick, exists)| Op::Cancel { pick, exists }),
        1 => proptest::sample::select([&ZONES[..], &BAD_ZONES[..]].concat()).prop_map(|tz| Op::Timezone { tz }),
        1 => Just(Op::List),
        3 => prop_oneof![
            proptest::sample::select(vec![1000, 29_000, 30_000, MINUTE, 15 * MINUTE, HOUR, 6 * HOUR, 24 * HOUR]),
            1..=26 * HOUR,
        ].prop_map(|ms| Op::Advance { ms }),
        3 => (nat(), proptest::sample::select(vec![-1000i64, 0, 1000])).prop_map(|(pick, offset_ms)| Op::AdvanceToFire { pick, offset_ms }),
        6 => Just(Op::Tick),
    ]
}

// ── rules ────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Violations(Vec<String>);

impl Violations {
    fn check(&mut self, rule: &str, holds: bool, detail: impl FnOnce() -> String) {
        if !holds {
            self.0.push(format!("{rule}: {}", detail()));
        }
    }
}

fn tasks_frame(v: &mut Violations, rule: &str, before: &World, after: &World, except: &HashSet<i64>) {
    for (id, t) in &before.tasks {
        if except.contains(id) {
            continue;
        }
        v.check(rule, after.tasks.get(id) == Some(t), || format!("{id}: {t:?} → {:?}", after.tasks.get(id)));
    }
}

fn no_signals(v: &mut Violations, rule: &str, before: &World, after: &World) {
    let added: Vec<&i64> = after.signals.keys().filter(|id| !before.signals.contains_key(id)).collect();
    v.check(rule, added.is_empty(), || format!("{added:?}"));
}

const MAX_SCAN: i64 = 40 * 24 * HOUR;

// Upcoming fires a tool reports: legitimate, increasing, and nothing owed
// skipped between them (from `from_ms`, the real time of the call).
fn check_upcoming(v: &mut Violations, rule: &str, cron_text: &str, tz: Tz, from_ms: i64, upcoming: &[Value]) {
    let c = parse_wall_cron(cron_text).expect("a valid cron");
    let mut prev = from_ms;
    for u in upcoming {
        let text = u.as_str().expect("an ISO string");
        let at = DateTime::parse_from_rfc3339(text).expect("ISO").timestamp_millis();
        v.check(&format!("{rule}: a legitimate fire time"), legitimate_slot(&c, tz, at), || {
            format!("{cron_text} {tz} {text}")
        });
        v.check(&format!("{rule}: increasing and in the future"), at > prev - 2000, || {
            format!("{text} after {}", iso(prev))
        });
        if at - prev < MAX_SCAN {
            let owed = first_owed(&c, tz, prev, at - 1);
            v.check(&format!("{rule}: no owed fire skipped"), owed.is_none(), || {
                format!("{cron_text} {tz} {text} skipped {:?}", owed.map(iso))
            });
        }
        prev = at;
    }
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

const TASK_KEYS: [&str; 7] = ["id", "cron_expr", "recurring", "prompt", "source", "last_run_at", "created_at"];

fn ok_value(outcome: Outcome, op: &str) -> Value {
    match outcome {
        Outcome::Ok(v) => v,
        other => panic!("{op}: expected a result, got {other:?}"),
    }
}

struct Env {
    _db: TestDatabase,
    server: Server,
    client: Client,
    settings: Settings,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        runtime().block_on(async {
            let db = support::test_database().await;
            let server = start_test_server(&db, None).await.expect("server starts");
            let client = connect(&server.url).await;
            let settings = Settings::new(server.db.clone());
            Env { _db: db, server, client, settings }
        })
    })
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

static WITNESSED: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

fn witness(w: &'static str) {
    WITNESSED.lock().unwrap().insert(w);
}

async fn set_created(env: &Env, id: i64, created_ms: i64) {
    env.server
        .db
        .client()
        .await
        .unwrap()
        .execute(
            "UPDATE scheduled_tasks SET created_at = to_timestamp($1::bigint / 1000.0) WHERE id = $2",
            &[&created_ms, &id],
        )
        .await
        .unwrap();
}

async fn step(env: &Env, clock: &mut i64, o: &Op) -> Vec<String> {
    let mut v = Violations::default();
    let before = observe(&env.server).await;
    let tz = tz_of(&before.tz);
    let none = HashSet::new();

    match o {
        Op::Advance { ms } => {
            *clock += ms;
        }
        Op::AdvanceToFire { pick, offset_ms } => {
            let candidates: Vec<&Task> = before.tasks.values().filter(|t| active(t)).collect();
            if let Some(t) = candidates.get(pick % candidates.len().max(1)) {
                let c = parse_wall_cron(&t.cron).expect("stored crons are valid");
                if let Some(owed) = first_owed(&c, tz, anchor_ms(t).max(*clock - MINUTE), *clock + 3 * 24 * HOUR) {
                    *clock = owed + offset_ms;
                }
            }
        }
        Op::Schedule { cron, recurring, prompt } => {
            let real_now = Utc::now().timestamp_millis();
            let reply = ok_value(
                call(
                    &env.client,
                    "schedule_task",
                    json!({ "cron_expr": cron, "recurring": recurring, "prompt": prompt }),
                )
                .await,
                "schedule_task",
            );
            let after = observe(&env.server).await;
            tasks_frame(&mut v, "schedule frame: existing tasks untouched", &before, &after, &none);
            no_signals(&mut v, "schedule frame: no signal", &before, &after);
            if reply["ok"] == false {
                v.check("schedule: a refusal is {ok, error}", keys(&reply) == set(&["ok", "error"]), || {
                    reply.to_string()
                });
            } else {
                v.check(
                    "schedule: reply shape",
                    keys(&reply) == set(&["ok", "task", "timezone", "upcoming_fires"])
                        && keys(&reply["task"]) == set(&TASK_KEYS),
                    || reply.to_string(),
                );
            }
            // Valid: five fields whose days exist in some selected month. Days
            // that don't exist but a restricted weekday could still match —
            // either answer is accepted.
            let valid = match parse_wall_cron(cron) {
                Ok(c) if days_exist(&c) => Some(true),
                Ok(c) if c.dow_restricted => None,
                _ => Some(false),
            };
            let Some(valid) = valid else {
                if reply["ok"] == false {
                    v.check("schedule frame: a refusal adds no task", after.tasks.len() == before.tasks.len(), || {
                        cron.clone()
                    });
                } else {
                    set_created(env, reply["task"]["id"].as_i64().unwrap(), *clock).await;
                }
                return v.0;
            };
            if !valid {
                witness("an invalid cron is refused");
                v.check("schedule: an invalid cron is refused", reply["ok"] == false, || format!("{cron:?} {reply}"));
                v.check("schedule frame: a refusal adds no task", after.tasks.len() == before.tasks.len(), || {
                    cron.clone()
                });
                return v.0;
            }
            v.check("schedule: a valid cron is accepted", reply["ok"] == true, || format!("{cron:?} {reply}"));
            if reply["ok"] != true {
                return v.0;
            }
            let id = reply["task"]["id"].as_i64().unwrap();
            let row = after.tasks.get(&id);
            v.check(
                "schedule: exactly one task is added",
                after.tasks.len() == before.tasks.len() + 1 && !before.tasks.contains_key(&id),
                || id.to_string(),
            );
            v.check(
                "schedule: the task holds what was asked",
                row.is_some_and(|r| {
                    r.cron == *cron
                        && r.recurring == *recurring
                        && r.prompt == *prompt
                        && r.source.is_none()
                        && r.last_run_at.is_none()
                }),
                || format!("{row:?}"),
            );
            v.check(
                "schedule: the reply echoes the stored task",
                reply["task"]["cron_expr"] == json!(cron)
                    && reply["task"]["prompt"] == json!(prompt)
                    && reply["task"]["recurring"] == json!(i64::from(*recurring)),
                || reply["task"].to_string(),
            );
            v.check("schedule: timezone is the configured one", reply["timezone"] == json!(before.tz), || {
                format!("{} vs {}", reply["timezone"], before.tz)
            });
            let upcoming = reply["upcoming_fires"].as_array().cloned().unwrap_or_default();
            v.check(
                "schedule: 3 upcoming fires for recurring, 1 for one-shot",
                upcoming.len() == if *recurring { 3 } else { 1 },
                || format!("{upcoming:?}"),
            );
            check_upcoming(&mut v, "schedule upcoming", cron, tz, real_now, &upcoming);
            // Move the new task into the controlled clock's world.
            set_created(env, id, *clock).await;
        }
        Op::Seed { cron, source, prompt } => {
            let row = env.server.scheduler.insert(cron, true, prompt, Some(source)).await.unwrap();
            set_created(env, row.id, *clock).await;
        }
        Op::Cancel { pick, exists } => {
            let ids: Vec<i64> = before.tasks.keys().copied().collect();
            let id = if *exists && !ids.is_empty() {
                ids[pick % ids.len()]
            } else {
                ids.iter().copied().max().unwrap_or(0).max(0) + 1 + (pick % 5) as i64
            };
            let reply = ok_value(
                call(&env.client, "cancel_scheduled_task", json!({ "id": id })).await,
                "cancel_scheduled_task",
            );
            let after = observe(&env.server).await;
            let existed = before.tasks.contains_key(&id);
            v.check("cancel: reply shape", keys(&reply) == set(&["ok", "id"]), || reply.to_string());
            v.check("cancel: ok exactly when the task existed", reply["ok"] == existed && reply["id"] == id, || {
                format!("{id} {existed} {reply}")
            });
            v.check("cancel: the task is gone", !after.tasks.contains_key(&id), || id.to_string());
            v.check(
                "cancel: nothing else is removed",
                after.tasks.len() == before.tasks.len() - usize::from(existed),
                || id.to_string(),
            );
            tasks_frame(&mut v, "cancel frame: other tasks untouched", &before, &after, &HashSet::from([id]));
            no_signals(&mut v, "cancel frame: no signal", &before, &after);
            if existed {
                witness("a task is cancelled");
            }
        }
        Op::Timezone { tz: zone } => {
            let reply = ok_value(call(&env.client, "set_timezone", json!({ "tz": zone })).await, "set_timezone");
            let after = observe(&env.server).await;
            let known = ZONES.contains(zone);
            let shape = if reply["ok"] == true { set(&["ok", "timezone", "local_now"]) } else { set(&["ok", "error"]) };
            v.check("timezone: reply shape", keys(&reply) == shape, || reply.to_string());
            v.check("timezone: accepted exactly when IANA knows it", reply["ok"] == known, || {
                format!("{zone} {reply}")
            });
            v.check(
                "timezone: stored when accepted, unchanged when refused",
                after.tz == if known { (*zone).to_owned() } else { before.tz.clone() },
                || after.tz.clone(),
            );
            tasks_frame(&mut v, "timezone frame: tasks untouched", &before, &after, &none);
            no_signals(&mut v, "timezone frame: no signal", &before, &after);
        }
        Op::List => {
            let real_now = Utc::now().timestamp_millis();
            let reply = ok_value(call(&env.client, "list_scheduled_tasks", json!({})).await, "list_scheduled_tasks");
            let after = observe(&env.server).await;
            tasks_frame(&mut v, "list frame: listing changes nothing", &before, &after, &none);
            no_signals(&mut v, "list frame: no signal", &before, &after);
            v.check("list: reply shape", keys(&reply) == set(&["timezone", "count", "tasks"]), || reply.to_string());
            let mut expected: Vec<i64> = before.tasks.values().filter(|t| active(t)).map(|t| t.id).collect();
            expected.sort();
            let listed = reply["tasks"].as_array().cloned().unwrap_or_default();
            let ids: Vec<i64> = listed.iter().map(|t| t["id"].as_i64().unwrap()).collect();
            v.check("list: exactly the tasks that may still fire", ids == expected, || {
                format!("{ids:?} vs {expected:?}")
            });
            v.check("list: count matches", reply["count"] == json!(listed.len()), || reply["count"].to_string());
            v.check("list: timezone is the configured one", reply["timezone"] == json!(before.tz), || {
                reply["timezone"].to_string()
            });
            let listed_keys = set(&[&TASK_KEYS[..], &["last_run_at_iso", "upcoming_fires"]].concat());
            for t in &listed {
                v.check("list: row shape", keys(t) == listed_keys, || t.to_string());
                let s = before.tasks.get(&t["id"].as_i64().unwrap());
                v.check(
                    "list: each row as stored",
                    s.is_some_and(|s| {
                        t["cron_expr"] == json!(s.cron)
                            && t["prompt"] == json!(s.prompt)
                            && t["recurring"] == json!(s.recurring)
                            && t["source"] == json!(s.source)
                            && t["last_run_at"] == json!(s.last_run_at)
                    }),
                    || format!("{t} vs {s:?}"),
                );
                let want_iso = t["last_run_at"].as_i64().map(|s| iso(s * 1000));
                v.check("list: last_run_at_iso is last_run_at", t["last_run_at_iso"] == json!(want_iso), || {
                    t.to_string()
                });
                let upcoming = t["upcoming_fires"].as_array().cloned().unwrap_or_default();
                check_upcoming(&mut v, "list upcoming", t["cron_expr"].as_str().unwrap(), tz, real_now, &upcoming);
            }
        }
        Op::Tick => {
            let now = *clock;
            let n = scheduler::tick(&env.server.scheduler, &env.server.signals, Utc.timestamp_millis_opt(now).unwrap())
                .await
                .unwrap();
            let after = observe(&env.server).await;
            let fires: Vec<Fire> = after
                .signals
                .iter()
                .filter(|(id, _)| !before.signals.contains_key(id))
                .map(|(id, (source, content))| parse_fire(*id, source, content))
                .collect();
            v.check("tick: returns how many fired", n == fires.len(), || format!("{n} vs {}", fires.len()));
            let mut fired_tasks = HashSet::new();
            for f in &fires {
                let t = before.tasks.get(&f.task_id);
                v.check("tick: fires only an existing, active task", t.is_some_and(active), || format!("{f:?} {t:?}"));
                let Some(t) = t else { continue };
                v.check("tick: at most one fire per task per tick", !fired_tasks.contains(&t.id), || t.id.to_string());
                fired_tasks.insert(t.id);
                let c = parse_wall_cron(&t.cron).unwrap();
                v.check(
                    "tick: source is the task's, or scheduler",
                    f.source == t.source.clone().unwrap_or_else(|| "scheduler".into()),
                    || format!("{} vs {:?}", f.source, t.source),
                );
                v.check(
                    "tick: the signal carries the task",
                    f.cron == t.cron
                        && f.body == t.prompt
                        && f.recurring == if t.recurring { "yes" } else { "no (one-shot)" },
                    || format!("{f:?}"),
                );
                v.check("tick: Now is the tick's time", f.now_iso == iso(now), || f.now_iso.clone());
                let want_previous = t.last_run_at.map_or("never (this is the first run)".to_owned(), |s| iso(s * 1000));
                v.check("tick: Previous fire is the slot last fired for", f.previous == want_previous, || {
                    format!("{} vs {want_previous}", f.previous)
                });
                v.check("tick: never early", f.slot_ms <= now, || format!("{} > {}", iso(f.slot_ms), iso(now)));
                v.check("tick: only after the anchor", f.slot_ms > anchor_ms(t), || {
                    format!("{} vs {}", iso(f.slot_ms), iso(anchor_ms(t)))
                });
                v.check("tick: the slot is a legitimate fire time", legitimate_slot(&c, tz, f.slot_ms), || {
                    format!("{} {tz} {}", t.cron, iso(f.slot_ms))
                });
                let skipped = first_owed(&c, tz, anchor_ms(t), f.slot_ms - 1);
                v.check("tick: no owed fire is skipped", skipped.is_none(), || {
                    format!(
                        "{} {tz} anchor {} slot {} skipped {:?}",
                        t.cron,
                        iso(anchor_ms(t)),
                        iso(f.slot_ms),
                        skipped.map(iso)
                    )
                });
                let a = after.tasks.get(&t.id);
                v.check(
                    "tick: last_run_at becomes the slot",
                    a.and_then(|a| a.last_run_at) == Some(f.slot_ms / 1000),
                    || format!("{a:?} vs {}", f.slot_ms / 1000),
                );
                if !t.recurring {
                    witness("a one-shot fires");
                }
                if t.last_run_at.is_some() && t.recurring {
                    witness("a recurring task fires again");
                }
                if now - f.slot_ms > HOUR {
                    witness("a late tick catches up");
                }
                if t.source.is_some() {
                    witness("a seeded task fires under its own source");
                }
                if before.tz != "UTC" {
                    witness("a fire in a non-UTC zone");
                }
            }
            // Progress: every active task with an owed fire by now fires.
            for t in before.tasks.values() {
                if !active(t) || fired_tasks.contains(&t.id) {
                    continue;
                }
                let owed = first_owed(&parse_wall_cron(&t.cron).unwrap(), tz, anchor_ms(t), now);
                v.check("tick: a task with an owed fire fires", owed.is_none(), || {
                    format!(
                        "task {} {} {tz} anchor {} owed {:?} now {}",
                        t.id,
                        t.cron,
                        iso(anchor_ms(t)),
                        owed.map(iso),
                        iso(now)
                    )
                });
            }
            if fires.is_empty() && before.tasks.values().any(active) {
                witness("a tick with nothing due");
            }
            tasks_frame(&mut v, "tick frame: tasks that didn't fire are untouched", &before, &after, &fired_tasks);
            v.check("tick frame: no task added or removed", after.tasks.len() == before.tasks.len(), || {
                format!("{} → {}", before.tasks.len(), after.tasks.len())
            });
        }
    }
    v.0
}

async fn reset_world(env: &Env) {
    env.server
        .db
        .client()
        .await
        .unwrap()
        .batch_execute("TRUNCATE signals, scheduled_tasks RESTART IDENTITY")
        .await
        .unwrap();
    env.settings.set_timezone(tz_of("UTC")).await.unwrap();
}

// ── suites ───────────────────────────────────────────────────────────────────

#[test]
fn keeps_every_rule_across_generated_schedules_cancels_zone_changes_and_ticks() {
    let _serial = serial();
    let env = env();
    TestRunner::new(Config::with_cases(property_runs(60)))
        .run(
            &(start(), proptest::sample::select(&ZONES[..]), proptest::collection::vec(op(), 1..=40)),
            |(t0, zone, ops)| {
                runtime().block_on(async {
                    reset_world(env).await;
                    env.settings.set_timezone(tz_of(zone)).await.unwrap();
                    let mut clock = t0;
                    for (i, o) in ops.iter().enumerate() {
                        let violations = step(env, &mut clock, o).await;
                        assert!(
                            violations.is_empty(),
                            "step {i} at {} {o:?}\n  {}",
                            iso(clock),
                            violations.join("\n  ")
                        );
                    }
                });
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        WITNESSED.lock().unwrap().iter().copied().collect::<Vec<_>>(),
        [
            "a fire in a non-UTC zone",
            "a late tick catches up",
            "a one-shot fires",
            "a recurring task fires again",
            "a seeded task fires under its own source",
            "a task is cancelled",
            "a tick with nothing due",
            "an invalid cron is refused",
        ]
    );
}

// A daily reminder fires exactly once on every local day across both DST
// transitions, ticking every 10 minutes like a slightly slow poller.
fn daily_across_dst(zone: &str, cron_text: &str, from: &str) {
    let _serial = serial();
    let env = env();
    runtime().block_on(async {
        reset_world(env).await;
        let tz = tz_of(zone);
        env.settings.set_timezone(tz).await.unwrap();
        let row = env.server.scheduler.insert(cron_text, true, "pills", None).await.unwrap();
        let start = ms(from);
        set_created(env, row.id, start).await;
        let mut now = start;
        while now <= start + 5 * 24 * HOUR {
            scheduler::tick(&env.server.scheduler, &env.server.signals, Utc.timestamp_millis_opt(now).unwrap())
                .await
                .unwrap();
            now += 10 * MINUTE;
        }
        let world = observe(&env.server).await;
        let days: Vec<chrono::NaiveDate> = world
            .signals
            .iter()
            .map(|(id, (source, content))| {
                let f = parse_fire(*id, source, content);
                Utc.timestamp_millis_opt(f.slot_ms).unwrap().with_timezone(&tz).date_naive()
            })
            .collect();
        assert!(days.len() >= 4, "fired on {days:?}");
        let gaps: Vec<i64> = days.windows(2).map(|w| (w[1] - w[0]).num_days()).collect();
        assert!(gaps.iter().all(|g| *g == 1), "fired on {days:?}");
    });
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_kiev_9() {
    daily_across_dst("Europe/Kiev", "0 9 * * *", "2026-03-27T00:00:00Z");
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_new_york_9() {
    daily_across_dst("America/New_York", "0 9 * * *", "2026-03-06T00:00:00Z");
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_kiev_8_autumn() {
    daily_across_dst("Europe/Kiev", "0 8 * * *", "2026-10-23T00:00:00Z");
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_kiev_3_30_spring() {
    daily_across_dst("Europe/Kiev", "30 3 * * *", "2026-03-27T00:00:00Z");
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_kiev_3_30_autumn() {
    daily_across_dst("Europe/Kiev", "30 3 * * *", "2026-10-23T00:00:00Z");
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_new_york_1_30_autumn() {
    daily_across_dst("America/New_York", "30 1 * * *", "2026-10-30T00:00:00Z");
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_new_york_2_30_spring() {
    daily_across_dst("America/New_York", "30 2 * * *", "2026-03-06T00:00:00Z");
}

#[test]
fn a_daily_task_fires_once_per_local_day_across_dst_lord_howe_1_45() {
    daily_across_dst("Australia/Lord_Howe", "45 1 * * *", "2026-04-03T00:00:00Z");
}

// A time inside Lord Howe's 30-minute spring-forward gap. Skipped in the TS
// suite, whose cron library skips the day (see Findings); next_slot shifts it
// past the gap like any other.
#[test]
fn a_daily_task_inside_a_30_minute_dst_gap_still_fires_that_day() {
    daily_across_dst("Australia/Lord_Howe", "15 2 * * *", "2026-10-02T00:00:00Z");
}

async fn seed_at(env: &Env, cron: &str, recurring: bool, prompt: &str, created: i64) -> TaskRow {
    let row = env.server.scheduler.insert(cron, recurring, prompt, None).await.unwrap();
    set_created(env, row.id, created).await;
    row
}

fn fires(world: &World) -> Vec<Fire> {
    world.signals.iter().map(|(id, (source, content))| parse_fire(*id, source, content)).collect()
}

// Ticks overlap when one runs long. Every slot still fires once.
#[test]
fn overlapping_ticks_fire_every_slot_once() {
    let _serial = serial();
    let env = env();
    runtime().block_on(async {
        reset_world(env).await;
        let created = ms("2026-07-01T00:00:00Z");
        for i in 0..20 {
            seed_at(env, "*/5 * * * *", i % 2 == 0, &format!("t{i}"), created).await;
        }
        let now = Utc.timestamp_millis_opt(created + 7 * MINUTE).unwrap();
        let (s, sig) = (&env.server.scheduler, &env.server.signals);
        let (a, b, c) =
            tokio::join!(scheduler::tick(s, sig, now), scheduler::tick(s, sig, now), scheduler::tick(s, sig, now));
        for result in [a, b, c] {
            result.unwrap();
        }
        let fired = fires(&observe(&env.server).await);
        let keys: Vec<String> = fired.iter().map(|f| format!("{}@{}", f.task_id, f.slot_ms)).collect();
        assert_eq!(keys.len(), keys.iter().collect::<HashSet<_>>().len(), "duplicate fires: {keys:?}");
        assert_eq!(fired.iter().map(|f| f.task_id).collect::<HashSet<_>>().len(), 20);
    });
}

// A tick held between reading the task list and firing.
struct Gated<'a> {
    inner: &'a mcp_tools::scheduler::Scheduler,
    gate: tokio::sync::Notify,
}

#[async_trait]
impl TaskStore for Gated<'_> {
    async fn timezone(&self) -> Tz {
        TaskStore::timezone(self.inner).await
    }

    async fn list_active(&self) -> anyhow::Result<Vec<TaskRow>> {
        let rows = self.inner.list_active().await;
        self.gate.notified().await;
        rows
    }

    async fn swap_last_run(&self, id: i64, expected: Option<i64>, next: Option<i64>) -> anyhow::Result<bool> {
        self.inner.swap_last_run(id, expected, next).await
    }
}

// A task cancelled after a tick read the task list, but before the tick got
// to it, must not fire: the user was told it is gone.
#[test]
fn a_task_cancelled_while_a_tick_is_in_flight_does_not_fire() {
    let _serial = serial();
    let env = env();
    runtime().block_on(async {
        reset_world(env).await;
        let created = ms("2026-07-01T00:00:00Z");
        let row = seed_at(env, "0 9 * * *", false, "take pills", created).await;
        let gated = Gated { inner: &env.server.scheduler, gate: tokio::sync::Notify::new() };
        let in_flight =
            scheduler::tick(&gated, &env.server.signals, Utc.timestamp_millis_opt(created + 10 * HOUR).unwrap());
        let cancel = async {
            let reply = ok_value(call(&env.client, "cancel_scheduled_task", json!({ "id": row.id })).await, "cancel");
            assert_eq!(reply["ok"], true);
            gated.gate.notify_one();
        };
        let (fired, ()) = tokio::join!(in_flight, cancel);
        fired.unwrap();
        assert!(observe(&env.server).await.signals.is_empty());
    });
}

struct Failing;

#[async_trait]
impl SignalSink for Failing {
    async fn record(&self, _: &str, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("connection terminated unexpectedly")
    }
}

// Durable intent is not delivery: if recording the signal fails (a Postgres
// blip), the slot must still be delivered by a later tick, not silently
// marked as done.
#[test]
fn a_slot_whose_signal_failed_to_record_is_delivered_by_the_next_tick() {
    let _serial = serial();
    let env = env();
    runtime().block_on(async {
        reset_world(env).await;
        let created = ms("2026-07-01T00:00:00Z");
        let row = seed_at(env, "30 14 1 7 *", false, "take pills", created).await;
        let _ =
            scheduler::tick(&env.server.scheduler, &Failing, Utc.timestamp_millis_opt(created + 15 * HOUR).unwrap())
                .await;
        scheduler::tick(
            &env.server.scheduler,
            &env.server.signals,
            Utc.timestamp_millis_opt(created + 15 * HOUR + 30_000).unwrap(),
        )
        .await
        .unwrap();
        let fired: Vec<(i64, String)> =
            fires(&observe(&env.server).await).iter().map(|f| (f.task_id, iso(f.slot_ms))).collect();
        assert_eq!(fired, [(row.id, "2026-07-01T14:30:00.000Z".to_owned())]);
    });
}

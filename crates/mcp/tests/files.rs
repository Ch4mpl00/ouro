// read_file / read_pdf over MCP on a generated maze of paths: only files whose
// real location lies under a readable root come back; everything else —
// absolute paths elsewhere, `..` escapes, symlinks pointing out, a sibling
// directory whose name merely starts like a root — is refused before it is
// opened. A port of packages/mcp/test/files.test.ts.

mod support;

use std::collections::BTreeSet;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use mcp_tools::fs::FileAccess;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{Value, json};
use support::{Client, Outcome, Overrides, Server, TestDatabase, call, connect, runtime, start_test_server_with};

// base/
//   allowed/       a.txt  sub/b.txt  link-in → allowed/a.txt  link-out → outside/secret.txt  link-dir-out → outside/
//   allowed-evil/  c.txt               (shares a prefix with "allowed")
//   second/        d.txt               (a second root)
//   root-link → allowed                (a root given through a symlink)
//   outside/       secret.txt
const FILES: [(&str, &str); 5] = [
    ("allowed/a.txt", "alpha"),
    ("allowed/sub/b.txt", "beta"),
    ("allowed-evil/c.txt", "evil"),
    ("second/d.txt", "delta"),
    ("outside/secret.txt", "TOKEN=hunter2"),
];

struct Env {
    db: TestDatabase,
    base: PathBuf,
    _server: Server,
    client: Client,
}

fn temp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

async fn server_with(db: &TestDatabase, files: FileAccess) -> (Server, Client) {
    let server = start_test_server_with(db, Some("fs,pdf"), Overrides { files: Some(files), ..Overrides::default() })
        .await
        .expect("server starts");
    let client = connect(&server.url).await;
    (server, client)
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let base = temp_dir("files-");
        for (rel, content) in FILES {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        symlink(base.join("allowed/a.txt"), base.join("allowed/link-in")).unwrap();
        symlink(base.join("outside/secret.txt"), base.join("allowed/link-out")).unwrap();
        symlink(base.join("outside"), base.join("allowed/link-dir-out")).unwrap();
        symlink(base.join("allowed"), base.join("root-link")).unwrap();
        runtime().block_on(async {
            let db = support::test_database().await;
            let files = FileAccess::new(&[PathBuf::from("root-link"), base.join("second")], &base);
            let (server, client) = server_with(&db, files).await;
            Env { db, base, _server: server, client }
        })
    })
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, PartialEq)]
enum Readable {
    Yes,
    No,
    Missing,
}

// `path.resolve`, lexically.
fn resolve(base: &Path, input: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in base.join(input).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

// Readable iff the real path is one of the roots' real paths or below one, by
// plain string comparison on whole path segments.
fn readable(base: &Path, target: &Path) -> Readable {
    let Ok(real) = std::fs::canonicalize(target) else { return Readable::Missing };
    let roots =
        [std::fs::canonicalize(base.join("allowed")).unwrap(), std::fs::canonicalize(base.join("second")).unwrap()];
    let r = real.to_string_lossy().into_owned();
    let inside = roots.iter().any(|root| {
        let root = root.to_string_lossy().into_owned();
        r == root || r.starts_with(&format!("{root}/"))
    });
    if inside { Readable::Yes } else { Readable::No }
}

fn content(outcome: &Outcome) -> Option<&Value> {
    match outcome {
        Outcome::Ok(v) => v.get("content"),
        _ => None,
    }
}

const SEGMENTS: [&str; 17] = [
    "allowed",
    "allowed-evil",
    "second",
    "root-link",
    "outside",
    "sub",
    "..",
    ".",
    "a.txt",
    "b.txt",
    "c.txt",
    "d.txt",
    "secret.txt",
    "link-in",
    "link-out",
    "link-dir-out",
    "nope",
];

// Random walks through the maze, plus walks known to end on a readable file
// (random ones rarely do).
fn spelled() -> impl Strategy<Value = (bool, Vec<&'static str>, bool)> {
    let segments = prop_oneof![
        3 => proptest::collection::vec(proptest::sample::select(&SEGMENTS[..]), 1..=5),
        1 => proptest::sample::select(vec![
            vec!["allowed", "a.txt"],
            vec!["root-link", "sub", "b.txt"],
            vec!["second", "d.txt"],
            vec!["allowed", "link-in"],
            vec!["allowed", "sub", "..", "a.txt"],
        ]),
    ];
    (any::<bool>(), segments, any::<bool>())
}

#[test]
fn reads_a_root_created_after_startup_and_still_refuses_escaping_symlinks() {
    let _serial = serial();
    let env = env();
    runtime().block_on(async {
        let dir = temp_dir(&format!("{}/late-", env.base.display()));
        let root = dir.join("storage");
        let (_server, client) = server_with(&env.db, FileAccess::new(std::slice::from_ref(&root), &dir)).await;
        let read = |path: &'static str| call(&client, "read_file", json!({ "path": path }));
        assert!(matches!(read("storage/attachment.txt").await, Outcome::ToolError(_)));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("attachment.txt"), "downloaded attachment").unwrap();
        let outcome = read("storage/attachment.txt").await;
        assert_eq!(content(&outcome), Some(&json!("downloaded attachment")), "{outcome:?}");
        symlink(env.base.join("outside/secret.txt"), root.join("escape")).unwrap();
        for (tool, arg) in [("read_file", "path"), ("read_pdf", "filePath")] {
            assert!(
                matches!(call(&client, tool, json!({ arg: "storage/escape" })).await, Outcome::ToolError(_)),
                "{tool}"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
        assert!(matches!(read("storage/attachment.txt").await, Outcome::ToolError(_)));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("attachment.txt"), "replacement attachment").unwrap();
        assert_eq!(content(&read("storage/attachment.txt").await), Some(&json!("replacement attachment")));
    });
}

#[test]
fn opens_exactly_the_files_whose_real_location_is_under_a_root() {
    let _serial = serial();
    let env = env();
    let seen = Mutex::new(BTreeSet::new());
    TestRunner::new(Config::with_cases(600))
        .run(&spelled(), |(absolute, segments, double_slash)| {
            let rel = segments.join(if double_slash { "//" } else { "/" });
            let input = if absolute { format!("{}/{rel}", env.base.display()) } else { rel };
            let target = resolve(&env.base, &input);
            let want = readable(&env.base, &target);
            let outcome = runtime().block_on(call(&env.client, "read_file", json!({ "path": input })));
            match want {
                Readable::Yes => {
                    // A directory: refused by the read itself.
                    let Ok(text) = std::fs::read_to_string(&target) else { return Ok(()) };
                    seen.lock().unwrap().insert("readable");
                    assert_eq!(content(&outcome), Some(&json!(text)), "{input} is readable: {outcome:?}");
                }
                Readable::No | Readable::Missing => {
                    seen.lock().unwrap().insert(if want == Readable::Missing { "missing" } else { "refused" });
                    let Outcome::ToolError(message) = &outcome else { panic!("{input} must not be read: {outcome:?}") };
                    if want == Readable::No {
                        assert!(message.contains("refusing to read"), "{input}: refused, not opened: {message}");
                        assert!(!message.contains("hunter2"));
                    }
                }
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(seen.into_inner().unwrap().into_iter().collect::<Vec<_>>(), ["missing", "readable", "refused"]);
}

#[test]
fn refuses_paths_outside_the_roots() {
    let _serial = serial();
    let env = env();
    runtime().block_on(async {
        for input in [
            "/etc/passwd",
            "/proc/self/environ",
            "../outside/secret.txt",
            "allowed/link-out",
            "allowed/link-dir-out/secret.txt",
            "allowed-evil/c.txt",
            "allowed/../outside/secret.txt",
        ] {
            for (tool, arg) in [("read_file", "path"), ("read_pdf", "filePath")] {
                let outcome = call(&env.client, tool, json!({ arg: input })).await;
                let Outcome::ToolError(message) = outcome else { panic!("{tool}({input}): {outcome:?}") };
                assert!(!message.contains("hunter2"));
            }
        }
    });
}

#[test]
fn reads_through_a_symlink_that_stays_inside_and_through_a_root_given_as_a_symlink() {
    let _serial = serial();
    let env = env();
    runtime().block_on(async {
        for input in [
            "allowed/link-in".to_owned(),
            "root-link/sub/b.txt".to_owned(),
            format!("{}/second/d.txt", env.base.display()),
        ] {
            assert!(
                matches!(call(&env.client, "read_file", json!({ "path": input })).await, Outcome::Ok(_)),
                "{input}"
            );
        }
    });
}

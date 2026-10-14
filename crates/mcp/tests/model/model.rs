// The reference model of memory documents: what `patch_doc`, `append_doc`,
// `write_doc`, `revert_patch`, `read_doc` and `doc_history` mean, with nothing
// else in the way — no database, no concurrency, no indexing, no async.
//
// This file is the specification. A line-by-line port of
// packages/mcp/model/model.ts (itself a port of the Lean model once on
// feat/rust-rewrite); main.rs checks the real server against it, sequentially
// and under concurrency, and properties.rs checks the model itself.
//
// One project; documents by name. Text is a list of characters (code
// points), so every rule below is ordinary list code. Nothing here mutates:
// every call takes a world and returns a new one.

use std::collections::BTreeMap;

pub type Text = Vec<char>;

pub fn chars(s: &str) -> Text {
    s.chars().collect()
}

pub fn str_of(t: &[char]) -> String {
    t.iter().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Write,
    Append,
    Patch,
    Revert,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Write => "write",
            Kind::Append => "append",
            Kind::Patch => "patch",
            Kind::Revert => "revert",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old: String,
    pub new: String,
}

// Every change is recorded with the body it replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub kind: Kind,
    pub edits: Vec<Edit>,
    pub body_before: String,
    pub version_before: i64,
    pub version_after: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Doc {
    pub body: String,
    pub version: i64,
    // Oldest first. Never rewritten, only extended.
    pub history: Vec<Patch>,
}

pub type World = BTreeMap<String, Doc>;

// ── Calls and answers ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    // `expected`: None when creating; the current version when overwriting.
    Write { doc: String, body: String, expected: Option<i64> },
    Append { doc: String, text: String },
    Patch { doc: String, expected: i64, edits: Vec<Edit> },
    // `index`: position in the document's history, oldest = 0.
    Revert { doc: String, index: usize, rollback: bool },
    Read { doc: String },
    History { doc: String },
}

impl Cmd {
    pub fn doc(&self) -> &str {
        match self {
            Cmd::Write { doc, .. }
            | Cmd::Append { doc, .. }
            | Cmd::Patch { doc, .. }
            | Cmd::Revert { doc, .. }
            | Cmd::Read { doc }
            | Cmd::History { doc } => doc,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Written { version: i64, size_bytes: usize },
    Read { body: String, version: i64 },
    // Newest first.
    History { entries: Vec<(Kind, i64, i64)> },
    DocNotFound,
    // Overwriting an existing document without saying which version you read.
    VersionRequired { current: i64 },
    VersionConflict { current: i64 },
    // A quote matched zero times or more than once. Nothing was written.
    EditFailed,
    PatchNotFound,
    // Later patches touched this patch's text; a rollback would restore this version.
    RevertConflict { rollback_to: i64 },
    // Bad parameters: rejected before memory is consulted.
    Rejected,
}

pub fn is_error(reply: &Reply) -> bool {
    !matches!(reply, Reply::Written { .. } | Reply::Read { .. } | Reply::History { .. })
}

// ── Text rules ───────────────────────────────────────────────────────────────

fn is_prefix_at(old: &[char], s: &[char], at: usize) -> bool {
    at + old.len() <= s.len() && old.iter().enumerate().all(|(i, c)| s[at + i] == *c)
}

// How many times `old` occurs in `s`. Overlapping matches count: "aaa"
// contains "aa" twice, because the quote could mean either.
pub fn occurrences(old: &[char], s: &[char]) -> usize {
    (0..s.len()).filter(|at| is_prefix_at(old, s, *at)).count()
}

// Replace the first occurrence of `old` by `new`.
pub fn replace_first(old: &[char], replacement: &[char], s: &[char]) -> Text {
    for at in 0..s.len() {
        if is_prefix_at(old, s, at) {
            return [&s[..at], replacement, &s[at + old.len()..]].concat();
        }
    }
    s.to_vec()
}

// All edits apply, in order, or none do. Each `old` must occur exactly once in
// the text as the earlier edits left it.
pub fn apply_edits(body: &[char], edits: &[Edit]) -> Option<Text> {
    let mut text = body.to_vec();
    for e in edits {
        if occurrences(&chars(&e.old), &text) != 1 {
            return None;
        }
        text = replace_first(&chars(&e.old), &chars(&e.new), &text);
    }
    Some(text)
}

// The edits that undo `edits`. A deletion cannot be undone in place — there
// is nothing left to anchor the re-insertion to.
pub fn invert(edits: &[Edit]) -> Option<Vec<Edit>> {
    let mut out = Vec::new();
    for e in edits.iter().rev() {
        if e.new.is_empty() {
            return None;
        }
        out.push(Edit { old: e.new.clone(), new: e.old.clone() });
    }
    Some(out)
}

// Lean's Char.isWhitespace: space, tab, newline, carriage return.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

pub fn trim_end(t: &[char]) -> Text {
    let mut end = t.len();
    while end > 0 && is_whitespace(t[end - 1]) {
        end -= 1;
    }
    t[..end].to_vec()
}

pub fn trim(t: &[char]) -> Text {
    let mut start = 0;
    while start < t.len() && is_whitespace(t[start]) {
        start += 1;
    }
    trim_end(&t[start..])
}

// Appending adds a paragraph: one blank line before it, exactly one newline
// at the end.
pub fn join_paragraphs(body: &[char], addition: &[char]) -> Text {
    let base = trim_end(body);
    match (base.is_empty(), addition.is_empty()) {
        (true, true) => Vec::new(),
        (false, true) => [&base[..], &['\n']].concat(),
        (true, false) => [addition, &['\n']].concat(),
        (false, false) => [&base[..], &['\n', '\n'], addition, &['\n']].concat(),
    }
}

pub fn size_bytes(body: &str) -> usize {
    body.len()
}

// ── The operations ───────────────────────────────────────────────────────────

// Every successful write: bump the version, record what was replaced.
fn commit(w: &World, name: &str, d: &Doc, kind: Kind, edits: Vec<Edit>, body: String) -> (World, Reply) {
    let p = Patch { kind, edits, body_before: d.body.clone(), version_before: d.version, version_after: d.version + 1 };
    let mut history = d.history.clone();
    history.push(p);
    let reply = Reply::Written { version: d.version + 1, size_bytes: size_bytes(&body) };
    let mut next = w.clone();
    next.insert(name.to_owned(), Doc { body, version: d.version + 1, history });
    (next, reply)
}

fn write(w: &World, name: &str, body: &str, expected: Option<i64>) -> (World, Reply) {
    let Some(d) = w.get(name) else {
        if expected.is_none() || expected == Some(0) {
            return commit(w, name, &Doc::default(), Kind::Write, Vec::new(), body.to_owned());
        }
        return (w.clone(), Reply::VersionConflict { current: 0 });
    };
    let Some(expected) = expected else { return (w.clone(), Reply::VersionRequired { current: d.version }) };
    if expected != d.version {
        return (w.clone(), Reply::VersionConflict { current: d.version });
    }
    commit(w, name, d, Kind::Write, Vec::new(), body.to_owned())
}

fn append(w: &World, name: &str, text: &str) -> (World, Reply) {
    if text.is_empty() {
        return (w.clone(), Reply::Rejected);
    }
    let Some(d) = w.get(name) else { return (w.clone(), Reply::DocNotFound) };
    let body = str_of(&join_paragraphs(&chars(&d.body), &trim(&chars(text))));
    commit(w, name, d, Kind::Append, Vec::new(), body)
}

fn patch(w: &World, name: &str, expected: i64, edits: &[Edit]) -> (World, Reply) {
    if expected == 0 || edits.is_empty() || edits.iter().any(|e| e.old.is_empty()) {
        return (w.clone(), Reply::Rejected);
    }
    let Some(d) = w.get(name) else { return (w.clone(), Reply::DocNotFound) };
    if expected != d.version {
        return (w.clone(), Reply::VersionConflict { current: d.version });
    }
    let Some(body) = apply_edits(&chars(&d.body), edits) else { return (w.clone(), Reply::EditFailed) };
    commit(w, name, d, Kind::Patch, edits.to_vec(), str_of(&body))
}

// The newest patch, or any patch with `rollback`, restores the body it
// replaced. An older one is undone in place by inverting its edits, which
// works only while later patches left its text alone.
fn revert(w: &World, name: &str, index: usize, rollback: bool) -> (World, Reply) {
    let Some(d) = w.get(name) else { return (w.clone(), Reply::DocNotFound) };
    let Some(p) = d.history.get(index) else { return (w.clone(), Reply::PatchNotFound) };
    if rollback || index + 1 == d.history.len() {
        return commit(w, name, d, Kind::Revert, Vec::new(), p.body_before.clone());
    }
    let inverse = if p.edits.is_empty() { None } else { invert(&p.edits) };
    let body = inverse.and_then(|inverse| apply_edits(&chars(&d.body), &inverse));
    let Some(body) = body else { return (w.clone(), Reply::RevertConflict { rollback_to: p.version_before }) };
    commit(w, name, d, Kind::Revert, Vec::new(), str_of(&body))
}

pub fn step(w: &World, cmd: &Cmd) -> (World, Reply) {
    match cmd {
        Cmd::Write { doc, body, expected } => write(w, doc, body, *expected),
        Cmd::Append { doc, text } => append(w, doc, text),
        Cmd::Patch { doc, expected, edits } => patch(w, doc, *expected, edits),
        Cmd::Revert { doc, index, rollback } => revert(w, doc, *index, *rollback),
        Cmd::Read { doc } => match w.get(doc) {
            Some(d) => (w.clone(), Reply::Read { body: d.body.clone(), version: d.version }),
            None => (w.clone(), Reply::DocNotFound),
        },
        Cmd::History { doc } => match w.get(doc) {
            Some(d) => (
                w.clone(),
                Reply::History {
                    entries: d.history.iter().rev().map(|p| (p.kind, p.version_before, p.version_after)).collect(),
                },
            ),
            None => (w.clone(), Reply::DocNotFound),
        },
    }
}

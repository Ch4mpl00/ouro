// Unified memory: one memory every agent shares — the droplet supervisor,
// Claude Code sessions, ChatGPT through the tunnel. Design and rationale in
// .claude/tasks/unified-memory.md (the D-numbers below refer to it).
//
// Two read models behind one search projection (D8): projects made of
// markdown documents, and flat facts. Both feed `memory_index`, the only
// table with embeddings. The agent's flow is two-step: `recall` returns refs,
// `read_doc` / `get_fact` loads the whole thing.
//
// Sections:
//   1. types      — states, projects, documents, facts, patches, index rows
//   2. refs       — `doc:<project>/<name>#<chunk>` / `fact:<id>`, parsed strictly
//   3. patch      — search/replace edits, near-match hints, append, invert
//   4. projection — markdown chunking, index text, recency-aware ranking
//   5. store      — the dumb-CRUD port + its Postgres implementation
//   6. indexer    — keeps `memory_index` in step with the read models
//   7. service    — every rule that protects a document
//   8. import     — one-shot copy of knowledge_base_notes into facts
//   9. tools      — `memory` toolset
//
// The rules live in the service and the store underneath is dumb, so the
// whole contract is tested against an in-memory store without a Postgres.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::embeddings::SharedEmbedder;
use crate::pg::{PgPool, vector_literal};
use crate::server::{McpTools, ToolResult, invalid_params, json_result, tool_failed};
use crate::time::iso;

// ── 1. types ─────────────────────────────────────────────────────────────────

// D7 — decay is an explicit state plus a recency boost at ranking time.
// Nothing is ever deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryState {
    Active,
    Done,
    Archived,
}

impl MemoryState {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryState::Active => "active",
            MemoryState::Done => "done",
            MemoryState::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "done" => MemoryState::Done,
            "archived" => MemoryState::Archived,
            _ => MemoryState::Active,
        }
    }
}

fn ser_iso<S: serde::Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&iso(*t))
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub title: String,
    #[serde(serialize_with = "ser_iso")]
    pub created_at: DateTime<Utc>,
    #[serde(serialize_with = "ser_iso")]
    pub updated_at: DateTime<Utc>,
}

// Enough to choose a document without loading it; the summary is what keeps
// a project from growing notes.md / notes2.md / progress-new.md (D5).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DocSummary {
    pub name: String,
    pub summary: Option<String>,
    pub version: i32,
    pub size_bytes: usize,
    #[serde(serialize_with = "ser_iso")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Doc {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub summary: Option<String>,
    pub body: String,
    pub version: i32,
    pub size_bytes: usize,
    #[serde(serialize_with = "ser_iso")]
    pub updated_at: DateTime<Utc>,
}

impl Doc {
    fn summary_view(&self) -> DocSummary {
        DocSummary {
            name: self.name.clone(),
            summary: self.summary.clone(),
            version: self.version,
            size_bytes: self.size_bytes,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Fact {
    pub id: i64,
    pub body: String,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub state: MemoryState,
    #[serde(serialize_with = "ser_iso")]
    pub created_at: DateTime<Utc>,
    #[serde(serialize_with = "ser_iso")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PatchKind {
    Write,
    Append,
    Patch,
    Revert,
}

impl PatchKind {
    fn as_str(self) -> &'static str {
        match self {
            PatchKind::Write => "write",
            PatchKind::Append => "append",
            PatchKind::Patch => "patch",
            PatchKind::Revert => "revert",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "append" => PatchKind::Append,
            "patch" => PatchKind::Patch,
            "revert" => PatchKind::Revert,
            _ => PatchKind::Write,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Edit {
    /// Exact text to find, unique in the document.
    pub old: String,
    /// Replacement. Empty string deletes.
    pub new: String,
}

// One row per write. `body_before` is the whole previous document: it makes
// "roll roadmap.md back to v7" answerable and is what a failed mid-stack
// revert falls back to (D9).
#[derive(Debug, Clone, PartialEq)]
pub struct DocPatch {
    // Short and random: sequential ids invite guessing a neighbour's.
    pub pid: String,
    pub doc_id: i64,
    pub kind: PatchKind,
    pub edits: Vec<Edit>,
    pub body_before: String,
    pub version_before: i32,
    pub version_after: i32,
    pub actor: String,
    pub rationale: Option<String>,
    pub created_at: DateTime<Utc>,
}

// A row on its way into the search projection. `source_ref` is the owning
// object and what a re-index deletes by — exact equality, never a prefix, so
// `fact:88` can't wipe `fact:880`.
#[derive(Debug, Clone)]
pub struct IndexUpsert {
    pub source_ref: String,
    pub r#ref: String,
    pub text: String,
    pub tags: Vec<String>,
    pub actor: Option<String>,
    pub state: MemoryState,
    pub ts: DateTime<Utc>,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub struct IndexHit {
    pub id: i64,
    pub r#ref: String,
    pub text: String,
    pub tags: Vec<String>,
    pub actor: Option<String>,
    pub state: MemoryState,
    pub ts: DateTime<Utc>,
    pub distance: f64,
}

// ── 2. refs ──────────────────────────────────────────────────────────────────

// Refs travel through an LLM, so they are parsed strictly: one that doesn't
// round-trip is a bug to see, not something to guess at.
fn slug_re() -> regex::Regex {
    regex::Regex::new(r"^[a-z0-9][a-z0-9-]*$").expect("valid regex")
}

// Markdown filenames, no directories: a project is a flat folder, and a
// separator would make refs ambiguous.
fn doc_name_re() -> regex::Regex {
    regex::Regex::new(r"^[a-z0-9][a-z0-9._-]*\.md$").expect("valid regex")
}

pub fn assert_project_slug(slug: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        slug_re().is_match(slug),
        "Invalid project slug \"{slug}\". Use lowercase letters, digits and hyphens, e.g. \"leetcode-graphs\"."
    );
    Ok(())
}

pub fn assert_doc_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        doc_name_re().is_match(name),
        "Invalid document name \"{name}\". Use a lowercase markdown filename with no directories, e.g. \"roadmap.md\"."
    );
    Ok(())
}

pub fn doc_ref(project: &str, doc: &str, chunk: Option<usize>) -> String {
    match chunk {
        Some(c) => format!("doc:{project}/{doc}#{c}"),
        None => format!("doc:{project}/{doc}"),
    }
}

pub fn fact_ref(id: i64) -> String {
    format!("fact:{id}")
}

#[derive(Debug, PartialEq)]
pub enum MemoryRef {
    Doc { project: String, doc: String, chunk: Option<usize> },
    Fact { id: i64 },
}

pub fn parse_ref(raw: &str) -> Option<MemoryRef> {
    if let Some(id) = raw.strip_prefix("fact:") {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        return Some(MemoryRef::Fact { id: id.parse().ok()? });
    }
    let caps = regex::Regex::new(r"^doc:([^/]+)/([^#]+)(?:#(\d+))?$").expect("valid regex").captures(raw)?;
    let (project, doc) = (caps[1].to_owned(), caps[2].to_owned());
    if !slug_re().is_match(&project) || !doc_name_re().is_match(&doc) {
        return None;
    }
    Some(MemoryRef::Doc { project, doc, chunk: caps.get(3).and_then(|c| c.as_str().parse().ok()) })
}

// ── 3. patch ─────────────────────────────────────────────────────────────────

// Search/replace on quoted literals, never line numbers (D10): an off-by-one
// line number corrupts silently, a quote that doesn't match fails loudly and
// changes nothing. Pure — body in, body out.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditFailureReason {
    Empty,
    NotFound,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EditFailure {
    // Position in the caller's edits, so the model fixes one edit rather
    // than resending the whole call blind.
    pub index: usize,
    pub old: String,
    pub reason: EditFailureReason,
    pub occurrences: usize,
    // Literal text that *nearly* matched. Suggested, never applied:
    // fuzzy-applying is how an agent silently edits the wrong sentence.
    pub suggestions: Vec<String>,
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

// Atomic: all edits apply or none do. Applied in order to a working copy, so
// a later edit may target text an earlier one produced.
pub fn apply_edits(body: &str, edits: &[Edit]) -> Result<String, Vec<EditFailure>> {
    let failure = |index, old: &str, reason, occurrences, suggestions| EditFailure {
        index,
        old: old.to_owned(),
        reason,
        occurrences,
        suggestions,
    };
    if edits.is_empty() {
        return Err(vec![failure(0, "", EditFailureReason::Empty, 0, Vec::new())]);
    }
    let mut working = body.to_owned();
    let mut failures = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        // An empty `old` matches everywhere; "insert at the start" is what
        // append_doc is for.
        if edit.old.is_empty() {
            failures.push(failure(index, &edit.old, EditFailureReason::Empty, 0, Vec::new()));
            continue;
        }
        match count_occurrences(&working, &edit.old) {
            1 => working = working.replacen(&edit.old, &edit.new, 1),
            // Never "take the first match": quote more context.
            0 => failures.push(failure(
                index,
                &edit.old,
                EditFailureReason::NotFound,
                0,
                find_near_matches(&working, &edit.old, 3),
            )),
            n => failures.push(failure(index, &edit.old, EditFailureReason::Ambiguous, n, Vec::new())),
        }
    }
    if failures.is_empty() { Ok(working) } else { Err(failures) }
}

// The inverse of a patch, for reverting one that is no longer the newest. A
// deletion has no inverse — re-inserting needs a position whose anchor is
// gone — so callers fall back to a whole-document rollback.
pub fn invert_edits(edits: &[Edit]) -> Option<Vec<Edit>> {
    edits.iter().rev().map(|e| (!e.new.is_empty()).then(|| Edit { old: e.new.clone(), new: e.old.clone() })).collect()
}

// The usual miss is a *normalised* quote: an em-dash retyped as a hyphen, a
// «quote» straightened, ё written as е, whitespace reflowed. Normalising both
// sides and mapping the hit back to the original characters hands the agent
// the exact literal to retry with.
struct Normalised {
    text: Vec<char>,
    // origin[i] = index in the source chars of normalised char i.
    origin: Vec<usize>,
}

fn is_dash(c: char) -> bool {
    ('\u{2010}'..='\u{2015}').contains(&c) || c == '\u{2212}'
}

fn is_quote(c: char) -> bool {
    matches!(c, '«' | '»' | '“' | '”' | '„' | '‘' | '’' | '′' | '″')
}

fn normalise(source: &[char]) -> Normalised {
    let mut text = Vec::with_capacity(source.len());
    let mut origin = Vec::with_capacity(source.len());
    let mut pending_space = false;
    for (i, &raw) in source.iter().enumerate() {
        if raw.is_whitespace() || raw == '\u{feff}' {
            // Collapse runs; never let one start the string.
            pending_space = !text.is_empty();
            continue;
        }
        if pending_space {
            text.push(' ');
            origin.push(i);
            pending_space = false;
        }
        for c in raw.to_lowercase() {
            let mapped = if is_dash(c) {
                '-'
            } else if is_quote(c) {
                '"'
            } else if c == 'ё' {
                'е'
            } else {
                c
            };
            text.push(mapped);
            origin.push(i);
        }
    }
    Normalised { text, origin }
}

fn find_chars(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&at| haystack[at..at + needle.len()] == *needle)
}

pub fn find_near_matches(body: &str, needle: &str, limit: usize) -> Vec<String> {
    let body_chars: Vec<char> = body.chars().collect();
    let n_body = normalise(&body_chars);
    let n_needle = normalise(&needle.chars().collect::<Vec<_>>());
    if n_needle.text.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut from = 0;
    while spans.len() < limit {
        let Some(at) = find_chars(&n_body.text, &n_needle.text, from) else { break };
        let (start, end) = (n_body.origin[at], n_body.origin[at + n_needle.text.len() - 1]);
        spans.push(body_chars[start..=end].iter().collect());
        from = at + n_needle.text.len().max(1);
    }
    if !spans.is_empty() {
        return spans;
    }
    // Nothing matched even loosely: the most similar lines at least tell the
    // agent where to look.
    let target: Vec<char> = n_needle.text.iter().take(200).copied().collect();
    let mut scored: Vec<(&str, f64)> = body
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(|line| (line, dice(&normalise(&line.chars().collect::<Vec<_>>()).text, &target)))
        .filter(|(_, score)| *score >= 0.4)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.into_iter().take(limit).map(|(line, _)| line.to_owned()).collect()
}

// Bigram Dice coefficient: cheap, and enough to rank "which line did they
// mean" without pretending to be a diff.
fn dice(a: &[char], b: &[char]) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.len() < 2 || b.len() < 2 {
        return 0.0;
    }
    let mut bigrams: HashMap<(char, char), usize> = HashMap::new();
    for w in a.windows(2) {
        *bigrams.entry((w[0], w[1])).or_default() += 1;
    }
    let mut hits = 0;
    for w in b.windows(2) {
        if let Some(left) = bigrams.get_mut(&(w[0], w[1]))
            && *left > 0
        {
            *left -= 1;
            hits += 1;
        }
    }
    (2 * hits) as f64 / (a.len() - 1 + b.len() - 1) as f64
}

#[derive(Debug, Clone, PartialEq)]
pub struct Heading {
    pub level: usize,
    pub text: String,
    pub line: usize,
}

fn heading_re() -> regex::Regex {
    regex::Regex::new(r"^(#{1,6})\s+(.+?)\s*$").expect("valid regex")
}

pub fn list_headings(body: &str) -> Vec<Heading> {
    let re = heading_re();
    body.split('\n')
        .enumerate()
        .filter_map(|(line, text)| {
            let caps = re.captures(text)?;
            Some(Heading { level: caps[1].len(), text: caps[2].to_owned(), line })
        })
        .collect()
}

// The safe default (D10, op 1): it cannot destroy text and needs no read.
// Headings are the anchors because they survive edits elsewhere.
pub fn append_to_body(body: &str, text: &str, under_heading: Option<&str>) -> Result<String, Vec<String>> {
    let addition = text.trim();
    let Some(heading) = under_heading.filter(|h| !h.trim().is_empty()) else {
        return Ok(join_blocks(body, addition));
    };
    let headings = list_headings(body);
    // "Progress" as readily as "## Progress".
    let wanted = normalise(&heading.trim_start_matches('#').trim_start().chars().collect::<Vec<_>>()).text;
    let Some(target) = headings.iter().find(|h| normalise(&h.text.chars().collect::<Vec<_>>()).text == wanted) else {
        return Err(headings.iter().map(|h| format!("{} {}", "#".repeat(h.level), h.text)).collect());
    };
    let lines: Vec<&str> = body.split('\n').collect();
    // The section runs to the next heading of the same or higher rank; a
    // deeper sub-heading is still part of it.
    let end = headings.iter().find(|h| h.line > target.line && h.level <= target.level).map_or(lines.len(), |h| h.line);
    let (head, tail) = (lines[..end].join("\n"), lines[end..].join("\n"));
    let merged = join_blocks(&head, addition);
    Ok(if tail.is_empty() { merged } else { format!("{merged}\n{tail}") })
}

// One blank line between blocks, exactly one trailing newline: stable
// formatting keeps later quotes matching what the agent last saw.
fn join_blocks(existing: &str, addition: &str) -> String {
    let base = existing.trim_end();
    match (base.is_empty(), addition.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("{base}\n"),
        (true, false) => format!("{addition}\n"),
        (false, false) => format!("{base}\n\n{addition}\n"),
    }
}

// ── 4. projection ────────────────────────────────────────────────────────────

pub const DEFAULT_CHUNK_CHARS: usize = 1200;

#[derive(Debug, Clone, PartialEq)]
pub struct MarkdownChunk {
    pub text: String,
    // Breadcrumb of enclosing headings ("Progress > Notes"); empty before
    // the first heading.
    pub heading_path: String,
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

// Paragraphs packed up to `max_chars`, split at every heading (the strongest
// topical boundary markdown has), never mid-paragraph unless one alone is
// oversized. A month of daily entries must not become one vector (D8).
pub fn chunk_markdown(body: &str, max_chars: usize) -> Vec<MarkdownChunk> {
    let heading = heading_re();
    let mut chunks = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut buffer: Vec<String> = Vec::new();
    let mut buffer_path = String::new();
    let path_now = |stack: &[(usize, String)]| stack.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" > ");
    let flush = |buffer: &mut Vec<String>, path: &str, chunks: &mut Vec<MarkdownChunk>| {
        let text = buffer.join("\n\n").trim().to_owned();
        buffer.clear();
        if !text.is_empty() {
            chunks.push(MarkdownChunk { text, heading_path: path.to_owned() });
        }
    };

    for block in regex::Regex::new(r"\n\s*\n").expect("valid regex").split(body) {
        let paragraph = block.trim();
        if paragraph.is_empty() {
            continue;
        }
        if let Some(caps) = heading.captures(paragraph) {
            flush(&mut buffer, &buffer_path, &mut chunks);
            let level = caps[1].len();
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                stack.pop();
            }
            stack.push((level, caps[2].to_owned()));
            buffer_path = path_now(&stack);
            continue;
        }
        if buffer.is_empty() {
            buffer_path = path_now(&stack);
        }
        let projected = buffer.iter().map(|p| char_len(p)).sum::<usize>() + 2 * buffer.len() + char_len(paragraph);
        if !buffer.is_empty() && projected > max_chars {
            flush(&mut buffer, &buffer_path, &mut chunks);
            buffer_path = path_now(&stack);
        }
        if char_len(paragraph) > max_chars {
            // A pasted log or a long table still has to fit the embedder.
            let chars: Vec<char> = paragraph.chars().collect();
            for piece in chars.chunks(max_chars) {
                chunks.push(MarkdownChunk { text: piece.iter().collect(), heading_path: path_now(&stack) });
            }
            continue;
        }
        buffer.push(paragraph.to_owned());
    }
    flush(&mut buffer, &buffer_path, &mut chunks);
    chunks
}

// Chunks must be self-contained; documents must not (D8). "застрял на
// Dijkstra" matches nothing alone, so the subject goes into the indexed text
// while the stored document stays clean.
pub fn build_index_text(project_title: &str, doc_name: &str, heading_path: &str, text: &str) -> String {
    if heading_path.is_empty() {
        format!("{project_title} — {doc_name}\n\n{text}")
    } else {
        format!("{project_title} — {doc_name} · {heading_path}\n\n{text}")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RankOpts {
    // Days for the recency bonus to halve.
    pub half_life_days: f64,
    // How much a brand-new row may improve its distance. Small on purpose:
    // recency breaks ties, it must not float an irrelevant note over a
    // relevant one (D7).
    pub recency_weight: f64,
}

pub const DEFAULT_RANK: RankOpts = RankOpts { half_life_days: 30.0, recency_weight: 0.05 };

pub fn rank_hits(hits: Vec<IndexHit>, now: DateTime<Utc>, opts: RankOpts) -> Vec<(IndexHit, f64)> {
    let mut ranked: Vec<(IndexHit, f64)> = hits
        .into_iter()
        .map(|hit| {
            let age_days = ((now - hit.ts).num_milliseconds() as f64 / 86_400_000.0).max(0.0);
            let boost = opts.recency_weight * 2f64.powf(-age_days / opts.half_life_days);
            let score = hit.distance - boost;
            (hit, score)
        })
        .collect();
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
    ranked
}

// ── 5. store ─────────────────────────────────────────────────────────────────

pub struct NewPatch {
    pub pid: String,
    pub doc_id: i64,
    pub kind: PatchKind,
    pub edits: Vec<Edit>,
    pub body_before: String,
    pub version_before: i32,
    pub version_after: i32,
    pub actor: String,
    pub rationale: Option<String>,
}

pub struct FactUpdate<'a> {
    pub body: Option<&'a str>,
    pub tags: Option<Vec<String>>,
    pub state: Option<MemoryState>,
}

// Dumb CRUD: no rules here, only the persistence the service's rules are
// expressed in. The one piece of real logic is the compare-and-swap.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn create_project(&self, slug: &str, title: &str) -> anyhow::Result<Project>;
    async fn get_project(&self, slug: &str) -> anyhow::Result<Option<Project>>;
    async fn list_projects(&self) -> anyhow::Result<Vec<Project>>;

    async fn list_docs(&self, project_id: i64) -> anyhow::Result<Vec<DocSummary>>;
    async fn get_doc(&self, project_id: i64, name: &str) -> anyhow::Result<Option<Doc>>;
    async fn create_doc(&self, project_id: i64, name: &str, summary: Option<&str>, body: &str) -> anyhow::Result<Doc>;
    // None when `expected_version` no longer matches — the whole concurrency
    // story for several agents on one document (D1). `summary: None` keeps it.
    async fn update_doc(
        &self,
        doc_id: i64,
        expected_version: i32,
        body: &str,
        summary: Option<Option<&str>>,
    ) -> anyhow::Result<Option<Doc>>;

    async fn insert_patch(&self, patch: NewPatch) -> anyhow::Result<DocPatch>;
    // Newest first.
    async fn list_patches(&self, doc_id: i64, limit: i64) -> anyhow::Result<Vec<DocPatch>>;
    async fn get_patch(&self, doc_id: i64, pid: &str) -> anyhow::Result<Option<DocPatch>>;

    async fn create_fact(&self, body: &str, tags: &[String], source: Option<&str>) -> anyhow::Result<Fact>;
    async fn get_fact(&self, id: i64) -> anyhow::Result<Option<Fact>>;
    // By provenance — what makes the knowledge_base_notes import re-runnable.
    async fn get_fact_by_source(&self, source: &str) -> anyhow::Result<Option<Fact>>;
    async fn update_fact(&self, id: i64, update: FactUpdate<'_>) -> anyhow::Result<Option<Fact>>;

    // Replaces every row owned by `source_ref`, so a shrunken document leaves
    // no orphaned chunks answering recalls.
    async fn replace_index(&self, source_ref: &str, entries: Vec<IndexUpsert>) -> anyhow::Result<()>;
    async fn search_index(
        &self,
        embedding: &[f32],
        limit: i64,
        states: &[MemoryState],
        tags: &[String],
    ) -> anyhow::Result<Vec<IndexHit>>;
    async fn list_unembedded(&self, limit: i64) -> anyhow::Result<Vec<(i64, String)>>;
    async fn set_embedding(&self, id: i64, embedding: &[f32]) -> anyhow::Result<()>;
}

pub struct PgMemoryStore {
    pool: PgPool,
}

impl PgMemoryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn project_of(r: &tokio_postgres::Row) -> Project {
    Project {
        id: r.get("id"),
        slug: r.get("slug"),
        title: r.get("title"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

fn doc_of(r: &tokio_postgres::Row) -> Doc {
    let body: String = r.get("body_md");
    Doc {
        id: r.get("id"),
        project_id: r.get("project_id"),
        name: r.get("name"),
        summary: r.get("summary"),
        size_bytes: body.len(),
        body,
        version: r.get("version"),
        updated_at: r.get("updated_at"),
    }
}

fn patch_of(r: &tokio_postgres::Row) -> DocPatch {
    DocPatch {
        pid: r.get("pid"),
        doc_id: r.get("doc_id"),
        kind: PatchKind::parse(r.get("kind")),
        edits: serde_json::from_value(r.get("edits")).unwrap_or_default(),
        body_before: r.get("body_before"),
        version_before: r.get("version_before"),
        version_after: r.get("version_after"),
        actor: r.get("actor"),
        rationale: r.get("rationale"),
        created_at: r.get("created_at"),
    }
}

fn fact_of(r: &tokio_postgres::Row) -> Fact {
    Fact {
        id: r.get("id"),
        body: r.get("body"),
        tags: r.get("tags"),
        source: r.get("source"),
        state: MemoryState::parse(r.get("state")),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

#[async_trait]
impl MemoryStore for PgMemoryStore {
    async fn create_project(&self, slug: &str, title: &str) -> anyhow::Result<Project> {
        let row = self
            .pool
            .get()
            .await?
            .query_one("INSERT INTO memory_projects (slug, title) VALUES ($1, $2) RETURNING *", &[&slug, &title])
            .await?;
        Ok(project_of(&row))
    }

    async fn get_project(&self, slug: &str) -> anyhow::Result<Option<Project>> {
        let row =
            self.pool.get().await?.query_opt("SELECT * FROM memory_projects WHERE slug = $1 LIMIT 1", &[&slug]).await?;
        Ok(row.as_ref().map(project_of))
    }

    async fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        let rows = self.pool.get().await?.query("SELECT * FROM memory_projects ORDER BY slug", &[]).await?;
        Ok(rows.iter().map(project_of).collect())
    }

    async fn list_docs(&self, project_id: i64) -> anyhow::Result<Vec<DocSummary>> {
        let rows = self
            .pool
            .get()
            .await?
            .query("SELECT * FROM memory_project_docs WHERE project_id = $1 ORDER BY name", &[&project_id])
            .await?;
        Ok(rows.iter().map(|r| doc_of(r).summary_view()).collect())
    }

    async fn get_doc(&self, project_id: i64, name: &str) -> anyhow::Result<Option<Doc>> {
        let row = self
            .pool
            .get()
            .await?
            .query_opt(
                "SELECT * FROM memory_project_docs WHERE project_id = $1 AND name = $2 LIMIT 1",
                &[&project_id, &name],
            )
            .await?;
        Ok(row.as_ref().map(doc_of))
    }

    async fn create_doc(&self, project_id: i64, name: &str, summary: Option<&str>, body: &str) -> anyhow::Result<Doc> {
        let row = self
            .pool
            .get()
            .await?
            .query_one(
                "INSERT INTO memory_project_docs (project_id, name, summary, body_md) VALUES ($1, $2, $3, $4) RETURNING *",
                &[&project_id, &name, &summary, &body],
            )
            .await?;
        Ok(doc_of(&row))
    }

    async fn update_doc(
        &self,
        doc_id: i64,
        expected_version: i32,
        body: &str,
        summary: Option<Option<&str>>,
    ) -> anyhow::Result<Option<Doc>> {
        // The version in the WHERE clause matches no row once another agent
        // has moved on; the caller reports a conflict instead of overwriting.
        let client = self.pool.get().await?;
        let row =
            match summary {
                None => {
                    client
                        .query_opt(
                            "UPDATE memory_project_docs SET body_md = $1, version = $2, updated_at = now()
                          WHERE id = $3 AND version = $4 RETURNING *",
                            &[&body, &(expected_version + 1), &doc_id, &expected_version],
                        )
                        .await?
                }
                Some(summary) => client
                    .query_opt(
                        "UPDATE memory_project_docs SET body_md = $1, summary = $2, version = $3, updated_at = now()
                          WHERE id = $4 AND version = $5 RETURNING *",
                        &[&body, &summary, &(expected_version + 1), &doc_id, &expected_version],
                    )
                    .await?,
            };
        Ok(row.as_ref().map(doc_of))
    }

    async fn insert_patch(&self, p: NewPatch) -> anyhow::Result<DocPatch> {
        let row = self
            .pool
            .get()
            .await?
            .query_one(
                "INSERT INTO memory_doc_patches (doc_id, pid, kind, edits, body_before, version_before, version_after, actor, rationale)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *",
                &[
                    &p.doc_id,
                    &p.pid,
                    &p.kind.as_str(),
                    &serde_json::to_value(&p.edits)?,
                    &p.body_before,
                    &p.version_before,
                    &p.version_after,
                    &p.actor,
                    &p.rationale,
                ],
            )
            .await?;
        Ok(patch_of(&row))
    }

    async fn list_patches(&self, doc_id: i64, limit: i64) -> anyhow::Result<Vec<DocPatch>> {
        let rows = self
            .pool
            .get()
            .await?
            .query(
                "SELECT * FROM memory_doc_patches WHERE doc_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
                &[&doc_id, &limit],
            )
            .await?;
        Ok(rows.iter().map(patch_of).collect())
    }

    async fn get_patch(&self, doc_id: i64, pid: &str) -> anyhow::Result<Option<DocPatch>> {
        let row = self
            .pool
            .get()
            .await?
            .query_opt("SELECT * FROM memory_doc_patches WHERE doc_id = $1 AND pid = $2 LIMIT 1", &[&doc_id, &pid])
            .await?;
        Ok(row.as_ref().map(patch_of))
    }

    async fn create_fact(&self, body: &str, tags: &[String], source: Option<&str>) -> anyhow::Result<Fact> {
        let row = self
            .pool
            .get()
            .await?
            .query_one(
                "INSERT INTO memory_facts (body, tags, source) VALUES ($1, $2, $3) RETURNING *",
                &[&body, &tags, &source],
            )
            .await?;
        Ok(fact_of(&row))
    }

    async fn get_fact(&self, id: i64) -> anyhow::Result<Option<Fact>> {
        let row = self.pool.get().await?.query_opt("SELECT * FROM memory_facts WHERE id = $1 LIMIT 1", &[&id]).await?;
        Ok(row.as_ref().map(fact_of))
    }

    async fn get_fact_by_source(&self, source: &str) -> anyhow::Result<Option<Fact>> {
        let row = self
            .pool
            .get()
            .await?
            .query_opt("SELECT * FROM memory_facts WHERE source = $1 LIMIT 1", &[&source])
            .await?;
        Ok(row.as_ref().map(fact_of))
    }

    async fn update_fact(&self, id: i64, update: FactUpdate<'_>) -> anyhow::Result<Option<Fact>> {
        let row = self
            .pool
            .get()
            .await?
            .query_opt(
                "UPDATE memory_facts SET body = COALESCE($1, body), tags = COALESCE($2, tags),
                        state = COALESCE($3, state), updated_at = now()
                  WHERE id = $4 RETURNING *",
                &[&update.body, &update.tags, &update.state.map(MemoryState::as_str), &id],
            )
            .await?;
        Ok(row.as_ref().map(fact_of))
    }

    async fn replace_index(&self, source_ref: &str, entries: Vec<IndexUpsert>) -> anyhow::Result<()> {
        // Delete-then-insert in one transaction rather than upsert: a
        // re-indexed document can have fewer chunks than before.
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        tx.execute("DELETE FROM memory_index WHERE source_ref = $1", &[&source_ref]).await?;
        for e in &entries {
            let embedding = e.embedding.as_deref().map(vector_literal);
            tx.execute(
                "INSERT INTO memory_index (source_ref, ref, text, tags, actor, state, ts, embedding, embedded_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::vector, CASE WHEN $8::text IS NULL THEN NULL ELSE now() END)",
                &[&e.source_ref, &e.r#ref, &e.text, &e.tags, &e.actor, &e.state.as_str(), &e.ts, &embedding],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn search_index(
        &self,
        embedding: &[f32],
        limit: i64,
        states: &[MemoryState],
        tags: &[String],
    ) -> anyhow::Result<Vec<IndexHit>> {
        let states: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
        let tags: Vec<String> = tags.iter().filter(|t| !t.is_empty()).cloned().collect();
        let rows = self
            .pool
            .get()
            .await?
            .query(
                "SELECT id, ref, text, tags, actor, state, ts, (embedding <=> $1::text::vector) AS distance
                   FROM memory_index
                  WHERE embedding IS NOT NULL AND state = ANY($2) AND (cardinality($3::text[]) = 0 OR tags && $3)
                  ORDER BY distance LIMIT $4",
                &[&vector_literal(embedding), &states, &tags, &limit],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| IndexHit {
                id: r.get("id"),
                r#ref: r.get("ref"),
                text: r.get("text"),
                tags: r.get("tags"),
                actor: r.get("actor"),
                state: MemoryState::parse(r.get("state")),
                ts: r.get("ts"),
                distance: r.get("distance"),
            })
            .collect())
    }

    async fn list_unembedded(&self, limit: i64) -> anyhow::Result<Vec<(i64, String)>> {
        let rows = self
            .pool
            .get()
            .await?
            .query("SELECT id, text FROM memory_index WHERE embedding IS NULL LIMIT $1", &[&limit])
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    async fn set_embedding(&self, id: i64, embedding: &[f32]) -> anyhow::Result<()> {
        self.pool
            .get()
            .await?
            .execute(
                "UPDATE memory_index SET embedding = $1::text::vector, embedded_at = now() WHERE id = $2",
                &[&vector_literal(embedding), &id],
            )
            .await?;
        Ok(())
    }
}

// ── 6. indexer ───────────────────────────────────────────────────────────────

// Chunk, denormalise, embed — inline after each write. With the provider
// down a row still lands with a NULL vector: reads and patches never depend
// on OpenAI, only searchability may lag (D4).
#[derive(Clone)]
pub struct Indexer {
    store: Arc<dyn MemoryStore>,
    embedder: SharedEmbedder,
    chunk_chars: usize,
}

impl Indexer {
    pub fn new(store: Arc<dyn MemoryStore>, embedder: SharedEmbedder) -> Self {
        Self { store, embedder, chunk_chars: DEFAULT_CHUNK_CHARS }
    }

    async fn embed_texts(&self, texts: &[String]) -> Vec<Option<Vec<f32>>> {
        if texts.is_empty() {
            return Vec::new();
        }
        match self.embedder.embed_batch(texts).await {
            Ok(vectors) => {
                let mut vectors = vectors.into_iter();
                texts.iter().map(|_| vectors.next()).collect()
            }
            Err(err) => {
                tracing::error!(chunks = texts.len(), error = %format!("{err:#}"), "memory index embed failed");
                texts.iter().map(|_| None).collect()
            }
        }
    }

    pub async fn index_doc(&self, project: &Project, doc: &Doc) -> anyhow::Result<()> {
        let chunks = chunk_markdown(&doc.body, self.chunk_chars);
        let texts: Vec<String> =
            chunks.iter().map(|c| build_index_text(&project.title, &doc.name, &c.heading_path, &c.text)).collect();
        let vectors = self.embed_texts(&texts).await;
        let owner = doc_ref(&project.slug, &doc.name, None);
        let entries = texts
            .into_iter()
            .zip(vectors)
            .enumerate()
            .map(|(i, (text, embedding))| IndexUpsert {
                source_ref: owner.clone(),
                r#ref: doc_ref(&project.slug, &doc.name, Some(i)),
                text,
                tags: Vec::new(),
                actor: None,
                // Documents have no lifecycle of their own.
                state: MemoryState::Active,
                ts: doc.updated_at,
                embedding,
            })
            .collect();
        // Always replace, even with nothing: an emptied document must stop
        // answering from its old chunks.
        self.store.replace_index(&owner, entries).await
    }

    pub async fn index_fact(&self, fact: &Fact) -> anyhow::Result<()> {
        let embedding = self.embed_texts(std::slice::from_ref(&fact.body)).await.into_iter().next().flatten();
        let owner = fact_ref(fact.id);
        self.store
            .replace_index(
                &owner,
                vec![IndexUpsert {
                    source_ref: owner.clone(),
                    r#ref: owner.clone(),
                    text: fact.body.clone(),
                    tags: fact.tags.clone(),
                    actor: fact.source.clone(),
                    // Archiving drops a fact from default recall, deleting nothing.
                    state: fact.state,
                    ts: fact.updated_at,
                    embedding,
                }],
            )
            .await
    }

    // Rows a failed inline embed left NULL. 0/0 when drained.
    pub async fn embed_missing_batch(&self, batch: i64) -> anyhow::Result<crate::news::EmbedResult> {
        let rows = self.store.list_unembedded(batch).await?;
        if rows.is_empty() {
            return Ok(crate::news::EmbedResult::default());
        }
        let texts: Vec<String> = rows.iter().map(|(_, t)| t.clone()).collect();
        let vectors = self.embed_texts(&texts).await;
        let mut embedded = 0;
        for ((id, _), vector) in rows.iter().zip(vectors) {
            if let Some(vector) = vector {
                self.store.set_embedding(*id, &vector).await?;
                embedded += 1;
            }
        }
        Ok(crate::news::EmbedResult { embedded, failed: rows.len() - embedded })
    }

    // None when the provider is unreachable — recall reports "search is
    // down" rather than an empty memory.
    async fn embed_query(&self, query: &str) -> Option<Vec<f32>> {
        self.embed_texts(&[query.to_owned()]).await.into_iter().next().flatten()
    }
}

// ── 7. service ───────────────────────────────────────────────────────────────

// Expected failures are values, not stack traces: an agent that gets
// `{ error: "version_conflict", currentVersion: 9 }` knows what to do next.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct MemoryError {
    pub code: &'static str,
    pub message: String,
    pub details: Map<String, Value>,
}

fn memory_error(code: &'static str, message: impl Into<String>, details: Value) -> anyhow::Error {
    let details = match details {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    MemoryError { code, message: message.into(), details }.into()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResult {
    pub project: String,
    pub doc: String,
    pub version: i32,
    pub patch_id: String,
    pub size_bytes: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub patch_id: String,
    pub kind: PatchKind,
    pub actor: String,
    pub rationale: Option<String>,
    pub version_before: i32,
    pub version_after: i32,
    pub edit_count: usize,
    #[serde(serialize_with = "ser_iso")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct RecallHit {
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub text: String,
    pub score: f64,
    pub distance: f64,
    pub state: MemoryState,
    pub actor: Option<String>,
    #[serde(serialize_with = "ser_iso")]
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct DocList {
    pub project: Project,
    pub docs: Vec<DocSummary>,
}

pub struct WriteDoc<'a> {
    pub project: &'a str,
    pub doc: &'a str,
    pub body: &'a str,
    // None keeps the stored summary.
    pub summary: Option<&'a str>,
    // Required once the document exists; absent or 0 when creating it.
    pub expected_version: Option<i32>,
    pub actor: &'a str,
    pub rationale: Option<&'a str>,
}

// append_doc takes no version — it is the version-free safe op — so it
// absorbs a race by re-reading instead of pushing it onto the caller.
const APPEND_CAS_ATTEMPTS: usize = 3;

pub type PatchIdGen = Arc<dyn Fn() -> String + Send + Sync>;

// One write, as `commit` records it.
struct Change<'a> {
    body: &'a str,
    // None keeps the stored summary.
    summary: Option<Option<&'a str>>,
    kind: PatchKind,
    edits: Vec<Edit>,
    actor: &'a str,
    rationale: Option<String>,
}

#[derive(Clone)]
pub struct MemoryService {
    store: Arc<dyn MemoryStore>,
    indexer: Indexer,
    new_patch_id: PatchIdGen,
}

fn random_patch_id() -> String {
    format!("pa:{:06x}", rand::random::<u32>() & 0xff_ffff)
}

fn normalize_tags(tags: Option<&[String]>) -> Vec<String> {
    crate::knowledge::normalize_tags(tags)
}

impl MemoryService {
    pub fn new(store: Arc<dyn MemoryStore>, embedder: SharedEmbedder) -> Self {
        let indexer = Indexer::new(store.clone(), embedder);
        Self { store, indexer, new_patch_id: Arc::new(random_patch_id) }
    }

    #[cfg(test)]
    fn with_patch_ids(mut self, generator: PatchIdGen) -> Self {
        self.new_patch_id = generator;
        self
    }

    pub fn indexer(&self) -> &Indexer {
        &self.indexer
    }

    pub fn store(&self) -> &Arc<dyn MemoryStore> {
        &self.store
    }

    async fn require_project(&self, slug: &str) -> anyhow::Result<Project> {
        assert_project_slug(slug)?;
        if let Some(project) = self.store.get_project(slug).await? {
            return Ok(project);
        }
        let projects: Vec<String> = self.store.list_projects().await?.into_iter().map(|p| p.slug).collect();
        Err(memory_error("project_not_found", format!("No project \"{slug}\"."), json!({ "projects": projects })))
    }

    async fn require_doc(&self, project: &Project, name: &str) -> anyhow::Result<Doc> {
        assert_doc_name(name)?;
        if let Some(doc) = self.store.get_doc(project.id, name).await? {
            return Ok(doc);
        }
        // Listing what does exist is the cheapest defence against notes.md /
        // notes2.md multiplying (D5).
        let docs: Vec<String> = self.store.list_docs(project.id).await?.into_iter().map(|d| d.name).collect();
        Err(memory_error(
            "doc_not_found",
            format!("No document \"{name}\" in project \"{}\".", project.slug),
            json!({ "project": project.slug, "docs": docs }),
        ))
    }

    // The document is the truth; searchability may lag (D4).
    async fn reindex_doc(&self, project: &Project, doc: &Doc) {
        if let Err(err) = self.indexer.index_doc(project, doc).await {
            tracing::error!(project = project.slug, doc = doc.name, error = %format!("{err:#}"), "memory indexing failed");
        }
    }

    async fn index_fact_quietly(&self, fact: &Fact) {
        if let Err(err) = self.indexer.index_fact(fact).await {
            tracing::error!(fact = fact.id, error = %format!("{err:#}"), "memory fact indexing failed");
        }
    }

    // The shared tail of every mutating op: swap the body under a version
    // check, record the patch, re-index. None when the CAS lost.
    async fn commit(&self, project: &Project, doc: &Doc, change: Change<'_>) -> anyhow::Result<Option<WriteResult>> {
        let Some(updated) = self.store.update_doc(doc.id, doc.version, change.body, change.summary).await? else {
            return Ok(None);
        };
        let patch = self
            .store
            .insert_patch(NewPatch {
                pid: (self.new_patch_id)(),
                doc_id: doc.id,
                kind: change.kind,
                edits: change.edits,
                body_before: doc.body.clone(),
                version_before: doc.version,
                version_after: updated.version,
                actor: change.actor.to_owned(),
                rationale: change.rationale,
            })
            .await?;
        self.reindex_doc(project, &updated).await;
        Ok(Some(write_result(project, &updated, patch.pid)))
    }

    pub async fn create_project(&self, slug: &str, title: &str) -> anyhow::Result<Project> {
        assert_project_slug(slug)?;
        if let Some(existing) = self.store.get_project(slug).await? {
            return Err(memory_error(
                "project_exists",
                format!("Project \"{slug}\" already exists."),
                json!({ "project": existing.slug, "title": existing.title }),
            ));
        }
        let title = if title.trim().is_empty() { slug } else { title.trim() };
        self.store.create_project(slug, title).await
    }

    pub async fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
        self.store.list_projects().await
    }

    pub async fn list_docs(&self, slug: &str) -> anyhow::Result<DocList> {
        let project = self.require_project(slug).await?;
        let docs = self.store.list_docs(project.id).await?;
        Ok(DocList { project, docs })
    }

    pub async fn read_doc(&self, slug: &str, name: &str) -> anyhow::Result<Doc> {
        let project = self.require_project(slug).await?;
        self.require_doc(&project, name).await
    }

    pub async fn write_doc(&self, input: WriteDoc<'_>) -> anyhow::Result<WriteResult> {
        let project = self.require_project(input.project).await?;
        assert_doc_name(input.doc)?;
        let Some(existing) = self.store.get_doc(project.id, input.doc).await? else {
            if input.expected_version.is_some_and(|v| v != 0) {
                return Err(memory_error(
                    "version_conflict",
                    format!("Document \"{}\" does not exist yet; expected_version must be omitted or 0.", input.doc),
                    json!({ "project": project.slug, "doc": input.doc, "currentVersion": 0 }),
                ));
            }
            let created = self.store.create_doc(project.id, input.doc, input.summary, input.body).await?;
            let patch = self
                .store
                .insert_patch(NewPatch {
                    pid: (self.new_patch_id)(),
                    doc_id: created.id,
                    kind: PatchKind::Write,
                    edits: Vec::new(),
                    body_before: String::new(),
                    version_before: 0,
                    version_after: created.version,
                    actor: input.actor.to_owned(),
                    rationale: input.rationale.map(str::to_owned),
                })
                .await?;
            self.reindex_doc(&project, &created).await;
            return Ok(write_result(&project, &created, patch.pid));
        };

        // write_doc is the only op that can lose content: never blind.
        let Some(expected) = input.expected_version else {
            return Err(memory_error(
                "version_required",
                format!(
                    "Document \"{}\" exists at version {}; read it and pass expected_version.",
                    input.doc, existing.version
                ),
                json!({ "project": project.slug, "doc": input.doc, "currentVersion": existing.version }),
            ));
        };
        if expected != existing.version {
            return Err(version_conflict(&project.slug, input.doc, existing.version, expected));
        }
        let committed = self
            .commit(
                &project,
                &existing,
                Change {
                    body: input.body,
                    summary: input.summary.map(Some),
                    kind: PatchKind::Write,
                    edits: Vec::new(),
                    actor: input.actor,
                    rationale: input.rationale.map(str::to_owned),
                },
            )
            .await?;
        committed.ok_or_else(|| raced(&project.slug, input.doc))
    }

    pub async fn append_doc(
        &self,
        slug: &str,
        name: &str,
        text: &str,
        under_heading: Option<&str>,
        actor: &str,
        rationale: Option<&str>,
    ) -> anyhow::Result<WriteResult> {
        let project = self.require_project(slug).await?;
        for _ in 0..APPEND_CAS_ATTEMPTS {
            let doc = self.require_doc(&project, name).await?;
            let body = append_to_body(&doc.body, text, under_heading).map_err(|headings| {
                memory_error(
                    "heading_not_found",
                    format!("No heading \"{}\" in \"{name}\".", under_heading.unwrap_or_default()),
                    json!({ "project": project.slug, "doc": name, "headings": headings }),
                )
            })?;
            if let Some(result) = self
                .commit(
                    &project,
                    &doc,
                    Change {
                        body: &body,
                        summary: None,
                        kind: PatchKind::Append,
                        edits: Vec::new(),
                        actor,
                        rationale: rationale.map(str::to_owned),
                    },
                )
                .await?
            {
                return Ok(result);
            }
        }
        Err(raced(&project.slug, name))
    }

    pub async fn patch_doc(
        &self,
        slug: &str,
        name: &str,
        expected_version: i32,
        edits: Vec<Edit>,
        actor: &str,
        rationale: Option<&str>,
    ) -> anyhow::Result<WriteResult> {
        let project = self.require_project(slug).await?;
        let doc = self.require_doc(&project, name).await?;
        if doc.version != expected_version {
            return Err(version_conflict(&project.slug, name, doc.version, expected_version));
        }
        let body = apply_edits(&doc.body, &edits).map_err(|failures| edit_failure(&project.slug, name, failures))?;
        let committed = self
            .commit(
                &project,
                &doc,
                Change {
                    body: &body,
                    summary: None,
                    kind: PatchKind::Patch,
                    edits,
                    actor,
                    rationale: rationale.map(str::to_owned),
                },
            )
            .await?;
        committed.ok_or_else(|| raced(&project.slug, name))
    }

    pub async fn history(&self, slug: &str, name: &str, limit: i64) -> anyhow::Result<Vec<HistoryEntry>> {
        let project = self.require_project(slug).await?;
        let doc = self.require_doc(&project, name).await?;
        Ok(self
            .store
            .list_patches(doc.id, limit)
            .await?
            .into_iter()
            .map(|p| HistoryEntry {
                patch_id: p.pid,
                kind: p.kind,
                actor: p.actor,
                rationale: p.rationale,
                version_before: p.version_before,
                version_after: p.version_after,
                edit_count: p.edits.len(),
                created_at: p.created_at,
            })
            .collect())
    }

    pub async fn revert(
        &self,
        slug: &str,
        name: &str,
        patch_id: &str,
        rollback: bool,
        actor: &str,
    ) -> anyhow::Result<WriteResult> {
        let project = self.require_project(slug).await?;
        let doc = self.require_doc(&project, name).await?;
        let Some(patch) = self.store.get_patch(doc.id, patch_id).await? else {
            // The model will hallucinate ids: a hard error listing real ones,
            // never a fuzzy match (D9).
            let known: Vec<String> = self.store.list_patches(doc.id, 10).await?.into_iter().map(|p| p.pid).collect();
            return Err(memory_error(
                "patch_not_found",
                format!("No patch \"{patch_id}\" on \"{name}\"."),
                json!({ "project": project.slug, "doc": name, "knownPatchIds": known }),
            ));
        };
        let is_newest = self.store.list_patches(doc.id, 1).await?.first().is_some_and(|newest| newest.pid == patch.pid);

        let body = if rollback || is_newest {
            // Exact by construction: every patch stored the body it replaced.
            patch.body_before.clone()
        } else {
            let inverse = if patch.edits.is_empty() { None } else { invert_edits(&patch.edits) };
            let Some(inverse) = inverse else { return Err(revert_conflict(&project.slug, name, &patch, Vec::new())) };
            apply_edits(&doc.body, &inverse)
                .map_err(|failures| revert_conflict(&project.slug, name, &patch, failures))?
        };
        let rationale = if rollback && !is_newest {
            format!("rollback to v{} (discards patches after {})", patch.version_before, patch.pid)
        } else {
            format!("revert {}", patch.pid)
        };
        let committed = self
            .commit(
                &project,
                &doc,
                Change {
                    body: &body,
                    summary: None,
                    kind: PatchKind::Revert,
                    edits: Vec::new(),
                    actor,
                    rationale: Some(rationale),
                },
            )
            .await?;
        committed.ok_or_else(|| raced(&project.slug, name))
    }

    pub async fn remember(&self, body: &str, tags: Option<&[String]>, source: Option<&str>) -> anyhow::Result<Fact> {
        let body = body.trim();
        if body.is_empty() {
            return Err(memory_error("empty_body", "A fact needs a body.", json!({})));
        }
        let fact = self.store.create_fact(body, &normalize_tags(tags), source).await?;
        self.index_fact_quietly(&fact).await;
        Ok(fact)
    }

    pub async fn get_fact(&self, id: i64) -> anyhow::Result<Fact> {
        self.store
            .get_fact(id)
            .await?
            .ok_or_else(|| memory_error("fact_not_found", format!("No fact {id}."), json!({ "id": id })))
    }

    pub async fn update_fact(
        &self,
        id: i64,
        body: Option<&str>,
        tags: Option<&[String]>,
        state: Option<MemoryState>,
    ) -> anyhow::Result<Fact> {
        let update = FactUpdate { body: body.map(str::trim), tags: tags.map(|t| normalize_tags(Some(t))), state };
        let updated = self
            .store
            .update_fact(id, update)
            .await?
            .ok_or_else(|| memory_error("fact_not_found", format!("No fact {id}."), json!({ "id": id })))?;
        self.index_fact_quietly(&updated).await;
        Ok(updated)
    }

    pub async fn recall(
        &self,
        query: &str,
        limit: Option<i64>,
        states: Option<&[MemoryState]>,
        tags: Option<&[String]>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Vec<RecallHit>> {
        let limit = limit.unwrap_or(10).clamp(1, 50);
        let Some(embedding) = self.indexer.embed_query(query).await else {
            // "Search is down" and "we remember nothing" must not look alike:
            // one is worth retrying.
            return Err(memory_error(
                "search_unavailable",
                "Recall needs the embedding provider, which is currently unreachable. Documents still read and patch.",
                json!({}),
            ));
        };
        // Over-fetch so the recency boost has something to reorder.
        let hits = self
            .store
            .search_index(&embedding, limit * 3, states.unwrap_or(&[MemoryState::Active]), tags.unwrap_or_default())
            .await?;
        Ok(rank_hits(hits, now, DEFAULT_RANK)
            .into_iter()
            .take(limit as usize)
            .map(|(hit, score)| RecallHit {
                r#ref: hit.r#ref,
                text: hit.text,
                score,
                distance: hit.distance,
                state: hit.state,
                actor: hit.actor,
                ts: hit.ts,
            })
            .collect())
    }
}

fn write_result(project: &Project, doc: &Doc, patch_id: String) -> WriteResult {
    WriteResult {
        project: project.slug.clone(),
        doc: doc.name.clone(),
        version: doc.version,
        patch_id,
        size_bytes: doc.size_bytes,
    }
}

fn version_conflict(project: &str, doc: &str, current: i32, expected: i32) -> anyhow::Error {
    memory_error(
        "version_conflict",
        format!("Document \"{doc}\" is at version {current}, not {expected}. Re-read it and retry."),
        json!({ "project": project, "doc": doc, "currentVersion": current }),
    )
}

fn raced(project: &str, doc: &str) -> anyhow::Error {
    memory_error(
        "version_conflict",
        format!("Document \"{doc}\" changed while this write was in flight. Re-read it and retry."),
        json!({ "project": project, "doc": doc }),
    )
}

fn summarise_failures(failures: &[EditFailure]) -> String {
    failures
        .iter()
        .map(|f| match f.reason {
            EditFailureReason::Empty => format!("edit {}: `old` is empty; use append_doc to add text", f.index),
            EditFailureReason::Ambiguous => {
                format!("edit {}: `old` matches {} times; quote more context", f.index, f.occurrences)
            }
            EditFailureReason::NotFound => {
                let hint = f
                    .suggestions
                    .first()
                    .map(|s| format!("; did you mean: {}", serde_json::to_string(s).unwrap_or_default()))
                    .unwrap_or_default();
                format!("edit {}: `old` not found{hint}", f.index)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn edit_failure(project: &str, doc: &str, failures: Vec<EditFailure>) -> anyhow::Error {
    // Nothing was written: the document is exactly as it was.
    memory_error(
        "edit_failed",
        summarise_failures(&failures),
        json!({ "project": project, "doc": doc, "applied": false, "failures": failures }),
    )
}

fn revert_conflict(project: &str, doc: &str, patch: &DocPatch, failures: Vec<EditFailure>) -> anyhow::Error {
    memory_error(
        "revert_conflict",
        format!(
            "Patch {} cannot be undone in place because later patches touched the same text. \
             Retry with rollback=true to restore version {}, discarding everything written after it.",
            patch.pid, patch.version_before
        ),
        json!({ "project": project, "doc": doc, "patchId": patch.pid, "rollbackToVersion": patch.version_before, "failures": failures }),
    )
}

// ── 8. import ────────────────────────────────────────────────────────────────

pub struct LegacyNote {
    pub id: i64,
    pub body: String,
    pub tags: Vec<String>,
}

pub fn legacy_note_source(note_id: i64) -> String {
    format!("knowledge_base_notes:{note_id}")
}

// A copy, not a transformation: the original phrasing is what a later
// structuring pass needs. Provenance in `source` makes it re-runnable.
pub async fn import_legacy_notes(notes: &[LegacyNote], service: &MemoryService) -> anyhow::Result<(usize, usize)> {
    let (mut imported, mut skipped) = (0, 0);
    for note in notes {
        let provenance = legacy_note_source(note.id);
        let body = note.body.trim();
        if service.store.get_fact_by_source(&provenance).await?.is_some() || body.is_empty() {
            skipped += 1;
            continue;
        }
        let fact = service.store.create_fact(body, &note.tags, Some(&provenance)).await?;
        service.index_fact_quietly(&fact).await;
        imported += 1;
    }
    Ok((imported, skipped))
}

// ── 9. tools ─────────────────────────────────────────────────────────────────

// A MemoryError becomes `{ ok: false, error, message, …details }` — a result
// the model can act on. Anything else (a malformed slug, a DB outage) stays a
// tool failure. Payloads are spread flat into `{ ok: true, … }`.
fn run<T: Serialize>(result: anyhow::Result<T>) -> ToolResult {
    match result {
        Ok(value) => {
            let mut out = Map::new();
            out.insert("ok".into(), true.into());
            match serde_json::to_value(value) {
                Ok(Value::Object(fields)) => out.extend(fields),
                Ok(other) => {
                    out.insert("value".into(), other);
                }
                Err(err) => return tool_failed(err),
            }
            json_result(&Value::Object(out))
        }
        Err(err) => match err.downcast::<MemoryError>() {
            Ok(e) => {
                let mut out = Map::new();
                out.insert("ok".into(), false.into());
                out.insert("error".into(), e.code.into());
                out.insert("message".into(), e.message.into());
                out.extend(e.details);
                json_result(&Value::Object(out))
            }
            Err(err) => tool_failed(format!("{err:#}")),
        },
    }
}

fn nonempty_tags(tags: Option<&Vec<String>>, max: usize) -> bool {
    tags.is_none_or(|t| t.len() <= max && t.iter().all(|x| !x.is_empty()))
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RecallParams {
    /// What to look for, in natural language.
    query: String,
    /// Max hits (default 10).
    #[schemars(range(min = 1, max = 50))]
    limit: Option<i64>,
    /// Keep only hits carrying one of these tags.
    tags: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RememberParams {
    /// The fact, as a self-contained sentence including its subject.
    body: String,
    /// 3–6 short lowercase topical tags.
    tags: Option<Vec<String>>,
    /// Optional provenance, e.g. "telegram".
    source: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GetFactParams {
    /// Fact id, the number in a fact:<id> ref.
    #[schemars(range(min = 1))]
    id: i64,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateFactParams {
    #[schemars(range(min = 1))]
    id: i64,
    /// Replacement text.
    body: Option<String>,
    /// Replacement tags (not merged).
    tags: Option<Vec<String>>,
    /// active = current, done = finished, archived = out of the way.
    state: Option<MemoryState>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListMemoryParams {
    /// Project slug. Omit to list all projects.
    project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateProjectParams {
    /// Lowercase id, hyphens only, e.g. "leetcode-graphs".
    slug: String,
    /// Human-readable name.
    title: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadDocParams {
    project: String,
    /// Document filename, e.g. "roadmap.md".
    doc: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AppendDocParams {
    project: String,
    doc: String,
    /// Markdown to append. One blank line is inserted before it.
    text: String,
    /// Append at the end of this section instead of the file, e.g. "Прогресс".
    under_heading: Option<String>,
    /// The user's own words that prompted this write.
    rationale: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PatchDocParams {
    project: String,
    doc: String,
    /// Version from the read_doc you just did.
    #[schemars(range(min = 1))]
    expected_version: i32,
    edits: Vec<Edit>,
    /// The user's own words that prompted this change.
    rationale: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WriteDocParams {
    project: String,
    /// Filename, e.g. "roadmap.md".
    doc: String,
    /// Full markdown content.
    body: String,
    /// One line describing what this document is for.
    summary: Option<String>,
    /// Required when the document already exists. Omit when creating.
    #[schemars(range(min = 0))]
    expected_version: Option<i32>,
    rationale: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DocHistoryParams {
    project: String,
    doc: String,
    /// Default 20.
    #[schemars(range(min = 1, max = 100))]
    limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RevertParams {
    project: String,
    doc: String,
    /// Patch id from doc_history, e.g. pa:4f2a1c.
    patch_id: String,
    /// Discard everything after this patch and restore the document as it was before it.
    rollback: Option<bool>,
}

#[tool_router(router = memory_tools, vis = "pub(crate)")]
impl McpTools {
    // Public recall is deliberately active-only: letting the model pick
    // lifecycle states made ordinary queries opt into archived facts and
    // defeated archive as a discovery boundary.
    #[tool(
        name = "recall",
        title = "Search everything the agents remember",
        description = "Semantic search across ALL shared memory — project documents and \
            standalone facts alike. Use it whenever the user refers to something \
            from the past (\"что там у нас было по X\", \"напомни про Y\") and \
            before starting work that might already have a project. Archived facts \
            are intentionally excluded from this ordinary recall path.\n\n\
            Returns REFS, not full content: `doc:<project>/<file>#<chunk>` or \
            `fact:<id>`. Follow the interesting ones with read_doc / get_fact — \
            the snippet is for choosing, the document is for answering."
    )]
    async fn recall(&self, Parameters(p): Parameters<RecallParams>) -> ToolResult {
        if p.query.is_empty()
            || p.limit.is_some_and(|l| !(1..=50).contains(&l))
            || !nonempty_tags(p.tags.as_ref(), usize::MAX)
        {
            return Err(invalid_params("query must be non-empty, limit 1–50, tags non-empty"));
        }
        let memory = &self.deps.memory;
        run(memory
            .recall(&p.query, p.limit, None, p.tags.as_deref(), Utc::now())
            .await
            .map(|hits| json!({ "hits": hits })))
    }

    #[tool(
        name = "remember",
        title = "Store a standalone fact",
        description = "Persist a freeform fact the user asked you to remember (\"запомни, что …\"). \
            For anything with ongoing progress use a project document instead — a \
            fact is a single self-contained statement, not a running log.\n\n\
            YOU generate the tags: 3–6 short lowercase topical words. Recall runs \
            over the TEXT, so write a body that names its own subject (\"Лёша \
            платит за интернет 1-го числа\", not \"платит 1-го\")."
    )]
    async fn remember(&self, Parameters(p): Parameters<RememberParams>) -> ToolResult {
        if p.body.is_empty() || !nonempty_tags(p.tags.as_ref(), 12) {
            return Err(invalid_params("body must be non-empty; at most 12 non-empty tags"));
        }
        let memory = &self.deps.memory;
        run(memory.remember(&p.body, p.tags.as_deref(), p.source.as_deref()).await.map(|fact| json!({ "fact": fact })))
    }

    #[tool(
        name = "get_fact",
        title = "Read one fact in full",
        description = "Load a fact by id — the read half of a `fact:<id>` ref returned by recall."
    )]
    async fn get_fact(&self, Parameters(p): Parameters<GetFactParams>) -> ToolResult {
        if p.id < 1 {
            return Err(invalid_params("id must be a positive integer"));
        }
        let memory = &self.deps.memory;
        run(memory.get_fact(p.id).await.map(|fact| json!({ "fact": fact })))
    }

    #[tool(
        name = "update_fact",
        title = "Correct a fact or retire it",
        description = "Change a fact's text, tags or lifecycle state. Nothing is ever deleted: \
            mark a finished or obsolete item `done` / `archived` and it drops out of \
            the default recall while staying findable on request."
    )]
    async fn update_fact(&self, Parameters(p): Parameters<UpdateFactParams>) -> ToolResult {
        if p.id < 1 || p.body.as_deref().is_some_and(str::is_empty) || !nonempty_tags(p.tags.as_ref(), 12) {
            return Err(invalid_params("id must be positive; body non-empty; at most 12 non-empty tags"));
        }
        let memory = &self.deps.memory;
        run(memory
            .update_fact(p.id, p.body.as_deref(), p.tags.as_deref(), p.state)
            .await
            .map(|fact| json!({ "fact": fact })))
    }

    #[tool(
        name = "list_memory",
        title = "List projects, or the documents in one",
        description = "Without `project`: every project. With it: that project's documents, \
            each with a one-line summary, version and size.\n\n\
            ALWAYS call this before creating a document. It is what keeps a \
            project from growing notes.md, notes2.md and progress-new.md until \
            nobody knows which one is current."
    )]
    async fn list_memory(&self, Parameters(p): Parameters<ListMemoryParams>) -> ToolResult {
        let memory = &self.deps.memory;
        match p.project {
            None => run(memory.list_projects().await.map(|projects| json!({ "projects": projects }))),
            Some(project) => run(memory.list_docs(&project).await),
        }
    }

    #[tool(
        name = "create_project",
        title = "Start a new project",
        description = "Create an empty project — a folder of markdown documents with progress, \
            e.g. preparing for an interview on a topic. Check list_memory first; \
            reusing an existing project is almost always right."
    )]
    async fn create_project(&self, Parameters(p): Parameters<CreateProjectParams>) -> ToolResult {
        if p.slug.is_empty() || p.title.is_empty() {
            return Err(invalid_params("slug and title must be non-empty"));
        }
        let memory = &self.deps.memory;
        run(memory.create_project(&p.slug, &p.title).await.map(|project| json!({ "project": project })))
    }

    #[tool(
        name = "read_doc",
        title = "Read a project document",
        description = "Return a document's full markdown plus its `version`. You need that \
            version to patch it, and the text has to be fresh or your quoted \
            `old` strings won't match. Read immediately before writing."
    )]
    async fn read_doc(&self, Parameters(p): Parameters<ReadDocParams>) -> ToolResult {
        let memory = &self.deps.memory;
        run(memory.read_doc(&p.project, &p.doc).await.map(|doc| json!({ "doc": doc })))
    }

    #[tool(
        name = "append_doc",
        title = "Add text to the end of a document (safe default)",
        description = "Append to a document, or to one section of it. THE PREFERRED WRITE: it \
            cannot destroy existing text and needs no prior read or version.\n\n\
            Use it for anything that is a record of what happened — progress \
            entries, notes, mistakes. Progress is history: correct an old entry by \
            appending a correction, never by editing the past."
    )]
    async fn append_doc(&self, Parameters(p): Parameters<AppendDocParams>) -> ToolResult {
        if p.text.is_empty() {
            return Err(invalid_params("text must be non-empty"));
        }
        let memory = &self.deps.memory;
        let actor = &self.deps.memory_actor;
        run(memory
            .append_doc(&p.project, &p.doc, &p.text, p.under_heading.as_deref(), actor, p.rationale.as_deref())
            .await)
    }

    #[tool(
        name = "patch_doc",
        title = "Change existing text in a document",
        description = "Apply search/replace edits. Each `old` must appear EXACTLY ONCE — copy \
            it verbatim from a fresh read_doc, including punctuation, dashes and \
            ё. `new: \"\"` deletes. All edits apply or none do.\n\n\
            Failures come back with what you need to fix them: `version_conflict` \
            gives the current version, an ambiguous quote gives the match count, \
            and a miss gives the literal from the document that nearly matched."
    )]
    async fn patch_doc(&self, Parameters(p): Parameters<PatchDocParams>) -> ToolResult {
        if p.expected_version < 1 || p.edits.is_empty() || p.edits.iter().any(|e| e.old.is_empty()) {
            return Err(invalid_params("expected_version must be ≥1; edits non-empty, each with a non-empty `old`"));
        }
        let memory = &self.deps.memory;
        let actor = &self.deps.memory_actor;
        run(memory.patch_doc(&p.project, &p.doc, p.expected_version, p.edits, actor, p.rationale.as_deref()).await)
    }

    #[tool(
        name = "write_doc",
        title = "Create a document, or replace one wholesale",
        description = "Create a new document, or overwrite an existing one entirely. THE ONLY \
            OP THAT CAN LOSE CONTENT — prefer append_doc for additions and \
            patch_doc for changes; use this to create, or when the user explicitly \
            asks to rewrite from scratch.\n\n\
            Creating: omit expected_version. Overwriting: read_doc first and pass \
            its version. Always give a `summary` — it is the line other agents see \
            in list_memory."
    )]
    async fn write_doc(&self, Parameters(p): Parameters<WriteDocParams>) -> ToolResult {
        if p.expected_version.is_some_and(|v| v < 0) {
            return Err(invalid_params("expected_version must be ≥0"));
        }
        let memory = &self.deps.memory;
        run(memory
            .write_doc(WriteDoc {
                project: &p.project,
                doc: &p.doc,
                body: &p.body,
                summary: p.summary.as_deref(),
                expected_version: p.expected_version,
                actor: &self.deps.memory_actor,
                rationale: p.rationale.as_deref(),
            })
            .await)
    }

    #[tool(
        name = "doc_history",
        title = "Who changed a document, when and why",
        description = "List a document's patches, newest first: patch id, kind, actor, the \
            rationale recorded at the time, and the versions it moved between. \
            Patch ids come from here — never invent one."
    )]
    async fn doc_history(&self, Parameters(p): Parameters<DocHistoryParams>) -> ToolResult {
        if p.limit.is_some_and(|l| !(1..=100).contains(&l)) {
            return Err(invalid_params("limit must be 1–100"));
        }
        let memory = &self.deps.memory;
        run(memory
            .history(&p.project, &p.doc, p.limit.unwrap_or(20))
            .await
            .map(|history| json!({ "history": history })))
    }

    #[tool(
        name = "revert_patch",
        title = "Undo a recorded change",
        description = "Undo one patch from doc_history. The newest patch always reverts \
            exactly. An older one is undone in place when later patches left its \
            text alone; when they didn't, the call fails and tells you which \
            version a rollback would restore.\n\n\
            `rollback: true` then restores the whole document to the state before \
            that patch, DISCARDING everything written after it — only do that when \
            the user asked for it. Reverting is itself recorded; history is never \
            rewritten."
    )]
    async fn revert_patch(&self, Parameters(p): Parameters<RevertParams>) -> ToolResult {
        if p.patch_id.is_empty() {
            return Err(invalid_params("patch_id must be non-empty"));
        }
        let memory = &self.deps.memory;
        run(memory.revert(&p.project, &p.doc, &p.patch_id, p.rollback == Some(true), &self.deps.memory_actor).await)
    }
}

// ── tests: an in-memory store, literal enough that passing here means the
// same thing against Postgres ────────────────────────────────────────────────

#[cfg(test)]
pub mod testing {
    use std::sync::Mutex;

    use futures::future::BoxFuture;

    use super::*;

    #[derive(Default)]
    struct State {
        seq: i64,
        projects: Vec<Project>,
        docs: Vec<Doc>,
        patches: Vec<DocPatch>,
        facts: Vec<Fact>,
        index: Vec<(i64, IndexUpsert)>,
    }

    pub type Hook = Box<dyn Fn(i64) -> BoxFuture<'static, ()> + Send + Sync>;

    #[derive(Default)]
    pub struct InMemoryStore {
        state: Mutex<State>,
        // Fires just before an update_doc CAS: simulates another agent
        // committing between this one's read and write.
        pub before_update: Mutex<Option<Hook>>,
    }

    impl InMemoryStore {
        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap()
        }
    }

    fn next(state: &mut State) -> i64 {
        state.seq += 1;
        state.seq
    }

    fn cosine(a: &[f32], b: &[f32]) -> f64 {
        let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum();
        let na: f64 = a.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        let nb: f64 = b.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        if na == 0.0 || nb == 0.0 { 1.0 } else { 1.0 - dot / (na * nb) }
    }

    #[async_trait]
    impl MemoryStore for InMemoryStore {
        async fn create_project(&self, slug: &str, title: &str) -> anyhow::Result<Project> {
            let mut s = self.lock();
            anyhow::ensure!(!s.projects.iter().any(|p| p.slug == slug), "project {slug} already exists");
            let now = Utc::now();
            let project =
                Project { id: next(&mut s), slug: slug.into(), title: title.into(), created_at: now, updated_at: now };
            s.projects.push(project.clone());
            Ok(project)
        }

        async fn get_project(&self, slug: &str) -> anyhow::Result<Option<Project>> {
            Ok(self.lock().projects.iter().find(|p| p.slug == slug).cloned())
        }

        async fn list_projects(&self) -> anyhow::Result<Vec<Project>> {
            let mut out = self.lock().projects.clone();
            out.sort_by(|a, b| a.slug.cmp(&b.slug));
            Ok(out)
        }

        async fn list_docs(&self, project_id: i64) -> anyhow::Result<Vec<DocSummary>> {
            let mut out: Vec<DocSummary> =
                self.lock().docs.iter().filter(|d| d.project_id == project_id).map(Doc::summary_view).collect();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        }

        async fn get_doc(&self, project_id: i64, name: &str) -> anyhow::Result<Option<Doc>> {
            Ok(self.lock().docs.iter().find(|d| d.project_id == project_id && d.name == name).cloned())
        }

        async fn create_doc(
            &self,
            project_id: i64,
            name: &str,
            summary: Option<&str>,
            body: &str,
        ) -> anyhow::Result<Doc> {
            let mut s = self.lock();
            anyhow::ensure!(
                !s.docs.iter().any(|d| d.project_id == project_id && d.name == name),
                "document {name} already exists"
            );
            let doc = Doc {
                id: next(&mut s),
                project_id,
                name: name.into(),
                summary: summary.map(str::to_owned),
                body: body.into(),
                version: 1,
                size_bytes: body.len(),
                updated_at: Utc::now(),
            };
            s.docs.push(doc.clone());
            Ok(doc)
        }

        async fn update_doc(
            &self,
            doc_id: i64,
            expected_version: i32,
            body: &str,
            summary: Option<Option<&str>>,
        ) -> anyhow::Result<Option<Doc>> {
            let hook = self.before_update.lock().unwrap().as_ref().map(|h| h(doc_id));
            if let Some(hook) = hook {
                hook.await;
            }
            let mut s = self.lock();
            let Some(doc) = s.docs.iter_mut().find(|d| d.id == doc_id) else { return Ok(None) };
            if doc.version != expected_version {
                return Ok(None);
            }
            doc.body = body.into();
            if let Some(summary) = summary {
                doc.summary = summary.map(str::to_owned);
            }
            doc.version += 1;
            doc.size_bytes = doc.body.len();
            doc.updated_at = Utc::now();
            Ok(Some(doc.clone()))
        }

        async fn insert_patch(&self, p: NewPatch) -> anyhow::Result<DocPatch> {
            let patch = DocPatch {
                pid: p.pid,
                doc_id: p.doc_id,
                kind: p.kind,
                edits: p.edits,
                body_before: p.body_before,
                version_before: p.version_before,
                version_after: p.version_after,
                actor: p.actor,
                rationale: p.rationale,
                created_at: Utc::now(),
            };
            self.lock().patches.push(patch.clone());
            Ok(patch)
        }

        async fn list_patches(&self, doc_id: i64, limit: i64) -> anyhow::Result<Vec<DocPatch>> {
            Ok(self.lock().patches.iter().rev().filter(|p| p.doc_id == doc_id).take(limit as usize).cloned().collect())
        }

        async fn get_patch(&self, doc_id: i64, pid: &str) -> anyhow::Result<Option<DocPatch>> {
            Ok(self.lock().patches.iter().find(|p| p.doc_id == doc_id && p.pid == pid).cloned())
        }

        async fn create_fact(&self, body: &str, tags: &[String], source: Option<&str>) -> anyhow::Result<Fact> {
            let mut s = self.lock();
            let now = Utc::now();
            let fact = Fact {
                id: next(&mut s),
                body: body.into(),
                tags: tags.to_vec(),
                source: source.map(str::to_owned),
                state: MemoryState::Active,
                created_at: now,
                updated_at: now,
            };
            s.facts.push(fact.clone());
            Ok(fact)
        }

        async fn get_fact(&self, id: i64) -> anyhow::Result<Option<Fact>> {
            Ok(self.lock().facts.iter().find(|f| f.id == id).cloned())
        }

        async fn get_fact_by_source(&self, source: &str) -> anyhow::Result<Option<Fact>> {
            Ok(self.lock().facts.iter().find(|f| f.source.as_deref() == Some(source)).cloned())
        }

        async fn update_fact(&self, id: i64, update: FactUpdate<'_>) -> anyhow::Result<Option<Fact>> {
            let mut s = self.lock();
            let Some(fact) = s.facts.iter_mut().find(|f| f.id == id) else { return Ok(None) };
            if let Some(body) = update.body {
                fact.body = body.into();
            }
            if let Some(tags) = update.tags {
                fact.tags = tags;
            }
            if let Some(state) = update.state {
                fact.state = state;
            }
            fact.updated_at = Utc::now();
            Ok(Some(fact.clone()))
        }

        async fn replace_index(&self, source_ref: &str, entries: Vec<IndexUpsert>) -> anyhow::Result<()> {
            let mut s = self.lock();
            s.index.retain(|(_, e)| e.source_ref != source_ref);
            for entry in entries {
                let id = next(&mut s);
                s.index.push((id, entry));
            }
            Ok(())
        }

        async fn search_index(
            &self,
            embedding: &[f32],
            limit: i64,
            states: &[MemoryState],
            tags: &[String],
        ) -> anyhow::Result<Vec<IndexHit>> {
            let wanted: Vec<&String> = tags.iter().filter(|t| !t.is_empty()).collect();
            let mut hits: Vec<IndexHit> = self
                .lock()
                .index
                .iter()
                .filter(|(_, e)| e.embedding.is_some() && states.contains(&e.state))
                .filter(|(_, e)| wanted.is_empty() || e.tags.iter().any(|t| wanted.contains(&t)))
                .map(|(id, e)| IndexHit {
                    id: *id,
                    r#ref: e.r#ref.clone(),
                    text: e.text.clone(),
                    tags: e.tags.clone(),
                    actor: e.actor.clone(),
                    state: e.state,
                    ts: e.ts,
                    distance: cosine(e.embedding.as_deref().unwrap_or_default(), embedding),
                })
                .collect();
            hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
            hits.truncate(limit as usize);
            Ok(hits)
        }

        async fn list_unembedded(&self, limit: i64) -> anyhow::Result<Vec<(i64, String)>> {
            Ok(self
                .lock()
                .index
                .iter()
                .filter(|(_, e)| e.embedding.is_none())
                .take(limit as usize)
                .map(|(id, e)| (*id, e.text.clone()))
                .collect())
        }

        async fn set_embedding(&self, id: i64, embedding: &[f32]) -> anyhow::Result<()> {
            if let Some((_, e)) = self.lock().index.iter_mut().find(|(i, _)| *i == id) {
                e.embedding = Some(embedding.to_vec());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::testing::InMemoryStore;
    use super::*;
    use crate::embeddings::testing::FakeEmbedder;

    // ── patch ────────────────────────────────────────────────────────────────

    const ROADMAP: &str = "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n- [ ] A*\n";

    fn edit(old: &str, new: &str) -> Edit {
        Edit { old: old.into(), new: new.into() }
    }

    #[test]
    fn replaces_a_unique_literal_and_deletes_with_empty_new() {
        assert_eq!(
            apply_edits(ROADMAP, &[edit("- [ ] Dijkstra", "- [x] Dijkstra")]).unwrap(),
            "# Roadmap\n\n- [ ] BFS\n- [x] Dijkstra\n- [ ] A*\n"
        );
        assert_eq!(
            apply_edits(ROADMAP, &[edit("- [ ] A*\n", "")]).unwrap(),
            "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n"
        );
    }

    #[test]
    fn refuses_ambiguous_empty_and_partial_edit_lists() {
        let failures =
            apply_edits("- [ ] review\n- [ ] review\n- [ ] review\n", &[edit("- [ ] review", "x")]).unwrap_err();
        assert_eq!(
            failures,
            vec![EditFailure {
                index: 0,
                old: "- [ ] review".into(),
                reason: EditFailureReason::Ambiguous,
                occurrences: 3,
                suggestions: vec![]
            }]
        );
        assert_eq!(
            apply_edits(ROADMAP, &[edit("- [ ] BFS", "- [x] BFS"), edit("- [ ] Floyd", "x")]).unwrap_err()[0].index,
            1
        );
        assert_eq!(apply_edits(ROADMAP, &[edit("", "anything")]).unwrap_err()[0].reason, EditFailureReason::Empty);
        assert!(apply_edits(ROADMAP, &[]).is_err());
        let both = apply_edits(ROADMAP, &[edit("nope one", "x"), edit("nope two", "y")]).unwrap_err();
        assert_eq!(both.iter().map(|f| f.index).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn a_later_edit_may_target_text_an_earlier_one_produced() {
        let out = apply_edits(
            ROADMAP,
            &[edit("- [ ] A*", "- [ ] A*\n- [ ] Bellman-Ford"), edit("- [ ] Bellman-Ford", "- [x] Bellman-Ford")],
        );
        assert_eq!(out.unwrap(), "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n- [ ] A*\n- [x] Bellman-Ford\n");
    }

    #[test]
    fn near_matches_recover_normalised_quotes() {
        assert_eq!(
            find_near_matches("Цель — пройти графы за месяц.\n", "Цель - пройти графы за месяц.", 3),
            ["Цель — пройти графы за месяц."]
        );
        assert_eq!(
            find_near_matches("- [ ] Обойдём граф в ширину\n", "Обойдем граф в ширину", 3),
            ["Обойдём граф в ширину"]
        );
        assert_eq!(find_near_matches("Плана\n    нет\n", "Плана нет", 3), ["Плана\n    нет"]);
        assert_eq!(
            find_near_matches("Проект «Графы» стартовал.\n", "Проект \"Графы\" стартовал.", 3),
            ["Проект «Графы» стартовал."]
        );
        assert_eq!(
            find_near_matches(
                "# Roadmap\n\n- [ ] Dijkstra shortest path\n- [ ] Unrelated topic\n",
                "- [ ] Dijkstra shortest paths",
                3
            ),
            ["- [ ] Dijkstra shortest path"]
        );
        assert!(find_near_matches("# Roadmap\n\n- [ ] BFS\n", "completely unrelated sentence", 3).is_empty());
        let failure = &apply_edits("Цель — пройти графы.\n", &[edit("Цель - пройти графы.", "x")]).unwrap_err()[0];
        assert_eq!(
            (failure.reason, failure.suggestions.clone()),
            (EditFailureReason::NotFound, vec!["Цель — пройти графы.".to_owned()])
        );
    }

    #[test]
    fn inverts_edits_except_deletions() {
        assert_eq!(invert_edits(&[edit("a", "b"), edit("c", "d")]).unwrap(), [edit("d", "c"), edit("b", "a")]);
        assert_eq!(invert_edits(&[edit("gone", "")]), None);
    }

    #[test]
    fn appends_with_stable_spacing_and_under_headings() {
        assert_eq!(
            append_to_body("# Progress\n\nDay 1: BFS\n", "Day 2: Dijkstra", None).unwrap(),
            "# Progress\n\nDay 1: BFS\n\nDay 2: Dijkstra\n"
        );
        assert_eq!(append_to_body("# Progress\n\n\n\n", "Day 1", None).unwrap(), "# Progress\n\nDay 1\n");
        assert_eq!(append_to_body("", "first note", None).unwrap(), "first note\n");
        let body = "# Doc\n\n## Progress\n\nDay 1\n\n## Mistakes\n\nForgot visited set\n";
        assert_eq!(
            append_to_body(body, "Day 2", Some("Progress")).unwrap(),
            "# Doc\n\n## Progress\n\nDay 1\n\nDay 2\n\n## Mistakes\n\nForgot visited set\n"
        );
        let body = "## Progress\n\nDay 1\n\n### Notes\n\nn\n\n## Mistakes\n\nm\n";
        assert_eq!(
            append_to_body(body, "Day 2", Some("## Progress")).unwrap(),
            "## Progress\n\nDay 1\n\n### Notes\n\nn\n\nDay 2\n\n## Mistakes\n\nm\n"
        );
        assert_eq!(
            append_to_body("## Progress\n\nDay 1\n\n## Mistakes\n\nm\n", "x", Some("Roadmap")).unwrap_err(),
            ["## Progress", "## Mistakes"]
        );
    }

    #[test]
    fn lists_headings() {
        assert_eq!(
            list_headings("# A\n\ntext\n\n### B\n#not a heading\n"),
            vec![Heading { level: 1, text: "A".into(), line: 0 }, Heading { level: 3, text: "B".into(), line: 4 }]
        );
    }

    // ── projection ───────────────────────────────────────────────────────────

    fn chunk(text: &str, path: &str) -> MarkdownChunk {
        MarkdownChunk { text: text.into(), heading_path: path.into() }
    }

    #[test]
    fn chunks_at_headings_with_breadcrumbs() {
        let body = "# Project\n\nIntro line.\n\n## Progress\n\nDay 1\n\n### Notes\n\nWatch out\n";
        assert_eq!(
            chunk_markdown(body, DEFAULT_CHUNK_CHARS),
            vec![
                chunk("Intro line.", "Project"),
                chunk("Day 1", "Project > Progress"),
                chunk("Watch out", "Project > Progress > Notes")
            ]
        );
        let paths: Vec<String> = chunk_markdown("## A\n\na\n\n### A1\n\na1\n\n## B\n\nb\n", 1200)
            .into_iter()
            .map(|c| c.heading_path)
            .collect();
        assert_eq!(paths, ["A", "A > A1", "B"]);
    }

    #[test]
    fn packs_and_splits_paragraphs_by_budget() {
        assert_eq!(chunk_markdown("## Log\n\none\n\ntwo\n\nthree\n", 100), vec![chunk("one\n\ntwo\n\nthree", "Log")]);
        let body = ["## Log", &"a".repeat(60), &"b".repeat(60), &"c".repeat(60)].join("\n\n");
        let chunks = chunk_markdown(&body, 100);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.text.len() <= 100 && c.heading_path == "Log"));
        let lengths: Vec<usize> = chunk_markdown(&"x".repeat(250), 100).iter().map(|c| c.text.len()).collect();
        assert_eq!(lengths, [100, 100, 50]);
        assert_eq!(chunk_markdown("loose note\n", 1200), vec![chunk("loose note", "")]);
        assert!(chunk_markdown("", 1200).is_empty() && chunk_markdown("# Title\n", 1200).is_empty());
    }

    #[test]
    fn index_text_names_its_subject() {
        assert_eq!(
            build_index_text("Графы для интервью", "progress.md", "Прогресс", "застрял на Dijkstra"),
            "Графы для интервью — progress.md · Прогресс\n\nзастрял на Dijkstra"
        );
        assert_eq!(build_index_text("P", "d.md", "", "body"), "P — d.md\n\nbody");
    }

    #[test]
    fn recency_breaks_ties_but_never_beats_relevance() {
        let now = Utc::now();
        let hit = |id: i64, distance: f64, days_ago: f64| IndexHit {
            id,
            r#ref: format!("fact:{id}"),
            text: String::new(),
            tags: vec![],
            actor: None,
            state: MemoryState::Active,
            ts: now - chrono::Duration::milliseconds((days_ago * 86_400_000.0) as i64),
            distance,
        };
        let order = |hits| rank_hits(hits, now, DEFAULT_RANK).into_iter().map(|(h, _)| h.id).collect::<Vec<_>>();
        assert_eq!(order(vec![hit(1, 0.3, 400.0), hit(2, 0.3, 0.0)]), [2, 1]);
        assert_eq!(order(vec![hit(1, 0.10, 400.0), hit(2, 0.40, 0.0)]), [1, 2]);
        let score = |h| rank_hits(vec![h], now, DEFAULT_RANK)[0].1;
        assert!((score(hit(1, 0.5, 0.0)) - 0.45).abs() < 1e-9);
        assert!((score(hit(1, 0.5, 30.0)) - 0.475).abs() < 1e-6);
        assert!((score(hit(1, 0.5, -10.0)) - 0.45).abs() < 1e-9);
    }

    #[test]
    fn refs_parse_strictly() {
        assert_eq!(parse_ref("fact:88"), Some(MemoryRef::Fact { id: 88 }));
        assert_eq!(
            parse_ref("doc:leetcode-graphs/roadmap.md#2"),
            Some(MemoryRef::Doc { project: "leetcode-graphs".into(), doc: "roadmap.md".into(), chunk: Some(2) })
        );
        assert_eq!(parse_ref("doc:Bad/roadmap.md"), None);
        assert_eq!(parse_ref("fact:x"), None);
        assert_eq!(doc_ref("p", "a.md", Some(1)), "doc:p/a.md#1");
    }

    // ── service ──────────────────────────────────────────────────────────────

    const ACTOR: &str = "supervisor";

    struct Harness {
        service: MemoryService,
        store: Arc<InMemoryStore>,
        embedder: Arc<FakeEmbedder>,
    }

    fn harness() -> Harness {
        let store = Arc::new(InMemoryStore::default());
        let embedder = FakeEmbedder::new();
        let seq = Arc::new(AtomicUsize::new(0));
        let service = MemoryService::new(store.clone(), embedder.clone())
            .with_patch_ids(Arc::new(move || format!("pa:{:04x}", seq.fetch_add(1, Ordering::SeqCst) + 1)));
        Harness { service, store, embedder }
    }

    fn code(err: anyhow::Error) -> (&'static str, Map<String, Value>) {
        let e = err.downcast::<MemoryError>().expect("a MemoryError");
        (e.code, e.details)
    }

    fn write<'a>(project: &'a str, doc: &'a str, body: &'a str) -> WriteDoc<'a> {
        WriteDoc { project, doc, body, summary: None, expected_version: None, actor: ACTOR, rationale: None }
    }

    #[tokio::test]
    async fn round_trips_a_project_and_lists_docs_with_summaries() {
        let h = harness();
        h.service.create_project("leetcode-graphs", "Графы для интервью").await.unwrap();
        h.service
            .write_doc(WriteDoc {
                summary: Some("Цель и рамки проекта"),
                ..write("leetcode-graphs", "passport.md", "# Паспорт\n\nЦель.\n")
            })
            .await
            .unwrap();
        h.service
            .write_doc(WriteDoc {
                summary: Some("Список тем"),
                ..write("leetcode-graphs", "roadmap.md", "# Roadmap\n")
            })
            .await
            .unwrap();

        let list = h.service.list_docs("leetcode-graphs").await.unwrap();
        assert_eq!(list.project.title, "Графы для интервью");
        let docs: Vec<(String, Option<String>, i32)> =
            list.docs.iter().map(|d| (d.name.clone(), d.summary.clone(), d.version)).collect();
        assert_eq!(
            docs,
            [
                ("passport.md".into(), Some("Цель и рамки проекта".into()), 1),
                ("roadmap.md".into(), Some("Список тем".into()), 1)
            ]
        );
        assert_eq!(h.service.read_doc("leetcode-graphs", "roadmap.md").await.unwrap().body, "# Roadmap\n");
    }

    #[tokio::test]
    async fn appends_without_a_version_and_names_what_exists_on_misses() {
        let h = harness();
        h.service.create_project("p", "P").await.unwrap();
        h.service.write_doc(write("p", "progress.md", "# Прогресс\n\nДень 1: BFS\n")).await.unwrap();
        let written = h.service.append_doc("p", "progress.md", "День 2: Dijkstra", None, ACTOR, None).await.unwrap();
        assert_eq!(written.version, 2);
        assert_eq!(
            h.service.read_doc("p", "progress.md").await.unwrap().body,
            "# Прогресс\n\nДень 1: BFS\n\nДень 2: Dijkstra\n"
        );

        let (c, d) = code(h.service.append_doc("p", "roadmap2.md", "y", None, ACTOR, None).await.unwrap_err());
        assert_eq!((c, &d["docs"]), ("doc_not_found", &json!(["progress.md"])));
        let (c, d) = code(h.service.read_doc("q", "a.md").await.unwrap_err());
        assert_eq!((c, &d["projects"]), ("project_not_found", &json!(["p"])));
        assert!(
            h.service.create_project("Bad Slug", "x").await.unwrap_err().to_string().contains("Invalid project slug")
        );
        assert!(
            h.service
                .write_doc(write("p", "../etc/passwd", "x"))
                .await
                .unwrap_err()
                .to_string()
                .contains("Invalid document name")
        );
        assert_eq!(code(h.service.create_project("p", "Другое").await.unwrap_err()).0, "project_exists");
    }

    #[tokio::test]
    async fn write_doc_never_runs_blind() {
        let h = harness();
        h.service.create_project("p", "P").await.unwrap();
        assert_eq!(h.service.write_doc(write("p", "a.md", "one\n")).await.unwrap().version, 1);
        let (c, d) = code(h.service.write_doc(write("p", "a.md", "two\n")).await.unwrap_err());
        assert_eq!((c, &d["currentVersion"]), ("version_required", &json!(1)));
        assert_eq!(h.service.read_doc("p", "a.md").await.unwrap().body, "one\n");
        let ahead = WriteDoc { expected_version: Some(3), ..write("p", "new.md", "x") };
        assert_eq!(code(h.service.write_doc(ahead).await.unwrap_err()).0, "version_conflict");
    }

    #[tokio::test]
    async fn patch_doc_checks_versions_and_leaves_failures_untouched() {
        let h = harness();
        h.service.create_project("p", "P").await.unwrap();
        h.service.write_doc(write("p", "roadmap.md", ROADMAP)).await.unwrap();
        let ok = h
            .service
            .patch_doc("p", "roadmap.md", 1, vec![edit("- [ ] Dijkstra", "- [x] Dijkstra")], ACTOR, Some("отметь"))
            .await;
        assert_eq!(ok.unwrap().version, 2);

        let (c, d) = code(
            h.service.patch_doc("p", "roadmap.md", 1, vec![edit("- [ ] BFS", "x")], ACTOR, None).await.unwrap_err(),
        );
        assert_eq!((c, &d["currentVersion"]), ("version_conflict", &json!(2)));

        let (c, d) = code(
            h.service
                .patch_doc(
                    "p",
                    "roadmap.md",
                    2,
                    vec![edit("- [ ] BFS", "- [x] BFS"), edit("- [ ] Floyd", "x")],
                    ACTOR,
                    None,
                )
                .await
                .unwrap_err(),
        );
        assert_eq!((c, &d["applied"]), ("edit_failed", &json!(false)));
        let doc = h.service.read_doc("p", "roadmap.md").await.unwrap();
        assert_eq!((doc.version, doc.body.contains("- [ ] BFS")), (2, true));
    }

    #[tokio::test]
    async fn append_absorbs_a_write_that_lands_between_read_and_commit() {
        let h = harness();
        h.service.create_project("p", "P").await.unwrap();
        h.service.write_doc(write("p", "log.md", "start\n")).await.unwrap();
        let store = h.store.clone();
        let sneaked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *h.store.before_update.lock().unwrap() = Some(Box::new(move |doc_id| {
            let (store, sneaked) = (store.clone(), sneaked.clone());
            Box::pin(async move {
                if sneaked.swap(true, Ordering::SeqCst) {
                    return;
                }
                let current = store.get_doc(1, "log.md").await.unwrap().unwrap();
                *store.before_update.lock().unwrap() = None;
                store
                    .update_doc(doc_id, current.version, &format!("{}\nfrom other agent\n", current.body), None)
                    .await
                    .unwrap();
            })
        }));
        h.service.append_doc("p", "log.md", "mine", None, ACTOR, None).await.unwrap();
        let body = h.service.read_doc("p", "log.md").await.unwrap().body;
        assert!(body.contains("from other agent") && body.contains("mine"), "{body}");
    }

    #[tokio::test]
    async fn history_and_revert_follow_the_rules() {
        let h = harness();
        h.service.create_project("p", "P").await.unwrap();
        h.service.write_doc(write("p", "roadmap.md", "- [ ] BFS\n- [ ] Dijkstra\n")).await.unwrap();
        let first = h
            .service
            .patch_doc("p", "roadmap.md", 1, vec![edit("- [ ] BFS", "- [x] BFS")], "claude-code", Some("BFS пройден"))
            .await
            .unwrap();
        let history = h.service.history("p", "roadmap.md", 20).await.unwrap();
        assert_eq!((history[0].kind, history[0].edit_count, history[1].version_before), (PatchKind::Patch, 1, 0));
        assert!(history[0].patch_id.starts_with("pa:"));

        // Newest reverts exactly, and the revert is itself a patch.
        h.service.revert("p", "roadmap.md", &first.patch_id, false, ACTOR).await.unwrap();
        let doc = h.service.read_doc("p", "roadmap.md").await.unwrap();
        assert_eq!((doc.body.as_str(), doc.version), ("- [ ] BFS\n- [ ] Dijkstra\n", 3));

        // Mid-stack: untouched text reverts in place, an extended line still
        // reverts, a rewritten anchor conflicts and offers a rollback.
        let a =
            h.service.patch_doc("p", "roadmap.md", 3, vec![edit("- [ ] BFS", "- [x] BFS")], ACTOR, None).await.unwrap();
        h.service
            .patch_doc("p", "roadmap.md", 4, vec![edit("- [x] BFS", "- [x] BFS (повторить)")], ACTOR, None)
            .await
            .unwrap();
        h.service.revert("p", "roadmap.md", &a.patch_id, false, ACTOR).await.unwrap();
        assert_eq!(
            h.service.read_doc("p", "roadmap.md").await.unwrap().body,
            "- [ ] BFS (повторить)\n- [ ] Dijkstra\n"
        );

        let b = h
            .service
            .patch_doc("p", "roadmap.md", 6, vec![edit("- [ ] Dijkstra", "- [x] Dijkstra")], ACTOR, None)
            .await
            .unwrap();
        h.service
            .patch_doc("p", "roadmap.md", 7, vec![edit("- [x] Dijkstra", "- [x] Дейкстра")], ACTOR, None)
            .await
            .unwrap();
        let (c, d) = code(h.service.revert("p", "roadmap.md", &b.patch_id, false, ACTOR).await.unwrap_err());
        assert_eq!((c, &d["rollbackToVersion"]), ("revert_conflict", &json!(6)));
        h.service.revert("p", "roadmap.md", &b.patch_id, true, ACTOR).await.unwrap();
        assert_eq!(
            h.service.read_doc("p", "roadmap.md").await.unwrap().body,
            "- [ ] BFS (повторить)\n- [ ] Dijkstra\n"
        );

        let (c, d) = code(h.service.revert("p", "roadmap.md", "pa:beef", false, ACTOR).await.unwrap_err());
        assert_eq!(c, "patch_not_found");
        assert!(!d["knownPatchIds"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_append_cannot_be_undone_in_place_but_names_the_rollback() {
        let h = harness();
        h.service.create_project("p", "P").await.unwrap();
        h.service.write_doc(write("p", "r.md", "x\n")).await.unwrap();
        let appended = h.service.append_doc("p", "r.md", "- [ ] Floyd", None, ACTOR, None).await.unwrap();
        h.service.append_doc("p", "r.md", "- [ ] Kruskal", None, ACTOR, None).await.unwrap();
        let (c, d) = code(h.service.revert("p", "r.md", &appended.patch_id, false, ACTOR).await.unwrap_err());
        assert_eq!((c, &d["rollbackToVersion"]), ("revert_conflict", &json!(1)));
    }

    #[tokio::test]
    async fn facts_round_trip_and_archive_out_of_recall() {
        let h = harness();
        let fact =
            h.service.remember("Лёша платит за интернет 1-го числа", Some(&["интернет".into()]), None).await.unwrap();
        assert_eq!(h.service.get_fact(fact.id).await.unwrap().body, fact.body);
        assert_eq!(code(h.service.remember("  ", None, None).await.unwrap_err()).0, "empty_body");
        assert_eq!(code(h.service.get_fact(999).await.unwrap_err()).0, "fact_not_found");

        let hits = h.service.recall("интернет Лёша", None, None, None, Utc::now()).await.unwrap();
        assert_eq!(hits[0].r#ref, fact_ref(fact.id));
        h.service.update_fact(fact.id, None, None, Some(MemoryState::Archived)).await.unwrap();
        assert!(h.service.recall("интернет Лёша", None, None, None, Utc::now()).await.unwrap().is_empty());
        let archived =
            h.service.recall("интернет Лёша", None, Some(&[MemoryState::Archived]), None, Utc::now()).await.unwrap();
        assert_eq!(archived.len(), 1);
    }

    #[tokio::test]
    async fn recall_spans_documents_and_drops_stale_chunks() {
        let h = harness();
        h.service.create_project("graphs", "Графы").await.unwrap();
        h.service.write_doc(write("graphs", "progress.md", "## Прогресс\n\nзастрял на dijkstra\n")).await.unwrap();
        h.service.remember("купить молоко и хлеб", None, None).await.unwrap();
        let hits = h.service.recall("dijkstra прогресс графы", Some(1), None, None, Utc::now()).await.unwrap();
        assert_eq!(hits[0].r#ref, "doc:graphs/progress.md#0");

        h.service
            .write_doc(WriteDoc { expected_version: Some(1), ..write("graphs", "progress.md", "") })
            .await
            .unwrap();
        let hits = h.service.recall("dijkstra прогресс графы", None, None, None, Utc::now()).await.unwrap();
        assert!(hits.iter().all(|hit| !hit.r#ref.starts_with("doc:")));
    }

    #[tokio::test]
    async fn the_embedder_being_down_degrades_search_not_writes() {
        let h = harness();
        h.embedder.set_down(true);
        h.service.create_project("p", "P").await.unwrap();
        h.service.write_doc(write("p", "a.md", "текст\n")).await.unwrap();
        h.service.patch_doc("p", "a.md", 1, vec![edit("текст", "новый текст")], ACTOR, None).await.unwrap();
        assert_eq!(
            code(h.service.recall("текст", None, None, None, Utc::now()).await.unwrap_err()).0,
            "search_unavailable"
        );

        h.embedder.set_down(false);
        let drained = h.service.indexer().embed_missing_batch(100).await.unwrap();
        assert_eq!((drained.embedded, drained.failed), (1, 0));
        assert_eq!(h.service.indexer().embed_missing_batch(100).await.unwrap().embedded, 0);
        assert_eq!(
            h.service.recall("новый текст", None, None, None, Utc::now()).await.unwrap()[0].r#ref,
            "doc:p/a.md#0"
        );
    }

    #[tokio::test]
    async fn imports_legacy_notes_idempotently() {
        let h = harness();
        let notes = vec![
            LegacyNote {
                id: 1, body: "пароль от роутера на наклейке".into(), tags: vec!["роутер".into()]
            },
            LegacyNote { id: 2, body: "   ".into(), tags: vec![] },
        ];
        assert_eq!(import_legacy_notes(&notes, &h.service).await.unwrap(), (1, 1));
        assert_eq!(import_legacy_notes(&notes, &h.service).await.unwrap(), (0, 2));
        let fact = h.store.get_fact_by_source(&legacy_note_source(1)).await.unwrap().unwrap();
        assert_eq!(fact.tags, ["роутер"]);
    }

    // The same rules against Postgres: the CAS, patch history order, revert,
    // index replacement and recall filters are SQL there, not Rust.
    #[tokio::test]
    async fn pg_store_upholds_the_contract() {
        let Some(pool) = crate::pg::test_pool().await else { return };
        let service = MemoryService::new(Arc::new(PgMemoryStore::new(pool)), FakeEmbedder::with_dims(1536));
        let slug = format!("pg-test-{}", rand::random::<u32>());
        service.create_project(&slug, "PG test").await.unwrap();
        assert_eq!(code(service.create_project(&slug, "again").await.unwrap_err()).0, "project_exists");

        let created = service
            .write_doc(WriteDoc {
                summary: Some("roadmap"),
                ..write(&slug, "roadmap.md", "## Темы\n\n- [ ] BFS\n- [ ] Dijkstra\n")
            })
            .await
            .unwrap();
        assert_eq!(created.version, 1);
        let patched = service
            .patch_doc(&slug, "roadmap.md", 1, vec![edit("- [ ] BFS", "- [x] BFS")], ACTOR, Some("BFS done"))
            .await
            .unwrap();
        assert_eq!(
            code(service.patch_doc(&slug, "roadmap.md", 1, vec![edit("x", "y")], ACTOR, None).await.unwrap_err()).0,
            "version_conflict"
        );
        service.append_doc(&slug, "roadmap.md", "- [ ] A*", Some("Темы"), ACTOR, None).await.unwrap();

        let doc = service.read_doc(&slug, "roadmap.md").await.unwrap();
        assert_eq!((doc.version, doc.summary.as_deref()), (3, Some("roadmap")));
        assert_eq!(doc.body, "## Темы\n\n- [x] BFS\n- [ ] Dijkstra\n\n- [ ] A*\n");
        let history = service.history(&slug, "roadmap.md", 10).await.unwrap();
        assert_eq!(
            history.iter().map(|h| h.kind).collect::<Vec<_>>(),
            [PatchKind::Append, PatchKind::Patch, PatchKind::Write]
        );
        assert_eq!(history[1].rationale.as_deref(), Some("BFS done"));

        service.revert(&slug, "roadmap.md", &patched.patch_id, false, ACTOR).await.unwrap();
        assert!(service.read_doc(&slug, "roadmap.md").await.unwrap().body.contains("- [ ] BFS"));

        let hits = service.recall("Dijkstra темы", Some(5), None, None, Utc::now()).await.unwrap();
        assert!(hits.iter().any(|h| h.r#ref == format!("doc:{slug}/roadmap.md#0")));

        let tag = format!("tag{}", rand::random::<u32>());
        let fact = service.remember("кот любит сметану", Some(std::slice::from_ref(&tag)), Some("test")).await.unwrap();
        let tagged =
            service.recall("кот сметана", Some(5), None, Some(std::slice::from_ref(&tag)), Utc::now()).await.unwrap();
        assert_eq!(tagged.iter().map(|h| h.r#ref.clone()).collect::<Vec<_>>(), [fact_ref(fact.id)]);
        service.update_fact(fact.id, Some(" кот любит сливки "), None, Some(MemoryState::Archived)).await.unwrap();
        assert_eq!(service.get_fact(fact.id).await.unwrap().body, "кот любит сливки");
        assert!(
            service
                .recall("кот", Some(5), None, Some(std::slice::from_ref(&tag)), Utc::now())
                .await
                .unwrap()
                .is_empty()
        );
        let archived =
            service.recall("кот", Some(5), Some(&[MemoryState::Archived]), Some(&[tag]), Utc::now()).await.unwrap();
        assert_eq!(archived.len(), 1);

        // Emptying a document drops its chunks from the projection.
        service.write_doc(WriteDoc { expected_version: Some(4), ..write(&slug, "roadmap.md", "") }).await.unwrap();
        let after = service.recall("Dijkstra темы", Some(50), None, None, Utc::now()).await.unwrap();
        assert!(after.iter().all(|h| !h.r#ref.starts_with(&format!("doc:{slug}/"))));
    }

    #[test]
    fn tool_envelope_flattens_payloads_and_errors() {
        let text = |r: ToolResult| {
            serde_json::from_str::<Value>(&r.unwrap().content[0].as_text().expect("text").text).unwrap()
        };
        let ok = text(run(Ok(WriteResult {
            project: "p".into(),
            doc: "a.md".into(),
            version: 2,
            patch_id: "pa:1".into(),
            size_bytes: 3,
        })));
        assert_eq!(
            ok,
            json!({ "ok": true, "project": "p", "doc": "a.md", "version": 2, "patchId": "pa:1", "sizeBytes": 3 })
        );
        let err = text(run::<()>(Err(version_conflict("p", "a.md", 9, 1))));
        assert_eq!(err["ok"], false);
        assert_eq!(err["error"], "version_conflict");
        assert_eq!(err["currentVersion"], 9);
        // A malformed slug is not a MemoryError: it fails the call.
        assert_eq!(run::<()>(Err(anyhow::anyhow!("Invalid project slug"))).unwrap().is_error, Some(true));
    }
}

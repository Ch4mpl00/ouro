// The workflow definition: 1. types; 2. parsing (YAML/JSON → types, with
// path-qualified errors); 3. validation against a function catalog
// (expressions compile, calls resolve, flow targets exist); 4. static
// analysis (the call sites, for manifests and previews).
//
// What the core implements of DSL 1.0 (the semantics of every task and
// definition property): do, call, emit, for, fork, listen (one/any/all,
// until, filters, correlation, read, foreach), raise, run (workflow natively;
// container/script/shell through a host process runner), set, switch,
// try/catch with retry (limit.attempt.duration included), wait;
// input/output/export transforms and schemas (inline or a host-resolved
// `resource`); if/then/timeout; schedule (every, cron, after, on; missed
// slots per `document.metadata.catchup`, an extension: `latest` | `none`);
// use.functions/errors/retries/timeouts/extensions/secrets; evaluate (jq,
// strict). Runtime arguments: $context, $input, $output, $task, $workflow,
// $runtime, $run (an extension: jump and visit counts), and $secrets in
// `input.from` only. The jump budget's tightening and its `on_exceeded`
// handling live in `document.metadata.limits` (an extension).
//
// Not the core's, rejected here: use.authentications and use.catalogs
// (protocol functions and packs carry credentials and resolve their own
// functions), $authorization (it describes a call's authentication),
// evaluate.mode `loose` and languages other than jq. Protocol calls (`call:
// http`, `openapi`, …) are ordinary host functions; a construct whose host
// capability is missing (events, emit, child runs, a process runner,
// secrets, resources) fails validation, never later.
use crate::expr::{self, Args};
use serde_json::{Map, Value};
use std::sync::OnceLock;
use std::time::Duration;

// ── 1. types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Workflow {
    pub document: Document,
    pub input: Option<Input>,
    pub output: Option<Output>,
    pub uses: Uses,
    pub tasks: TaskList,
    pub timeout: Option<DurationSpec>,
    /// When a host's scheduler starts runs; the interpreter ignores it.
    pub schedule: Option<Schedule>,
    /// `document.metadata.limits`: a tighter jump budget, and what happens
    /// when it runs out.
    pub budget: Budget,
    /// The definition as parsed, exposed as `$workflow.definition`.
    pub definition: Value,
}

/// `document.metadata.limits` (an extension: the OWS document admits no other
/// property, and `metadata` is free-form). Only jumps to a named task count
/// (`then: <task>` on a task, a switch case or a catch): `for` is bounded by
/// its collection and `retry` by its attempts. The host's ceiling
/// (`run::Limits::max_jumps`) bounds the whole run tree; a definition may
/// only tighten it.
#[derive(Clone, Debug, Default)]
pub struct Budget {
    /// At most this many jumps; above the host's ceiling fails validation.
    pub max_jumps: Option<u64>,
    pub on_exceeded: Exceeded,
}

impl Budget {
    /// The budget a run of this definition gets under `ceiling`.
    pub fn max_jumps(&self, ceiling: u64) -> u64 {
        self.max_jumps.map_or(ceiling, |n| n.min(ceiling))
    }
}

/// What a refused jump does.
#[derive(Clone, Debug, Default)]
pub enum Exceeded {
    /// Raise `budget.exhausted`, which `try` can catch (omitted).
    #[default]
    Raise,
    /// End the run normally; its output is the jumping task's (`end`).
    End,
    /// Run this task (typically a `call`); the run ends with its output. It
    /// may run a few tasks (`run::HANDLER_TASKS`) and cannot jump.
    Task(Box<Task>),
}

#[derive(Clone, Debug)]
pub struct Document {
    pub dsl: String,
    pub namespace: String,
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Uses {
    pub functions: Vec<(String, Task)>,
    pub errors: Vec<(String, ErrorDef)>,
    pub retries: Vec<(String, Retry)>,
    pub timeouts: Vec<(String, DurationSpec)>,
    /// In declaration order: the first wraps the others (its `before` runs
    /// first, its `after` last).
    pub extensions: Vec<(String, Extension)>,
    /// The names `$secrets` may hold; the host resolves their values.
    pub secrets: Vec<String>,
}

/// Tasks run around every task of a kind (`use.extensions`).
#[derive(Clone, Debug)]
pub struct Extension {
    /// A task kind (`Kind::name`), `composite` (do and fork) or `all`.
    pub extend: String,
    pub when: Option<String>,
    pub before: Option<TaskList>,
    pub after: Option<TaskList>,
}

impl Extension {
    pub fn extends(&self, kind: &Kind) -> bool {
        match self.extend.as_str() {
            "all" => true,
            "composite" => matches!(kind, Kind::Do(_) | Kind::Fork { .. }),
            other => other == kind.name(),
        }
    }
}

/// `schedule`: when a host's scheduler starts runs of the definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    pub every: Option<Duration>,
    pub cron: Option<String>,
    pub after: Option<Duration>,
    pub on: Option<Strategy>,
    pub read: Read,
    /// `document.metadata.catchup` (an extension): what becomes of `every`
    /// and `cron` slots missed while no scheduler ran.
    pub catchup: Catchup,
}

/// A missed slot is one whose following slot is due as well.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Catchup {
    /// One late run for the missed slots (the default).
    #[default]
    Latest,
    /// Skip missed slots (with a record); run at the next one.
    None,
}

/// `listen`: what to consume, how to read it, what to do with each event.
#[derive(Clone, Debug)]
pub struct Listen {
    pub to: Strategy,
    pub read: Read,
    pub foreach: Option<Foreach>,
}

/// An event consumption strategy.
#[derive(Clone, Debug, PartialEq)]
pub enum Strategy {
    One(EventFilter),
    /// An empty list consumes any event.
    Any {
        filters: Vec<EventFilter>,
        until: Option<Until>,
    },
    All(Vec<EventFilter>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Until {
    /// Evaluated on the events consumed so far, after each one.
    Condition(String),
    /// Events consumed by this strategy stop the listen; they are not output.
    Events(Box<Strategy>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventFilter {
    /// Attribute name → literal (equality or a regular expression) or
    /// runtime expression (see `events::matches`).
    pub with: Map<String, Value>,
    pub correlate: Vec<(String, Correlation)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Correlation {
    pub from: String,
    /// A constant, or a `${ }` expression evaluated on the task input.
    pub expect: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Read {
    #[default]
    Data,
    Envelope,
    Raw,
}

/// `listen.foreach`: tasks run for each consumed event, in consumption order.
#[derive(Clone, Debug)]
pub struct Foreach {
    pub item: String,
    pub at: String,
    pub tasks: TaskList,
    pub output: Option<Output>,
    pub export: Option<Export>,
}

/// `run`: a process, awaited or not, and what of its result the task outputs.
#[derive(Clone, Debug)]
pub struct RunTask {
    pub process: Process,
    pub await_: bool,
    pub return_: Return,
}

#[derive(Clone, Debug)]
pub enum Process {
    /// A child run of a workflow the host's catalog holds.
    Workflow { namespace: String, name: String, version: String, input: Option<Value> },
    /// `container`, `script` or `shell` and its definition (a template),
    /// for a host-registered process runner.
    External { kind: String, definition: Value },
}

impl Process {
    /// The name its durable call records carry (`run:shell`, …).
    pub fn function(kind: &str) -> String {
        format!("run:{kind}")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Return {
    #[default]
    Stdout,
    Stderr,
    Code,
    All,
    None,
}

pub type TaskList = Vec<(String, Task)>;

#[derive(Clone, Debug)]
pub struct Task {
    pub delivery: crate::durable::Delivery,
    pub hold: Duration,
    pub when: Option<String>,
    pub input: Option<Input>,
    pub output: Option<Output>,
    pub export: Option<Export>,
    pub timeout: Option<DurationSpec>,
    pub then: Option<Flow>,
    pub kind: Kind,
    /// The task's own definition, exposed as `$task.definition`.
    pub definition: Value,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Call {
        function: String,
        with: Option<Value>,
    },
    Do(TaskList),
    /// `concurrency` (`metadata.concurrency`, an extension): iterations run
    /// at once, at most that many, each on the `for`'s input and a copy of
    /// the context; the output is their outputs in item order.
    For {
        each: String,
        at: String,
        items: Items,
        condition: Option<String>,
        concurrency: Option<usize>,
        tasks: TaskList,
    },
    Fork {
        branches: TaskList,
        compete: bool,
    },
    Set(Value),
    Switch(Vec<(String, Case)>),
    Try {
        tasks: TaskList,
        catch: Box<Catch>,
    },
    Raise(Raise),
    Wait(DurationSpec),
    Listen(Box<Listen>),
    /// `emit.event.with`: the event's attributes, a template.
    Emit(Map<String, Value>),
    Run(Box<RunTask>),
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Call { .. } => "call",
            Kind::Do(_) => "do",
            Kind::For { .. } => "for",
            Kind::Fork { .. } => "fork",
            Kind::Set(_) => "set",
            Kind::Switch(_) => "switch",
            Kind::Try { .. } => "try",
            Kind::Raise(_) => "raise",
            Kind::Wait(_) => "wait",
            Kind::Listen(_) => "listen",
            Kind::Emit(_) => "emit",
            Kind::Run(_) => "run",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Items {
    Expr(String),
    Inline(Vec<Value>),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Flow {
    Continue,
    Exit,
    End,
    Goto(String),
}

/// A JSON Schema: inline, or an external resource the host resolves.
#[derive(Clone, Debug, PartialEq)]
pub enum Schema {
    Document(Value),
    Resource(String),
}

#[derive(Clone, Debug, Default)]
pub struct Input {
    pub schema: Option<Schema>,
    /// A bare/`${}` expression string, or an object template.
    pub from: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub schema: Option<Schema>,
    pub as_: Option<Value>,
}

pub type Export = Output;

#[derive(Clone, Debug)]
pub struct Case {
    pub when: Option<String>,
    pub then: Flow,
}

#[derive(Clone, Debug)]
pub struct Catch {
    /// Static filter: each listed property must equal the error's.
    pub errors: Option<Map<String, Value>>,
    pub as_: String,
    pub when: Option<String>,
    pub except_when: Option<String>,
    pub retry: Option<Retry>,
    pub tasks: Option<TaskList>,
    pub then: Option<Flow>,
}

#[derive(Clone, Debug, Default)]
pub struct Retry {
    pub when: Option<String>,
    pub except_when: Option<String>,
    pub delay: Option<DurationSpec>,
    pub backoff: Backoff,
    pub attempts: Option<u32>,
    /// `limit.attempt.duration`: how long each attempt may run.
    pub attempt_duration: Option<DurationSpec>,
    pub duration: Option<DurationSpec>,
    pub jitter: Option<(DurationSpec, DurationSpec)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backoff {
    #[default]
    Constant,
    Linear,
    Exponential,
}

#[derive(Clone, Debug)]
pub enum Raise {
    Inline(ErrorDef),
    Named(String),
}

#[derive(Clone, Debug)]
pub struct ErrorDef {
    pub type_: String,
    pub status: u16,
    /// Literal or `${}` expression.
    pub title: Option<String>,
    pub detail: Option<String>,
    pub instance: Option<String>,
}

#[derive(Clone, Debug)]
pub enum DurationSpec {
    Fixed(Duration),
    /// An expression producing an ISO 8601 duration string.
    Expr(String),
}

// ── 2. parsing ─────────────────────────────────────────────────────────────

/// A definition problem, located by JSON pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub at: String,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", if self.at.is_empty() { "/" } else { &self.at }, self.message)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid workflow:\n{}", .0.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))]
pub struct Invalid(pub Vec<Problem>);

/// Parses a YAML (or JSON, which is YAML) definition.
pub fn parse_yaml(text: &str) -> Result<Workflow, Invalid> {
    let value: Value =
        serde_yaml_ng::from_str(text).map_err(|e| Invalid(vec![problem("", format!("not YAML: {e}"))]))?;
    parse(value)
}

/// Our parser runs first because its errors name the exact property and the
/// fix; the official schema then guarantees that whatever we accept is a
/// valid definition by the specification too.
pub fn parse(definition: Value) -> Result<Workflow, Invalid> {
    let mut p = Parser::default();
    let wf = p.workflow(&definition);
    match (wf, p.problems.is_empty()) {
        (Some(wf), true) => {
            conforms(&definition)?;
            Ok(wf)
        }
        _ => Err(Invalid(p.problems)),
    }
}

/// Validates against the official JSON Schema (`schema/workflow.yaml`).
pub fn conforms(definition: &Value) -> Result<(), Invalid> {
    static SCHEMA: OnceLock<jsonschema::Validator> = OnceLock::new();
    let validator = SCHEMA.get_or_init(|| {
        let schema: Value =
            serde_yaml_ng::from_str(include_str!("../schema/workflow.yaml")).expect("the bundled schema is YAML");
        jsonschema::validator_for(&schema).expect("the bundled schema compiles")
    });
    let problems: Vec<Problem> = validator
        .iter_errors(definition)
        .take(5)
        .map(|e| problem(&e.instance_path().to_string(), format!("not valid by the specification's schema: {e}")))
        .collect();
    if problems.is_empty() { Ok(()) } else { Err(Invalid(problems)) }
}

fn problem(at: &str, message: impl Into<String>) -> Problem {
    Problem { at: at.to_string(), message: message.into() }
}

const COMMON: &[&str] = &["if", "input", "output", "export", "timeout", "then", "metadata"];

#[derive(Default)]
struct Parser {
    problems: Vec<Problem>,
    /// Named retries and timeouts, resolved where tasks refer to them.
    retries: Vec<(String, Retry)>,
    timeouts: Vec<(String, DurationSpec)>,
}

impl Parser {
    fn fail(&mut self, at: &str, message: impl Into<String>) {
        self.problems.push(problem(at, message));
    }

    fn object<'v>(&mut self, v: &'v Value, at: &str) -> Option<&'v Map<String, Value>> {
        match v {
            Value::Object(m) => Some(m),
            _ => {
                self.fail(at, "expected an object");
                None
            }
        }
    }

    fn only(&mut self, m: &Map<String, Value>, at: &str, allowed: &[&str]) {
        for k in m.keys() {
            if !allowed.contains(&k.as_str()) {
                self.fail(&format!("{at}/{k}"), format!("unknown property (allowed: {})", allowed.join(", ")));
            }
        }
    }

    fn string(&mut self, m: &Map<String, Value>, key: &str, at: &str) -> Option<String> {
        match m.get(key) {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                self.fail(&format!("{at}/{key}"), "expected a string");
                None
            }
        }
    }

    fn required_string(&mut self, m: &Map<String, Value>, key: &str, at: &str) -> String {
        match self.string(m, key, at) {
            Some(s) => s,
            None => {
                if !m.contains_key(key) {
                    self.fail(at, format!("missing `{key}`"));
                }
                String::new()
            }
        }
    }

    fn workflow(&mut self, v: &Value) -> Option<Workflow> {
        let m = self.object(v, "")?;
        self.only(m, "", &["document", "input", "output", "use", "do", "timeout", "schedule", "evaluate"]);
        if let Some(ev) = m.get("evaluate").and_then(|e| self.object(e, "/evaluate")) {
            self.only(ev, "/evaluate", &["language", "mode"]);
            if ev.get("language").is_some_and(|l| l != "jq") {
                self.fail("/evaluate/language", "only jq is supported");
            }
            if ev.get("mode").is_some_and(|l| l != "strict") {
                self.fail("/evaluate/mode", "only strict mode is supported");
            }
        }
        let document = match m.get("document") {
            Some(d) => self.document(d),
            None => {
                self.fail("", "missing `document`");
                None
            }
        };
        let mut uses = m.get("use").map(|u| self.uses(u)).unwrap_or_default();
        self.retries = uses.retries.clone();
        self.timeouts = uses.timeouts.clone();
        for (name, def) in m
            .get("use")
            .and_then(Value::as_object)
            .and_then(|u| u.get("functions"))
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            // Functions are parsed again now that named retries/timeouts are known.
            if let Some(t) = self.task(def, &format!("/use/functions/{name}"))
                && let Some(slot) = uses.functions.iter_mut().find(|(n, _)| n == name)
            {
                slot.1 = t;
            }
        }
        if let Some(e) = m.get("use").and_then(|u| u.get("extensions")) {
            uses.extensions = self.extensions(e);
        }
        let tasks = match m.get("do") {
            Some(d) => self.task_list(d, "/do"),
            None => {
                self.fail("", "missing `do`");
                Vec::new()
            }
        };
        let input = m.get("input").map(|i| self.input(i, "/input"));
        let output = m.get("output").map(|o| self.output(o, "/output"));
        let timeout = m.get("timeout").and_then(|t| self.timeout(t, "/timeout"));
        let mut schedule = m.get("schedule").and_then(|s| self.schedule(s));
        // CHOICE: the OWS `schedule` object admits no extra property, so the
        // missed-slot policy is the document's metadata.
        match m.get("document").and_then(|d| d.get("metadata")).and_then(|d| d.get("catchup")) {
            None => {}
            Some(c) => match (c.as_str(), schedule.as_mut()) {
                (Some(_), None) => self.fail("/document/metadata/catchup", "catchup applies only with a schedule"),
                (Some("latest"), Some(s)) => s.catchup = Catchup::Latest,
                (Some("none"), Some(s)) => s.catchup = Catchup::None,
                _ => self.fail("/document/metadata/catchup", "expected latest or none"),
            },
        }
        let budget = match m.get("document").and_then(|d| d.get("metadata")).and_then(|d| d.get("limits")) {
            Some(l) => self.budget(l),
            None => Budget::default(),
        };
        Some(Workflow {
            document: document?,
            input,
            output,
            uses,
            tasks,
            timeout,
            schedule,
            budget,
            definition: v.clone(),
        })
    }

    fn budget(&mut self, v: &Value) -> Budget {
        const AT: &str = "/document/metadata/limits";
        let Some(m) = self.object(v, AT) else {
            return Budget::default();
        };
        self.only(m, AT, &["max_jumps", "on_exceeded"]);
        let max_jumps = match m.get("max_jumps") {
            None => None,
            Some(n) => {
                if n.as_u64().is_none() {
                    self.fail(&format!("{AT}/max_jumps"), "expected a non-negative integer");
                }
                n.as_u64()
            }
        };
        let on_exceeded = match m.get("on_exceeded") {
            None => Exceeded::Raise,
            Some(Value::String(s)) if s == "end" => Exceeded::End,
            Some(t @ Value::Object(_)) => match self.task(t, &format!("{AT}/on_exceeded")) {
                Some(t) => Exceeded::Task(Box::new(t)),
                None => Exceeded::Raise,
            },
            Some(_) => {
                self.fail(&format!("{AT}/on_exceeded"), "expected `end` or a task (e.g. `call: report`)");
                Exceeded::Raise
            }
        };
        Budget { max_jumps, on_exceeded }
    }

    fn document(&mut self, v: &Value) -> Option<Document> {
        let m = self.object(v, "/document")?;
        self.only(m, "/document", &["dsl", "namespace", "name", "version", "title", "summary", "tags", "metadata"]);
        let dsl = self.required_string(m, "dsl", "/document");
        if !dsl.is_empty() && !dsl.starts_with("1.") {
            self.fail("/document/dsl", format!("DSL {dsl} is not supported (1.x only)"));
        }
        Some(Document {
            dsl,
            namespace: self.required_string(m, "namespace", "/document"),
            name: self.required_string(m, "name", "/document"),
            version: self.required_string(m, "version", "/document"),
            title: self.string(m, "title", "/document"),
            summary: self.string(m, "summary", "/document"),
        })
    }

    fn uses(&mut self, v: &Value) -> Uses {
        let mut uses = Uses::default();
        let Some(m) = self.object(v, "/use") else {
            return uses;
        };
        for (k, why) in [
            ("authentications", "authentication is not supported; functions carry their own credentials"),
            ("catalogs", "external catalogs are not supported; functions come from the runtime's catalog"),
        ] {
            if m.contains_key(k) {
                self.fail(&format!("/use/{k}"), why);
            }
        }
        self.only(
            m,
            "/use",
            &["functions", "errors", "retries", "timeouts", "authentications", "catalogs", "extensions", "secrets"],
        );
        match m.get("secrets") {
            None => {}
            Some(Value::Array(names)) => {
                for (i, name) in names.iter().enumerate() {
                    match name.as_str().filter(|n| !n.is_empty()) {
                        Some(n) if uses.secrets.iter().any(|s| s == n) => {
                            self.fail(&format!("/use/secrets/{i}"), format!("secret `{n}` is declared twice"))
                        }
                        Some(n) => uses.secrets.push(n.to_string()),
                        None => self.fail(&format!("/use/secrets/{i}"), "expected a secret name"),
                    }
                }
            }
            Some(_) => self.fail("/use/secrets", "expected a list of secret names"),
        }
        if let Some(e) = m.get("errors").and_then(|e| self.object(e, "/use/errors")) {
            for (name, def) in e {
                if let Some(d) = self.error_def(def, &format!("/use/errors/{name}")) {
                    uses.errors.push((name.clone(), d));
                }
            }
        }
        if let Some(r) = m.get("retries").and_then(|r| self.object(r, "/use/retries")) {
            for (name, def) in r {
                if let Some(d) = self.retry(def, &format!("/use/retries/{name}")) {
                    uses.retries.push((name.clone(), d));
                }
            }
        }
        if let Some(t) = m.get("timeouts").and_then(|t| self.object(t, "/use/timeouts")) {
            for (name, def) in t {
                let at = format!("/use/timeouts/{name}");
                if let Some(d) = self.timeout_object(def, &at) {
                    uses.timeouts.push((name.clone(), d));
                }
            }
        }
        if let Some(f) = m.get("functions").and_then(|f| self.object(f, "/use/functions")) {
            // Placeholders keep declaration order; `workflow` parses the bodies
            // once the named retries/timeouts are in place.
            for name in f.keys() {
                let placeholder = Task {
                    delivery: crate::durable::Delivery::default(),
                    hold: Duration::ZERO,
                    when: None,
                    input: None,
                    output: None,
                    export: None,
                    timeout: None,
                    then: None,
                    kind: Kind::Do(Vec::new()),
                    definition: Value::Null,
                };
                uses.functions.push((name.clone(), placeholder));
            }
        }
        uses
    }

    fn task_list(&mut self, v: &Value, at: &str) -> TaskList {
        let Value::Array(items) = v else {
            self.fail(at, "expected a list of `- name: task` entries");
            return Vec::new();
        };
        let mut out: TaskList = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let at = format!("{at}/{i}");
            let Some(m) = self.object(item, &at) else {
                continue;
            };
            if m.len() != 1 {
                self.fail(&at, "each entry must be a single `name: task` pair");
                continue;
            }
            let (name, def) = m.iter().next().expect("one entry");
            if out.iter().any(|(n, _)| n == name) {
                self.fail(&at, format!("duplicate task name `{name}` in this list"));
            }
            if let Some(t) = self.task(def, &format!("{at}/{name}")) {
                out.push((name.clone(), t));
            }
        }
        out
    }

    fn task(&mut self, v: &Value, at: &str) -> Option<Task> {
        let m = self.object(v, at)?;
        let kind = if m.contains_key("for") {
            self.only(m, at, &[COMMON, &["for", "while", "do"]].concat());
            self.for_task(m, at)
        } else if m.contains_key("try") {
            self.only(m, at, &[COMMON, &["try", "catch"]].concat());
            self.try_task(m, at)
        } else if let Some(f) = m.get("fork") {
            self.only(m, at, &[COMMON, &["fork"]].concat());
            self.fork_task(f, &format!("{at}/fork"))
        } else if m.contains_key("call") {
            self.only(m, at, &[COMMON, &["call", "with"]].concat());
            let function = self.required_string(m, "call", at);
            let with = m.get("with").cloned();
            if let Some(w) = &with
                && !w.is_object()
            {
                self.fail(&format!("{at}/with"), "expected an object of arguments");
            }
            Some(Kind::Call { function, with })
        } else if let Some(s) = m.get("set") {
            self.only(m, at, &[COMMON, &["set"]].concat());
            if !(s.is_object() || s.as_str().is_some_and(|t| expr::strip(t).is_some())) {
                self.fail(
                    &format!("{at}/set"),
                    "expected an object or a `${ … }` expression (a fixed text is `set: '${ \"…\" }'`)",
                );
            }
            Some(Kind::Set(s.clone()))
        } else if let Some(s) = m.get("switch") {
            self.only(m, at, &[COMMON, &["switch"]].concat());
            self.switch_task(s, &format!("{at}/switch"))
        } else if let Some(r) = m.get("raise") {
            self.only(m, at, &[COMMON, &["raise"]].concat());
            self.raise_task(r, &format!("{at}/raise"))
        } else if let Some(v) = m.get("listen") {
            self.only(m, at, &[COMMON, &["listen", "foreach"]].concat());
            self.listen(v, m.get("foreach"), at)
        } else if let Some(e) = m.get("emit") {
            self.only(m, at, &[COMMON, &["emit"]].concat());
            self.emit(e, &format!("{at}/emit"))
        } else if let Some(r) = m.get("run") {
            self.only(m, at, &[COMMON, &["run"]].concat());
            self.run_task(r, &format!("{at}/run"))
        } else if let Some(w) = m.get("wait") {
            self.only(m, at, &[COMMON, &["wait"]].concat());
            self.duration(w, &format!("{at}/wait")).map(Kind::Wait)
        } else if let Some(d) = m.get("do") {
            self.only(m, at, &[COMMON, &["do"]].concat());
            Some(Kind::Do(self.task_list(d, &format!("{at}/do"))))
        } else {
            self.fail(
                at,
                "unknown task: expected one of call, do, emit, for, fork, listen, raise, run, set, switch, try, wait",
            );
            None
        };
        let when = self.string(m, "if", at);
        let input = m.get("input").map(|i| self.input(i, &format!("{at}/input")));
        let output = m.get("output").map(|o| self.output(o, &format!("{at}/output")));
        let export = m.get("export").map(|o| self.output(o, &format!("{at}/export")));
        let timeout = m.get("timeout").and_then(|t| self.timeout(t, &format!("{at}/timeout")));
        let then = m.get("then").and_then(|t| self.flow(t, &format!("{at}/then")));
        if m.get("metadata").and_then(|m| m.get("concurrency")).is_some() && !m.contains_key("for") {
            self.fail(&format!("{at}/metadata/concurrency"), "concurrency belongs on a for task");
        }
        let delivery = match m.get("metadata").and_then(|m| m.get("delivery")) {
            None => crate::durable::Delivery::AtLeastOnce,
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(policy) if m.contains_key("call") => policy,
                _ => {
                    self.fail(
                        &format!("{at}/metadata/delivery"),
                        "expected at_least_once or at_most_once on a call task",
                    );
                    crate::durable::Delivery::AtLeastOnce
                }
            },
        };
        let hold = match m.get("metadata").and_then(|m| m.get("hold")) {
            None => Duration::ZERO,
            Some(v) => {
                if !m.contains_key("listen") {
                    self.fail(&format!("{at}/metadata/hold"), "hold belongs on a listen task");
                }
                match v.as_str().and_then(parse_iso_duration) {
                    Some(d) => d,
                    None => {
                        self.fail(&format!("{at}/metadata/hold"), "expected an ISO 8601 duration (e.g. PT5S)");
                        Duration::ZERO
                    }
                }
            }
        };
        Some(Task { delivery, hold, when, input, output, export, timeout, then, kind: kind?, definition: v.clone() })
    }

    fn listen(&mut self, v: &Value, foreach: Option<&Value>, task_at: &str) -> Option<Kind> {
        let at = format!("{task_at}/listen");
        let m = self.object(v, &at)?;
        self.only(m, &at, &["to", "read"]);
        let read = self.read(m.get("read"), &format!("{at}/read"));
        let to = match m.get("to") {
            Some(to) => self.strategy(to, &format!("{at}/to"), true)?,
            None => {
                self.fail(&at, "missing `to`");
                return None;
            }
        };
        let foreach = match foreach {
            None => None,
            Some(f) => {
                let fat = format!("{task_at}/foreach");
                let f = self.object(f, &fat)?;
                self.only(f, &fat, &["item", "at", "do", "output", "export"]);
                Some(Foreach {
                    item: self.string(f, "item", &fat).unwrap_or_else(|| "item".into()),
                    at: self.string(f, "at", &fat).unwrap_or_else(|| "index".into()),
                    tasks: f.get("do").map(|d| self.task_list(d, &format!("{fat}/do"))).unwrap_or_default(),
                    output: f.get("output").map(|o| self.output(o, &format!("{fat}/output"))),
                    export: f.get("export").map(|o| self.output(o, &format!("{fat}/export"))),
                })
            }
        };
        Some(Kind::Listen(Box::new(Listen { to, read, foreach })))
    }

    fn read(&mut self, v: Option<&Value>, at: &str) -> Read {
        match v.map(|v| v.as_str()) {
            None | Some(Some("data")) => Read::Data,
            Some(Some("envelope")) => Read::Envelope,
            Some(Some("raw")) => Read::Raw,
            Some(_) => {
                self.fail(at, "expected data, envelope or raw");
                Read::Data
            }
        }
    }

    /// An event consumption strategy; `until` only where the DSL allows it.
    fn strategy(&mut self, v: &Value, at: &str, until: bool) -> Option<Strategy> {
        let m = self.object(v, at)?;
        self.only(m, at, &["one", "any", "all", "until"]);
        let kinds = ["one", "any", "all"].iter().filter(|k| m.contains_key(**k)).count();
        if kinds != 1 {
            self.fail(at, "expected exactly one of one, any, all");
            return None;
        }
        if m.contains_key("until") && !(until && m.contains_key("any")) {
            self.fail(&format!("{at}/until"), "until applies to `any` only, and not inside another until");
        }
        if let Some(one) = m.get("one") {
            return self.filter(one, &format!("{at}/one")).map(Strategy::One);
        }
        let list = |p: &mut Self, key: &str| -> Vec<EventFilter> {
            match m.get(key) {
                Some(Value::Array(items)) => {
                    items.iter().enumerate().filter_map(|(i, f)| p.filter(f, &format!("{at}/{key}/{i}"))).collect()
                }
                _ => {
                    p.fail(&format!("{at}/{key}"), "expected a list of event filters");
                    Vec::new()
                }
            }
        };
        if m.contains_key("all") {
            let filters = list(self, "all");
            if filters.is_empty() {
                self.fail(&format!("{at}/all"), "`all` needs at least one event filter");
            }
            return Some(Strategy::All(filters));
        }
        let filters = list(self, "any");
        let until = match m.get("until") {
            None => None,
            Some(Value::String(code)) => Some(Until::Condition(expr::bare(code).to_string())),
            Some(other) => self.strategy(other, &format!("{at}/until"), false).map(|s| Until::Events(Box::new(s))),
        };
        Some(Strategy::Any { filters, until })
    }

    fn filter(&mut self, v: &Value, at: &str) -> Option<EventFilter> {
        let m = self.object(v, at)?;
        self.only(m, at, &["with", "correlate"]);
        let with = match m.get("with") {
            Some(w) => self.object(w, &format!("{at}/with"))?.clone(),
            None => {
                self.fail(at, "missing `with`");
                return None;
            }
        };
        if with.is_empty() {
            self.fail(&format!("{at}/with"), "an event filter needs at least one attribute");
        }
        let mut correlate = Vec::new();
        if let Some(c) = m.get("correlate").and_then(|c| self.object(c, &format!("{at}/correlate"))) {
            for (name, def) in c {
                let cat = format!("{at}/correlate/{name}");
                let Some(d) = self.object(def, &cat) else { continue };
                self.only(d, &cat, &["from", "expect"]);
                let from = self.required_string(d, "from", &cat);
                let expect = self.string(d, "expect", &cat);
                correlate.push((name.clone(), Correlation { from: expr::bare(&from).to_string(), expect }));
            }
        }
        Some(EventFilter { with, correlate })
    }

    fn emit(&mut self, v: &Value, at: &str) -> Option<Kind> {
        let m = self.object(v, at)?;
        self.only(m, at, &["event"]);
        let Some(event) = m.get("event").and_then(|e| self.object(e, &format!("{at}/event"))) else {
            self.fail(at, "missing `event`");
            return None;
        };
        self.only(event, &format!("{at}/event"), &["with"]);
        let with = self.object(event.get("with").unwrap_or(&Value::Null), &format!("{at}/event/with"))?.clone();
        if with.get("type").and_then(Value::as_str).is_none_or(str::is_empty) {
            self.fail(&format!("{at}/event/with"), "an emitted event needs a `type`");
        }
        Some(Kind::Emit(with))
    }

    fn run_task(&mut self, v: &Value, at: &str) -> Option<Kind> {
        let m = self.object(v, at)?;
        self.only(m, at, &["container", "script", "shell", "workflow", "await", "return"]);
        let await_ = match m.get("await") {
            None => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                self.fail(&format!("{at}/await"), "expected a boolean");
                true
            }
        };
        let return_ = match m.get("return").map(|r| r.as_str()) {
            None | Some(Some("stdout")) => Return::Stdout,
            Some(Some("stderr")) => Return::Stderr,
            Some(Some("code")) => Return::Code,
            Some(Some("all")) => Return::All,
            Some(Some("none")) => Return::None,
            Some(_) => {
                self.fail(&format!("{at}/return"), "expected stdout, stderr, code, all or none");
                Return::Stdout
            }
        };
        let kinds: Vec<&str> =
            ["container", "script", "shell", "workflow"].into_iter().filter(|k| m.contains_key(*k)).collect();
        let [kind] = kinds[..] else {
            self.fail(at, "expected exactly one of container, script, shell, workflow");
            return None;
        };
        let wat = format!("{at}/{kind}");
        let definition = self.object(&m[kind], &wat)?;
        let process = if kind == "workflow" {
            self.only(definition, &wat, &["namespace", "name", "version", "input"]);
            Process::Workflow {
                namespace: self.required_string(definition, "namespace", &wat),
                name: self.required_string(definition, "name", &wat),
                version: self.string(definition, "version", &wat).unwrap_or_else(|| "latest".into()),
                input: definition.get("input").cloned(),
            }
        } else {
            Process::External { kind: kind.to_string(), definition: Value::Object(definition.clone()) }
        };
        Some(Kind::Run(Box::new(RunTask { process, await_, return_ })))
    }

    fn extensions(&mut self, v: &Value) -> Vec<(String, Extension)> {
        let Value::Array(items) = v else {
            self.fail("/use/extensions", "expected a list of `- name: extension` entries");
            return Vec::new();
        };
        let mut out: Vec<(String, Extension)> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let at = format!("/use/extensions/{i}");
            let Some((name, def)) = item.as_object().filter(|m| m.len() == 1).and_then(|m| m.iter().next()) else {
                self.fail(&at, "each entry must be a single `name: extension` pair");
                continue;
            };
            let at = format!("{at}/{name}");
            let Some(e) = self.object(def, &at) else { continue };
            self.only(e, &at, &["extend", "when", "before", "after"]);
            let extend = self.required_string(e, "extend", &at);
            const KINDS: &[&str] =
                &["call", "composite", "emit", "for", "listen", "raise", "run", "set", "switch", "try", "wait", "all"];
            if !extend.is_empty() && !KINDS.contains(&extend.as_str()) {
                self.fail(&format!("{at}/extend"), format!("expected one of {}", KINDS.join(", ")));
            }
            out.push((
                name.clone(),
                Extension {
                    extend,
                    when: self.string(e, "when", &at).map(|w| expr::bare(&w).to_string()),
                    before: e.get("before").map(|b| self.task_list(b, &format!("{at}/before"))),
                    after: e.get("after").map(|b| self.task_list(b, &format!("{at}/after"))),
                },
            ));
        }
        out
    }

    fn schedule(&mut self, v: &Value) -> Option<Schedule> {
        let at = "/schedule";
        let m = self.object(v, at)?;
        self.only(m, at, &["every", "cron", "after", "on", "read"]);
        let fixed = |p: &mut Self, key: &str| match m.get(key).and_then(|d| p.duration(d, &format!("{at}/{key}"))) {
            Some(DurationSpec::Fixed(d)) if !d.is_zero() => Some(d),
            Some(_) => {
                p.fail(&format!("{at}/{key}"), "expected a positive literal duration");
                None
            }
            None => None,
        };
        let every = fixed(self, "every");
        let after = fixed(self, "after");
        let cron = self.string(m, "cron", at);
        if let Some(c) = &cron
            && let Err(e) = c.parse::<croner::Cron>()
        {
            self.fail(&format!("{at}/cron"), format!("invalid cron expression: {e}"));
        }
        let on = m.get("on").and_then(|o| self.strategy(o, &format!("{at}/on"), true));
        if m.contains_key("read") && !m.contains_key("on") {
            self.fail(&format!("{at}/read"), "read applies only with `on`");
        }
        let read = self.read(m.get("read"), &format!("{at}/read"));
        if every.is_none() && after.is_none() && cron.is_none() && on.is_none() {
            self.fail(at, "a schedule needs every, cron, after or on");
        }
        Some(Schedule { every, cron, after, on, read, catchup: Catchup::Latest })
    }

    fn for_task(&mut self, m: &Map<String, Value>, at: &str) -> Option<Kind> {
        let f = self.object(&m["for"], &format!("{at}/for"))?;
        self.only(f, &format!("{at}/for"), &["each", "in", "at"]);
        let items = match f.get("in") {
            Some(Value::String(s)) => Items::Expr(expr::bare(s).to_string()),
            Some(Value::Array(a)) => Items::Inline(a.clone()),
            Some(_) => {
                self.fail(&format!("{at}/for/in"), "expected an expression or an inline array");
                return None;
            }
            None => {
                self.fail(&format!("{at}/for"), "missing `in`");
                return None;
            }
        };
        let each = self.string(f, "each", &format!("{at}/for")).unwrap_or_else(|| "item".into());
        let index = self.string(f, "at", &format!("{at}/for")).unwrap_or_else(|| "index".into());
        let condition = self.string(m, "while", at).map(|s| expr::bare(&s).to_string());
        let concurrency = match m.get("metadata").and_then(|m| m.get("concurrency")) {
            None => None,
            Some(n) => match n.as_u64().filter(|n| *n > 0).and_then(|n| usize::try_from(n).ok()) {
                Some(_) if condition.is_some() => {
                    self.fail(&format!("{at}/metadata/concurrency"), "a concurrent for has no `while`");
                    None
                }
                Some(n) => Some(n),
                None => {
                    self.fail(&format!("{at}/metadata/concurrency"), "expected a positive integer");
                    None
                }
            },
        };
        let tasks = match m.get("do") {
            Some(d) => self.task_list(d, &format!("{at}/do")),
            None => {
                self.fail(at, "a for task needs `do`");
                Vec::new()
            }
        };
        Some(Kind::For { each, at: index, items, condition, concurrency, tasks })
    }

    fn try_task(&mut self, m: &Map<String, Value>, at: &str) -> Option<Kind> {
        let tasks = self.task_list(&m["try"], &format!("{at}/try"));
        let Some(c) = m.get("catch") else {
            self.fail(at, "a try task needs `catch`");
            return None;
        };
        let cat = format!("{at}/catch");
        let c = self.object(c, &cat)?;
        self.only(c, &cat, &["errors", "as", "when", "exceptWhen", "retry", "do", "then"]);
        let errors = match c.get("errors") {
            None => None,
            Some(e) => {
                let e = self.object(e, &format!("{cat}/errors"))?;
                self.only(e, &format!("{cat}/errors"), &["with"]);
                e.get("with").and_then(|w| self.object(w, &format!("{cat}/errors/with"))).cloned()
            }
        };
        let retry = match c.get("retry") {
            None => None,
            Some(Value::String(name)) => match self.retries.iter().find(|(n, _)| n == name) {
                Some((_, r)) => Some(r.clone()),
                None => {
                    self.fail(&format!("{cat}/retry"), format!("no retry policy `{name}` under use.retries"));
                    None
                }
            },
            Some(r) => self.retry(r, &format!("{cat}/retry")),
        };
        Some(Kind::Try {
            tasks,
            catch: Box::new(Catch {
                errors,
                as_: self.string(c, "as", &cat).unwrap_or_else(|| "error".into()),
                when: self.string(c, "when", &cat).map(|s| expr::bare(&s).to_string()),
                except_when: self.string(c, "exceptWhen", &cat).map(|s| expr::bare(&s).to_string()),
                retry,
                tasks: c.get("do").map(|d| self.task_list(d, &format!("{cat}/do"))),
                then: c.get("then").and_then(|t| self.flow(t, &format!("{cat}/then"))),
            }),
        })
    }

    fn fork_task(&mut self, v: &Value, at: &str) -> Option<Kind> {
        let f = self.object(v, at)?;
        self.only(f, at, &["branches", "compete"]);
        let branches = match f.get("branches") {
            Some(b) => self.task_list(b, &format!("{at}/branches")),
            None => Vec::new(),
        };
        let compete = match f.get("compete") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                self.fail(&format!("{at}/compete"), "expected a boolean");
                false
            }
        };
        Some(Kind::Fork { branches, compete })
    }

    fn switch_task(&mut self, v: &Value, at: &str) -> Option<Kind> {
        let Value::Array(items) = v else {
            self.fail(at, "expected a list of `- name: case` entries");
            return None;
        };
        let mut cases = Vec::new();
        let mut defaults = 0;
        for (i, item) in items.iter().enumerate() {
            let cat = format!("{at}/{i}");
            let Some(m) = self.object(item, &cat) else {
                continue;
            };
            let Some((name, case)) = m.iter().next().filter(|_| m.len() == 1) else {
                self.fail(&cat, "each entry must be a single `name: case` pair");
                continue;
            };
            let cat = format!("{cat}/{name}");
            let Some(c) = self.object(case, &cat) else {
                continue;
            };
            self.only(c, &cat, &["when", "then"]);
            let when = self.string(c, "when", &cat).map(|s| expr::bare(&s).to_string());
            if when.is_none() {
                defaults += 1;
            }
            let Some(then) = c.get("then").and_then(|t| self.flow(t, &format!("{cat}/then"))) else {
                if !c.contains_key("then") {
                    self.fail(&cat, "a switch case needs `then`");
                }
                continue;
            };
            cases.push((name.clone(), Case { when, then }));
        }
        if defaults > 1 {
            self.fail(at, "only one case may omit `when` (the default)");
        }
        Some(Kind::Switch(cases))
    }

    fn raise_task(&mut self, v: &Value, at: &str) -> Option<Kind> {
        let r = self.object(v, at)?;
        self.only(r, at, &["error"]);
        match r.get("error") {
            Some(Value::String(name)) => Some(Kind::Raise(Raise::Named(name.clone()))),
            Some(e) => self.error_def(e, &format!("{at}/error")).map(|d| Kind::Raise(Raise::Inline(d))),
            None => {
                self.fail(at, "missing `error`");
                None
            }
        }
    }

    fn error_def(&mut self, v: &Value, at: &str) -> Option<ErrorDef> {
        let e = self.object(v, at)?;
        self.only(e, at, &["type", "status", "title", "detail", "instance"]);
        let status = match e.get("status") {
            Some(Value::Number(n)) if n.as_u64().is_some_and(|s| (100..=599).contains(&s)) => {
                n.as_u64().unwrap_or(500) as u16
            }
            Some(_) => {
                self.fail(&format!("{at}/status"), "expected an HTTP-style status between 100 and 599");
                500
            }
            None => {
                self.fail(at, "missing `status`");
                500
            }
        };
        Some(ErrorDef {
            type_: self.required_string(e, "type", at),
            status,
            title: self.string(e, "title", at),
            detail: self.string(e, "detail", at),
            instance: self.string(e, "instance", at),
        })
    }

    fn retry(&mut self, v: &Value, at: &str) -> Option<Retry> {
        let r = self.object(v, at)?;
        self.only(r, at, &["when", "exceptWhen", "delay", "backoff", "limit", "jitter"]);
        let mut retry = Retry {
            when: self.string(r, "when", at).map(|s| expr::bare(&s).to_string()),
            except_when: self.string(r, "exceptWhen", at).map(|s| expr::bare(&s).to_string()),
            delay: r.get("delay").and_then(|d| self.duration(d, &format!("{at}/delay"))),
            ..Retry::default()
        };
        if let Some(b) = r.get("backoff").and_then(|b| self.object(b, &format!("{at}/backoff"))) {
            self.only(b, &format!("{at}/backoff"), &["constant", "linear", "exponential"]);
            retry.backoff = if b.contains_key("exponential") {
                Backoff::Exponential
            } else if b.contains_key("linear") {
                Backoff::Linear
            } else {
                Backoff::Constant
            };
        }
        if let Some(l) = r.get("limit").and_then(|l| self.object(l, &format!("{at}/limit"))) {
            let lat = format!("{at}/limit");
            self.only(l, &lat, &["attempt", "duration"]);
            if let Some(a) = l.get("attempt").and_then(|a| self.object(a, &format!("{lat}/attempt"))) {
                self.only(a, &format!("{lat}/attempt"), &["count", "duration"]);
                // CHOICE: the reference calls it "the duration limit for all
                // retry attempts" next to `limit.duration` ("during which to
                // retry"); under `attempt`, beside the count, it bounds each
                // attempt, which times out like a task `timeout` and is
                // caught by this same `catch`.
                retry.attempt_duration =
                    a.get("duration").and_then(|d| self.duration(d, &format!("{lat}/attempt/duration")));
                retry.attempts = match a.get("count") {
                    None => None,
                    Some(Value::Number(n)) if n.as_u64().is_some() => {
                        Some(n.as_u64().unwrap_or(0).min(u32::MAX as u64) as u32)
                    }
                    Some(_) => {
                        self.fail(&format!("{lat}/attempt/count"), "expected a non-negative integer");
                        None
                    }
                };
            }
            retry.duration = l.get("duration").and_then(|d| self.duration(d, &format!("{lat}/duration")));
        }
        if let Some(j) = r.get("jitter").and_then(|j| self.object(j, &format!("{at}/jitter"))) {
            let from = j.get("from").and_then(|d| self.duration(d, &format!("{at}/jitter/from")));
            let to = j.get("to").and_then(|d| self.duration(d, &format!("{at}/jitter/to")));
            match (from, to) {
                (Some(f), Some(t)) => retry.jitter = Some((f, t)),
                _ => self.fail(&format!("{at}/jitter"), "jitter needs `from` and `to`"),
            }
        }
        Some(retry)
    }

    fn input(&mut self, v: &Value, at: &str) -> Input {
        let Some(m) = self.object(v, at) else {
            return Input::default();
        };
        self.only(m, at, &["schema", "from"]);
        Input {
            schema: m.get("schema").and_then(|s| self.schema(s, &format!("{at}/schema"))),
            from: self.transform(m.get("from"), &format!("{at}/from")),
        }
    }

    fn output(&mut self, v: &Value, at: &str) -> Output {
        let Some(m) = self.object(v, at) else {
            return Output::default();
        };
        self.only(m, at, &["schema", "as"]);
        Output {
            schema: m.get("schema").and_then(|s| self.schema(s, &format!("{at}/schema"))),
            as_: self.transform(m.get("as"), &format!("{at}/as")),
        }
    }

    /// `from`/`as`: an expression string (bare or `${}`) or an object template.
    fn transform(&mut self, v: Option<&Value>, at: &str) -> Option<Value> {
        match v? {
            Value::String(s) => Some(Value::String(expr::bare(s).to_string())),
            o @ Value::Object(_) => Some(o.clone()),
            _ => {
                self.fail(at, "expected an expression or an object");
                None
            }
        }
    }

    fn schema(&mut self, v: &Value, at: &str) -> Option<Schema> {
        let m = self.object(v, at)?;
        self.only(m, at, &["format", "document", "resource"]);
        if m.get("format").is_some_and(|f| f != "json") {
            self.fail(&format!("{at}/format"), "only JSON Schema (`json`) is supported");
        }
        if let (Some(r), None) = (m.get("resource"), m.get("document")) {
            let rat = format!("{at}/resource");
            let r = self.object(r, &rat)?;
            self.only(r, &rat, &["name", "endpoint"]);
            return match r.get("endpoint") {
                Some(Value::String(uri)) => Some(Schema::Resource(uri.clone())),
                Some(Value::Object(e)) => {
                    let eat = format!("{rat}/endpoint");
                    self.only(e, &eat, &["uri", "authentication"]);
                    if e.contains_key("authentication") {
                        self.fail(
                            &format!("{eat}/authentication"),
                            "authentication is not supported; the host's resource resolver carries credentials",
                        );
                    }
                    Some(Schema::Resource(self.required_string(e, "uri", &eat)))
                }
                _ => {
                    self.fail(&rat, "missing `endpoint`");
                    None
                }
            };
        }
        match m.get("document") {
            Some(d) => match jsonschema::validator_for(d) {
                Ok(_) => Some(Schema::Document(d.clone())),
                Err(e) => {
                    self.fail(&format!("{at}/document"), format!("invalid JSON Schema: {e}"));
                    None
                }
            },
            None => {
                self.fail(at, "missing `document`");
                None
            }
        }
    }

    fn timeout(&mut self, v: &Value, at: &str) -> Option<DurationSpec> {
        match v {
            Value::String(name) => match self.timeouts.iter().find(|(n, _)| n == name) {
                Some((_, d)) => Some(d.clone()),
                None => {
                    self.fail(at, format!("no timeout `{name}` under use.timeouts"));
                    None
                }
            },
            _ => self.timeout_object(v, at),
        }
    }

    fn timeout_object(&mut self, v: &Value, at: &str) -> Option<DurationSpec> {
        let m = self.object(v, at)?;
        self.only(m, at, &["after"]);
        match m.get("after") {
            Some(d) => self.duration(d, &format!("{at}/after")),
            None => {
                self.fail(at, "missing `after`");
                None
            }
        }
    }

    fn duration(&mut self, v: &Value, at: &str) -> Option<DurationSpec> {
        match v {
            Value::String(s) => match expr::strip(s) {
                Some(code) => Some(DurationSpec::Expr(code.to_string())),
                None => match parse_iso_duration(s) {
                    Some(d) => Some(DurationSpec::Fixed(d)),
                    None => {
                        self.fail(at, format!("`{s}` is not an ISO 8601 duration (e.g. PT30S)"));
                        None
                    }
                },
            },
            Value::Object(m) => {
                self.only(m, at, &["days", "hours", "minutes", "seconds", "milliseconds"]);
                let mut ms: u64 = 0;
                for (k, unit) in [
                    ("days", 86_400_000u64),
                    ("hours", 3_600_000),
                    ("minutes", 60_000),
                    ("seconds", 1_000),
                    ("milliseconds", 1),
                ] {
                    match m.get(k) {
                        None => {}
                        Some(Value::Number(n)) if n.as_u64().is_some() => {
                            ms = ms.saturating_add(n.as_u64().unwrap_or(0).saturating_mul(unit))
                        }
                        Some(_) => self.fail(&format!("{at}/{k}"), "expected a non-negative integer"),
                    }
                }
                Some(DurationSpec::Fixed(Duration::from_millis(ms)))
            }
            _ => {
                self.fail(at, "expected a duration object or an ISO 8601 string");
                None
            }
        }
    }

    fn flow(&mut self, v: &Value, at: &str) -> Option<Flow> {
        match v.as_str() {
            Some("continue") => Some(Flow::Continue),
            Some("exit") => Some(Flow::Exit),
            Some("end") => Some(Flow::End),
            Some(name) if !name.is_empty() => Some(Flow::Goto(name.to_string())),
            _ => {
                self.fail(at, "expected continue, exit, end or a task name");
                None
            }
        }
    }
}

/// `P[nD][T[nH][nM][n[.n]S]]` (weeks/months/years are not durations here).
pub fn parse_iso_duration(s: &str) -> Option<Duration> {
    let rest = s.trim().strip_prefix('P')?;
    if rest.is_empty() {
        return None;
    }
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) if !t.is_empty() => (d, Some(t)),
        Some(_) => return None,
        None => (rest, None),
    };
    let mut ms: f64 = 0.0;
    let mut take = |part: &str, units: &[(char, f64)]| -> Option<()> {
        let mut num = String::new();
        let mut unit_idx = 0;
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else {
                let pos = units[unit_idx..].iter().position(|(u, _)| *u == c)? + unit_idx;
                ms += num.parse::<f64>().ok()? * units[pos].1;
                num.clear();
                unit_idx = pos + 1;
            }
        }
        num.is_empty().then_some(())
    };
    take(date, &[('D', 86_400_000.0)])?;
    if let Some(t) = time {
        take(t, &[('H', 3_600_000.0), ('M', 60_000.0), ('S', 1_000.0)])?;
    }
    Some(Duration::from_millis(ms.round() as u64))
}

// ── 3. validation ──────────────────────────────────────────────────────────

/// What validation needs to know about the host: its functions and which of
/// the DSL's host-provided capabilities it has. A construct whose capability
/// is missing fails validation, never later.
pub trait Catalog {
    /// Whether `name` is callable here.
    fn has(&self, name: &str) -> bool;
    /// `listen`: durable event delivery.
    fn events(&self) -> bool {
        false
    }
    /// `emit`: publishing events.
    fn emits(&self) -> bool {
        false
    }
    /// `run: workflow`: child runs.
    fn children(&self) -> bool {
        false
    }
    /// `run: container | script | shell`: a process runner for `kind`.
    fn processes(&self, _kind: &str) -> bool {
        false
    }
    /// `use.secrets`: a secret resolver.
    fn secrets(&self) -> bool {
        false
    }
    /// `schema.resource`: a resource resolver.
    fn resources(&self) -> bool {
        false
    }
}

/// Normalises `name:version@default` (the runtime's default catalog) to
/// `name`; any other catalog suffix is kept and fails to resolve.
pub fn function_name(call: &str) -> &str {
    match call.strip_suffix("@default") {
        Some(rest) => rest.split_once(':').map_or(rest, |(n, _)| n),
        None => call,
    }
}

/// The runtime arguments every expression may use. `$secrets` is bound only
/// in `input.from` (see `SECRETS`); `$authorization` describes a call's
/// resolved authentication, which this runtime leaves to functions.
pub const BASE_VARS: &[&str] = &["context", "input", "output", "task", "workflow", "runtime", RUN];

/// `$run` (an extension): `{ jumps, maxJumps, visits }`, the jumps this run
/// took to a named task, its budget, and how often each task of the
/// enclosing lists was entered. Reserved: no loop or catch variable takes it.
pub const RUN: &str = "run";

/// The argument holding the declared secrets. CHOICE: the DSL's argument
/// table ticks `$secrets` for every expression, but its description says
/// "to avoid unintentional bleeding, secrets can only be used in the
/// `input.from` runtime expression"; the narrower rule holds here.
pub const SECRETS: &str = "secrets";

fn base_args() -> Args {
    BASE_VARS.iter().fold(Args::new(), |a, n| a.with(n, Value::Null))
}

/// The secret names an expression reads (`$secrets.name`, `$secrets["name"]`);
/// `None` when it reads them some other way (the whole map, a computed key).
pub fn secret_names(code: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut rest = code;
    while let Some(i) = rest.find("$secrets") {
        let after = &rest[i + "$secrets".len()..];
        if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            // `$secretsX` is another variable.
            rest = after;
            continue;
        }
        let name: String = if let Some(n) = after.strip_prefix('.') {
            n.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
        } else if let Some(n) = after.strip_prefix("[\"") {
            n.split('"').next().unwrap_or_default().to_string()
        } else {
            String::new()
        };
        if name.is_empty() {
            return None;
        }
        names.push(name);
        rest = after;
    }
    Some(names)
}

/// Checks what parsing cannot: expressions compile with the variables in
/// scope, calls resolve, flow directives target tasks of their own list,
/// named errors exist, value strings do not embed `${`, and the host offers
/// every capability the definition needs.
pub fn validate(wf: &Workflow, catalog: &dyn Catalog) -> Result<(), Invalid> {
    let mut v = Validator { wf, catalog, problems: Vec::new() };
    let args = base_args();
    if !wf.uses.secrets.is_empty() && !catalog.secrets() {
        v.problems.push(problem("/use/secrets", "secrets need a host secret resolver"));
    }
    if let Some(i) = &wf.input {
        v.input(i, "/input", &args);
    }
    if let Some(o) = &wf.output {
        v.output(o, "/output", &args);
    }
    for (name, f) in &wf.uses.functions {
        v.task(f, &format!("/use/functions/{name}"), &args, &[]);
    }
    for (i, (name, e)) in wf.uses.extensions.iter().enumerate() {
        let at = format!("/use/extensions/{i}/{name}");
        if let Some(w) = &e.when {
            v.expr(w, &format!("{at}/when"), &args);
        }
        for (key, list) in [("before", &e.before), ("after", &e.after)] {
            if let Some(list) = list {
                v.list(list, &format!("{at}/{key}"), &args);
            }
        }
    }
    if let Some(on) = wf.schedule.as_ref().and_then(|s| s.on.as_ref()) {
        v.strategy(on, "/schedule/on", &args);
    }
    if let Exceeded::Task(handler) = &wf.budget.on_exceeded {
        // Alone in its scope: it cannot jump back into the workflow's lists.
        v.task(handler, "/document/metadata/limits/on_exceeded", &args, &[]);
    }
    v.list(&wf.tasks, "/do", &args);
    if v.problems.is_empty() { Ok(()) } else { Err(Invalid(v.problems)) }
}

struct Validator<'a> {
    wf: &'a Workflow,
    catalog: &'a dyn Catalog,
    problems: Vec<Problem>,
}

impl Validator<'_> {
    fn fail(&mut self, at: &str, message: impl Into<String>) {
        self.problems.push(problem(at, message));
    }

    fn expr(&mut self, code: &str, at: &str, args: &Args) {
        if code.contains("$authorization") {
            self.fail(
                at,
                "`$authorization` describes a call's resolved authentication, which this runtime does not implement; functions carry their own credentials",
            );
            return;
        }
        if code.contains("$secrets") {
            if !args.names().any(|n| n == SECRETS) {
                self.fail(at, "`$secrets` is only available in `input.from`");
                return;
            }
            for name in secret_names(code).unwrap_or_default() {
                if !self.wf.uses.secrets.contains(&name) {
                    self.fail(at, format!("secret `{name}` is not declared under use.secrets"));
                }
            }
        }
        if let Err(e) = expr::check(code, args) {
            self.problems.push(problem(at, e));
        }
    }

    fn template(&mut self, t: &Value, at: &str, args: &Args) {
        let mut exprs = Vec::new();
        expr::template_exprs(t, &mut exprs);
        for e in exprs {
            self.expr(&e, at, args);
        }
        let mut embedded = Vec::new();
        expr::embedded_strings(t, &mut embedded);
        for s in embedded {
            self.problems.push(problem(at, format!("`{s}` embeds `${{` inside text; make the whole value one expression, e.g. \"${{ \\\"Hello \\(.name)\\\" }}\"")));
        }
    }

    fn transform(&mut self, t: Option<&Value>, at: &str, args: &Args) {
        match t {
            Some(Value::String(code)) => self.expr(code, at, args),
            Some(o) => self.template(o, at, args),
            None => {}
        }
    }

    fn schema(&mut self, s: Option<&Schema>, at: &str) {
        if let Some(Schema::Resource(uri)) = s
            && !self.catalog.resources()
        {
            self.fail(at, format!("external schema `{uri}` needs a host resource resolver"));
        }
    }

    /// `input`: its `from` also sees `$secrets`.
    fn input(&mut self, i: &Input, at: &str, args: &Args) {
        self.schema(i.schema.as_ref(), &format!("{at}/schema"));
        let with_secrets = args.clone().with(SECRETS, Value::Null);
        self.transform(i.from.as_ref(), &format!("{at}/from"), &with_secrets);
    }

    fn output(&mut self, o: &Output, at: &str, args: &Args) {
        self.schema(o.schema.as_ref(), &format!("{at}/schema"));
        self.transform(o.as_.as_ref(), &format!("{at}/as"), args);
    }

    fn flow(&mut self, f: &Option<Flow>, at: &str, siblings: &[&str]) {
        if let Some(Flow::Goto(name)) = f
            && !siblings.contains(&name.as_str())
        {
            self.problems.push(problem(
                at,
                format!("`then: {name}` must name a task in the same list (flow directives cannot leave their scope)"),
            ));
        }
    }

    fn list(&mut self, tasks: &TaskList, at: &str, args: &Args) {
        let names: Vec<&str> = tasks.iter().map(|(n, _)| n.as_str()).collect();
        for (i, (name, t)) in tasks.iter().enumerate() {
            self.task(t, &format!("{at}/{i}/{name}"), args, &names);
        }
        let exits = self.wf.uses.extensions.iter().any(|(_, e)| {
            [&e.before, &e.after].into_iter().flatten().flatten().any(|(_, t)| can_leave(t, &self.wf.uses.functions))
        });
        if !exits {
            for cycle in closed_cycles(tasks, &self.wf.uses.functions) {
                let (first, _) = &tasks[cycle[0]];
                let names: Vec<&str> = cycle.iter().map(|i| tasks[*i].0.as_str()).collect();
                self.fail(
                    &format!("{at}/{}/{first}", cycle[0]),
                    format!(
                        "the tasks {} jump in a cycle with no conditional way out (add a switch case or an `if` that leaves it)",
                        names.join(", ")
                    ),
                );
            }
        }
    }

    /// A loop or catch variable must not hide `$run`.
    fn variable(&mut self, name: &str, at: &str) {
        if name == RUN {
            self.fail(at, "`run` is reserved for the runtime argument `$run`");
        }
    }

    fn filter(&mut self, f: &EventFilter, at: &str, args: &Args) {
        for (attribute, value) in &f.with {
            if let Some(code) = value.as_str().and_then(expr::strip) {
                self.expr(code, &format!("{at}/with/{attribute}"), args);
            } else if value.as_str().is_some_and(expr::is_embedded) {
                self.fail(&format!("{at}/with/{attribute}"), "embeds `${` inside text; make it one expression");
            }
        }
        for (name, c) in &f.correlate {
            self.expr(&c.from, &format!("{at}/correlate/{name}/from"), args);
            if let Some(code) = c.expect.as_deref().and_then(expr::strip) {
                self.expr(code, &format!("{at}/correlate/{name}/expect"), args);
            }
        }
    }

    fn strategy(&mut self, s: &Strategy, at: &str, args: &Args) {
        match s {
            Strategy::One(f) => self.filter(f, &format!("{at}/one"), args),
            Strategy::All(filters) => {
                for (i, f) in filters.iter().enumerate() {
                    self.filter(f, &format!("{at}/all/{i}"), args);
                }
            }
            Strategy::Any { filters, until } => {
                for (i, f) in filters.iter().enumerate() {
                    self.filter(f, &format!("{at}/any/{i}"), args);
                }
                match until {
                    Some(Until::Condition(c)) => self.expr(c, &format!("{at}/until"), args),
                    Some(Until::Events(s)) => self.strategy(s, &format!("{at}/until"), args),
                    None => {}
                }
            }
        }
    }

    fn task(&mut self, t: &Task, at: &str, args: &Args, siblings: &[&str]) {
        if let Some(c) = &t.when {
            self.expr(expr::bare(c), &format!("{at}/if"), args);
        }
        if let Some(i) = &t.input {
            self.input(i, &format!("{at}/input"), args);
        }
        if let Some(o) = &t.output {
            self.output(o, &format!("{at}/output"), args);
        }
        if let Some(e) = &t.export {
            self.output(e, &format!("{at}/export"), args);
        }
        if let Some(DurationSpec::Expr(e)) = &t.timeout {
            self.expr(e, &format!("{at}/timeout"), args);
        }
        self.flow(&t.then, &format!("{at}/then"), siblings);
        match &t.kind {
            Kind::Call { function, with } => {
                let name = function_name(function);
                let local = self.wf.uses.functions.iter().any(|(n, _)| n == name);
                if local && t.delivery != crate::durable::Delivery::AtLeastOnce {
                    self.problems.push(problem(
                        &format!("{at}/metadata/delivery"),
                        "put delivery on the external call inside the reusable function",
                    ));
                }
                if !local && !self.catalog.has(name) {
                    self.problems.push(problem(&format!("{at}/call"), format!("unknown function `{function}`")));
                }
                if let Some(w) = with {
                    self.template(w, &format!("{at}/with"), args);
                }
            }
            Kind::Do(tasks) => self.list(tasks, &format!("{at}/do"), args),
            Kind::For { each, at: index, items, condition, tasks, .. } => {
                self.variable(each, &format!("{at}/for/each"));
                self.variable(index, &format!("{at}/for/at"));
                let inner = args.clone().with(each, Value::Null).with(index, Value::Null);
                if let Items::Expr(e) = items {
                    self.expr(e, &format!("{at}/for/in"), args);
                }
                if let Some(c) = condition {
                    self.expr(c, &format!("{at}/while"), &inner);
                }
                self.list(tasks, &format!("{at}/do"), &inner);
            }
            Kind::Fork { branches, .. } => {
                for (i, (name, b)) in branches.iter().enumerate() {
                    // A branch is alone in its scope: it cannot jump to a sibling branch.
                    self.task(b, &format!("{at}/fork/branches/{i}/{name}"), args, &[]);
                }
            }
            Kind::Set(s) => match s {
                Value::String(code) => self.expr(expr::bare(code), &format!("{at}/set"), args),
                other => self.template(other, &format!("{at}/set"), args),
            },
            Kind::Switch(cases) => {
                for (i, (name, c)) in cases.iter().enumerate() {
                    let cat = format!("{at}/switch/{i}/{name}");
                    if let Some(w) = &c.when {
                        self.expr(w, &format!("{cat}/when"), args);
                    }
                    self.flow(&Some(c.then.clone()), &format!("{cat}/then"), siblings);
                }
            }
            Kind::Try { tasks, catch } => {
                self.list(tasks, &format!("{at}/try"), args);
                self.variable(&catch.as_, &format!("{at}/catch/as"));
                let caught = args.clone().with(&catch.as_, Value::Null);
                for (code, key) in [(&catch.when, "when"), (&catch.except_when, "exceptWhen")] {
                    if let Some(c) = code {
                        self.expr(c, &format!("{at}/catch/{key}"), &caught);
                    }
                }
                if let Some(r) = &catch.retry {
                    for (code, key) in [(&r.when, "when"), (&r.except_when, "exceptWhen")] {
                        if let Some(c) = code {
                            self.expr(c, &format!("{at}/catch/retry/{key}"), &caught);
                        }
                    }
                    for d in [&r.delay, &r.attempt_duration, &r.duration].into_iter().flatten() {
                        if let DurationSpec::Expr(e) = d {
                            self.expr(e, &format!("{at}/catch/retry"), &caught);
                        }
                    }
                }
                if let Some(handler) = &catch.tasks {
                    self.list(handler, &format!("{at}/catch/do"), &caught);
                }
                self.flow(&catch.then, &format!("{at}/catch/then"), siblings);
            }
            Kind::Raise(r) => {
                let def = match r {
                    Raise::Inline(d) => Some(d),
                    Raise::Named(name) => {
                        let found = self.wf.uses.errors.iter().find(|(n, _)| n == name).map(|(_, d)| d);
                        if found.is_none() {
                            self.problems.push(problem(
                                &format!("{at}/raise/error"),
                                format!("no error `{name}` under use.errors"),
                            ));
                        }
                        found
                    }
                };
                for text in def.into_iter().flat_map(|d| [&d.title, &d.detail]).flatten() {
                    if let Some(code) = expr::strip(text) {
                        self.expr(code, &format!("{at}/raise/error"), args);
                    }
                }
            }
            Kind::Listen(listen) => {
                if !self.catalog.events() {
                    self.problems.push(problem(&format!("{at}/listen"), "events are not supported by this host"));
                }
                self.strategy(&listen.to, &format!("{at}/listen/to"), args);
                if let Some(f) = &listen.foreach {
                    self.variable(&f.item, &format!("{at}/foreach/item"));
                    self.variable(&f.at, &format!("{at}/foreach/at"));
                    let inner = args.clone().with(&f.item, Value::Null).with(&f.at, Value::Null);
                    self.list(&f.tasks, &format!("{at}/foreach/do"), &inner);
                    if let Some(o) = &f.output {
                        self.output(o, &format!("{at}/foreach/output"), &inner);
                    }
                    if let Some(e) = &f.export {
                        self.output(e, &format!("{at}/foreach/export"), &inner);
                    }
                }
            }
            Kind::Emit(with) => {
                if !self.catalog.emits() {
                    self.fail(&format!("{at}/emit"), "emitting events is not supported by this host");
                }
                self.template(&Value::Object(with.clone()), &format!("{at}/emit/event/with"), args);
            }
            Kind::Run(r) => match &r.process {
                Process::Workflow { input, .. } => {
                    if !self.catalog.children() {
                        self.fail(&format!("{at}/run/workflow"), "child workflows are not supported by this host");
                    }
                    if let Some(i) = input {
                        self.template(i, &format!("{at}/run/workflow/input"), args);
                    }
                }
                Process::External { kind, definition } => {
                    if !self.catalog.processes(kind) {
                        self.fail(
                            &format!("{at}/run/{kind}"),
                            format!("run: {kind} needs a host process runner, and this host has none for it"),
                        );
                    }
                    self.template(definition, &format!("{at}/run/{kind}"), args);
                }
            },
            Kind::Wait(d) => {
                if let DurationSpec::Expr(e) = d {
                    self.expr(e, &format!("{at}/wait"), args);
                }
            }
        }
    }
}

/// Where a task's flow may go next within its list: a sibling, or out of
/// the list (`exit`, `end`, past the last task, a raised error).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    Task(usize),
    Out,
}

/// Every way the task at `i` may continue in its list. The edges are
/// conditional when they lead to more than one place: an `if` (a skipped
/// task continues with the next one), a switch, a catch's own `then`.
fn successors(tasks: &TaskList, i: usize) -> Vec<Next> {
    let t = &tasks[i].1;
    let to = |f: &Flow| match f {
        Flow::Continue if i + 1 < tasks.len() => Next::Task(i + 1),
        Flow::Goto(name) => tasks.iter().position(|(n, _)| n == name).map_or(Next::Out, Next::Task),
        _ => Next::Out,
    };
    let own = t.then.as_ref().map_or_else(|| to(&Flow::Continue), to);
    let mut out = match &t.kind {
        Kind::Raise(_) => vec![Next::Out],
        Kind::Switch(cases) => {
            let mut out: Vec<Next> = cases.iter().map(|(_, c)| to(&c.then)).collect();
            if !cases.iter().any(|(_, c)| c.when.is_none()) {
                out.push(own);
            }
            out
        }
        Kind::Try { catch, .. } => vec![own, catch.then.as_ref().map_or(own, to)],
        _ => vec![own],
    };
    if t.when.is_some() {
        out.push(to(&Flow::Continue));
    }
    let mut distinct = Vec::new();
    for n in out {
        if !distinct.contains(&n) {
            distinct.push(n);
        }
    }
    distinct
}

/// Whether running the task can leave an enclosing cycle from inside: some
/// nested task raises or ends the workflow, or it calls a reusable function
/// (whose body is not followed). Permissive on purpose: the check rejects
/// only loops that can never end.
fn can_leave(t: &Task, functions: &[(String, Task)]) -> bool {
    let ends = |f: &Option<Flow>| matches!(f, Some(Flow::End));
    let list = |tasks: &TaskList| tasks.iter().any(|(_, t)| ends(&t.then) || can_leave(t, functions));
    match &t.kind {
        Kind::Raise(_) => true,
        Kind::Call { function, .. } => functions.iter().any(|(n, _)| n == function_name(function)),
        Kind::Do(tasks) | Kind::For { tasks, .. } => list(tasks),
        Kind::Fork { branches, .. } => list(branches),
        Kind::Switch(cases) => cases.iter().any(|(_, c)| c.then == Flow::End),
        Kind::Try { tasks, catch } => {
            list(tasks) || ends(&catch.then) || catch.tasks.as_ref().is_some_and(&list)
        }
        Kind::Listen(l) => l.foreach.as_ref().is_some_and(|f| list(&f.tasks)),
        Kind::Set(_) | Kind::Wait(_) | Kind::Emit(_) | Kind::Run(_) => false,
    }
}

/// The jump cycles of a list (its strongly connected components with an
/// edge back) that have no conditional edge out and no task that can leave
/// them from inside, each as its task indices in order. Such a loop can
/// never end; one with a way out is not proven to end either, which the
/// jump budget covers at runtime.
pub(crate) fn closed_cycles(tasks: &TaskList, functions: &[(String, Task)]) -> Vec<Vec<usize>> {
    let n = tasks.len();
    let edges: Vec<Vec<Next>> = (0..n).map(|i| successors(tasks, i)).collect();
    // reach[i][j]: j is reachable from i in one or more steps.
    let mut reach = vec![vec![false; n]; n];
    for (i, row) in reach.iter_mut().enumerate() {
        let mut stack: Vec<usize> =
            edges[i].iter().filter_map(|e| if let Next::Task(j) = e { Some(*j) } else { None }).collect();
        while let Some(j) = stack.pop() {
            if !row[j] {
                row[j] = true;
                stack.extend(edges[j].iter().filter_map(|e| if let Next::Task(k) = e { Some(*k) } else { None }));
            }
        }
    }
    let mut seen = vec![false; n];
    let mut out = Vec::new();
    for i in 0..n {
        if seen[i] || !reach[i][i] {
            continue;
        }
        let component: Vec<usize> = (0..n).filter(|j| *j == i || (reach[i][*j] && reach[*j][i])).collect();
        component.iter().for_each(|j| seen[*j] = true);
        let leaves = component.iter().any(|j| {
            can_leave(&tasks[*j].1, functions)
                || (edges[*j].len() > 1
                    && edges[*j].iter().any(|e| !matches!(e, Next::Task(k) if component.contains(k))))
        });
        if !leaves {
            out.push(component);
        }
    }
    out
}

// ── 4. static analysis ─────────────────────────────────────────────────────

/// One `call` in the definition: where it is and what it calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
    pub reference: String,
    pub function: String,
    /// The arguments as written: a template, or one expression.
    pub with: Option<Value>,
    pub delivery: crate::durable::Delivery,
}

/// Every task list in the definition with the reference prefix its tasks
/// get: the top level, reusable functions and extension lists included.
fn lists(wf: &Workflow, visit: &mut dyn FnMut(&Task, &str)) {
    fn list(tasks: &TaskList, at: &str, visit: &mut dyn FnMut(&Task, &str)) {
        for (i, (name, t)) in tasks.iter().enumerate() {
            task(t, &format!("{at}/{i}/{name}"), visit);
        }
    }
    fn task(t: &Task, at: &str, visit: &mut dyn FnMut(&Task, &str)) {
        visit(t, at);
        match &t.kind {
            Kind::Do(tasks) | Kind::For { tasks, .. } => list(tasks, &format!("{at}/do"), visit),
            Kind::Fork { branches, .. } => list(branches, &format!("{at}/fork/branches"), visit),
            Kind::Try { tasks, catch } => {
                list(tasks, &format!("{at}/try"), visit);
                if let Some(h) = &catch.tasks {
                    list(h, &format!("{at}/catch/do"), visit);
                }
            }
            Kind::Listen(l) => {
                if let Some(f) = &l.foreach {
                    list(&f.tasks, &format!("{at}/foreach/do"), visit);
                }
            }
            Kind::Set(_) | Kind::Switch(_) | Kind::Raise(_) | Kind::Wait(_) | Kind::Emit(_) | Kind::Run(_) => {}
            Kind::Call { .. } => {}
        }
    }
    for (name, f) in &wf.uses.functions {
        task(f, &format!("/use/functions/{name}"), visit);
    }
    for (i, (name, e)) in wf.uses.extensions.iter().enumerate() {
        for (key, l) in [("before", &e.before), ("after", &e.after)] {
            if let Some(l) = l {
                list(l, &format!("/use/extensions/{i}/{name}/{key}"), visit);
            }
        }
    }
    list(&wf.tasks, "/do", visit);
}

/// Every call site, including those inside reusable functions and
/// extensions, in definition order. Callers derive manifests and previews.
pub fn call_sites(wf: &Workflow) -> Vec<CallSite> {
    let mut out = Vec::new();
    lists(wf, &mut |t, at| {
        if let Kind::Call { function, with } = &t.kind {
            out.push(CallSite {
                reference: at.to_string(),
                function: function_name(function).to_string(),
                with: with.clone(),
                delivery: t.delivery,
            });
        }
    });
    out
}

/// Every external schema the definition refers to, for a host to resolve
/// and pin before admitting a run.
pub fn schema_resources(wf: &Workflow) -> Vec<String> {
    let mut out = Vec::new();
    let mut add = |s: Option<&Schema>| {
        if let Some(Schema::Resource(uri)) = s
            && !out.contains(uri)
        {
            out.push(uri.clone());
        }
    };
    let io = |i: &Option<Input>, o: &Option<Output>, e: &Option<Export>| {
        [
            i.as_ref().and_then(|i| i.schema.clone()),
            o.as_ref().and_then(|o| o.schema.clone()),
            e.as_ref().and_then(|e| e.schema.clone()),
        ]
    };
    for s in io(&wf.input, &wf.output, &None) {
        add(s.as_ref());
    }
    let mut found = Vec::new();
    lists(wf, &mut |t, _| {
        found.extend(io(&t.input, &t.output, &t.export));
        if let Kind::Listen(l) = &t.kind
            && let Some(f) = &l.foreach
        {
            found.extend(io(&None, &f.output, &f.export));
        }
    });
    for s in found {
        add(s.as_ref());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Names(&'static [&'static str]);
    impl Catalog for Names {
        fn has(&self, name: &str) -> bool {
            self.0.contains(&name)
        }
    }

    const HEAD: &str = "document: { dsl: '1.0.4', namespace: t, name: w, version: '0.1.0' }\n";

    fn wf(body: &str) -> Result<Workflow, Invalid> {
        parse_yaml(&format!("{HEAD}{body}"))
    }

    fn problems(body: &str, catalog: &[&'static str]) -> Vec<String> {
        let leaked: &'static [&'static str] = Box::leak(catalog.to_vec().into_boxed_slice());
        match wf(body) {
            Err(Invalid(p)) => p.iter().map(|p| p.to_string()).collect(),
            Ok(w) => match validate(&w, &Names(leaked)) {
                Ok(()) => Vec::new(),
                Err(Invalid(p)) => p.iter().map(|p| p.to_string()).collect(),
            },
        }
    }

    #[test]
    fn parses_every_supported_kind() {
        let w = wf(r#"
do:
  - a: { call: list_news, with: { since: '${ .s }' } }
  - b: { set: { x: 1 } }
  - c: { for: { each: n, in: '.items' }, do: [ { d: { set: '${ $n }' } } ] }
  - e: { switch: [ { hit: { when: '.x == 1', then: f } }, { other: { then: end } } ] }
  - f: { fork: { compete: true, branches: [ { g: { set: { y: 2 } } } ] } }
  - h: { try: [ { i: { call: list_news } } ], catch: { errors: { with: { status: 503 } }, retry: { delay: { seconds: 1 }, backoff: { exponential: {} }, limit: { attempt: { count: 3 } } } } }
  - j: { raise: { error: { type: 'https://x/e', status: 400, title: Bad } } }
  - k: { wait: PT1M30S }
  - l: { do: [ { m: { set: { z: 3 } } } ] }
"#)
        .unwrap();
        let kinds: Vec<&str> = w.tasks.iter().map(|(_, t)| t.kind.name()).collect();
        assert_eq!(kinds, ["call", "set", "for", "switch", "fork", "try", "raise", "wait", "do"]);
        assert!(matches!(&w.tasks[7].1.kind, Kind::Wait(DurationSpec::Fixed(d)) if d.as_secs() == 90));
        validate(&w, &Names(&["list_news"])).unwrap();
    }

    #[test]
    fn host_capabilities_and_unknown_constructs_are_checked() {
        // Constructs the host must provide fail validation on a bare host.
        let p = problems("do:\n  - a: { listen: { to: { one: { with: { type: x } } } } }\n", &[]);
        assert!(p[0].contains("/do/0/a/listen") && p[0].contains("not supported"), "{p:?}");
        let p = problems("do: [ { a: { emit: { event: { with: { type: x } } } } } ]\n", &[]);
        assert!(p[0].contains("/do/0/a/emit") && p[0].contains("not supported"), "{p:?}");
        let p = problems("do: [ { a: { run: { shell: { command: ls } } } } ]\n", &[]);
        assert!(p[0].contains("/do/0/a/run/shell") && p[0].contains("process runner"), "{p:?}");
        let p = problems("do: [ { a: { run: { workflow: { namespace: t, name: c, version: '1.0.0' } } } } ]\n", &[]);
        assert!(p[0].contains("/do/0/a/run/workflow"), "{p:?}");
        let p = problems("use: { secrets: [ token ] }\ndo: [ { a: { set: { x: 1 } } } ]\n", &[]);
        assert!(p.iter().any(|p| p.contains("/use/secrets") && p.contains("resolver")), "{p:?}");
        let p = problems(
            "do: [ { a: { set: { x: 1 }, input: { schema: { format: json, resource: { endpoint: 'https://x/s.json' } } } } } ]\n",
            &[],
        );
        assert!(p.iter().any(|p| p.contains("resource resolver")), "{p:?}");
        // A schedule is part of the definition.
        assert!(problems("schedule: { cron: '0 * * * *' }\ndo: [ { a: { set: { x: 1 } } } ]\n", &[]).is_empty());
        let p = problems("schedule: { cron: 'every day' }\ndo: [ { a: { set: { x: 1 } } } ]\n", &[]);
        assert!(p.iter().any(|p| p.contains("/schedule/cron")), "{p:?}");
        // Authentication and catalogs are not the core's.
        let p =
            problems("use: { catalogs: { c: { endpoint: 'https://x' } } }\ndo: [ { a: { set: { x: 1 } } } ]\n", &[]);
        assert!(p.iter().any(|p| p.contains("/use/catalogs") && p.contains("not supported")), "{p:?}");
        let p = problems("do: [ { a: { set: { x: 1 }, color: red } } ]\n", &[]);
        assert!(p.iter().any(|p| p.contains("/do/0/a/color") && p.contains("unknown property")), "{p:?}");
        let p = problems("do: [ { a: { frobnicate: 1 } } ]\n", &[]);
        assert!(p.iter().any(|p| p.contains("unknown task")), "{p:?}");
    }

    struct Everything;
    impl Catalog for Everything {
        fn has(&self, _: &str) -> bool {
            true
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
        fn processes(&self, _: &str) -> bool {
            true
        }
        fn secrets(&self) -> bool {
            true
        }
        fn resources(&self) -> bool {
            true
        }
    }

    fn checked(body: &str) -> Vec<String> {
        match wf(body) {
            Err(Invalid(p)) => p.iter().map(|p| p.to_string()).collect(),
            Ok(w) => validate(&w, &Everything)
                .err()
                .map(|Invalid(p)| p.iter().map(|p| p.to_string()).collect())
                .unwrap_or_default(),
        }
    }

    #[test]
    fn secrets_only_in_input_from_and_only_declared_ones() {
        let ok = "use: { secrets: [ api ] }\ndo: [ { a: { input: { from: '${ { key: $secrets.api.key } }' }, call: f, with: { k: '${ .key }' } } } ]\n";
        assert!(checked(ok).is_empty(), "{:?}", checked(ok));
        let p = checked("use: { secrets: [ api ] }\ndo: [ { a: { call: f, with: { k: '${ $secrets.api }' } } } ]\n");
        assert!(p[0].contains("only available in `input.from`"), "{p:?}");
        let p = checked(
            "use: { secrets: [ api ] }\ninput: { from: '${ $secrets.other }' }\ndo: [ { a: { set: { x: 1 } } } ]\n",
        );
        assert!(p[0].contains("`other` is not declared"), "{p:?}");
        let p = checked("do: [ { a: { set: { x: '${ $authorization.parameter }' } } } ]\n");
        assert!(p[0].contains("$authorization"), "{p:?}");
        assert_eq!(secret_names("$secrets.a + $secrets[\"b\"]"), Some(vec!["a".to_string(), "b".into()]));
        assert_eq!(secret_names("$secrets | keys"), None);
    }

    #[test]
    fn listen_emit_run_extensions_and_schedules_parse() {
        let w = wf(r#"
schedule: { on: { all: [ { with: { type: a }, correlate: { k: { from: .k } } }, { with: { type: b } } ] }, read: envelope }
use:
  extensions:
    - log: { extend: call, when: '$task.call == "f"', before: [ { b: { set: { x: 1 } } } ] }
do:
  - l:
      listen: { to: { any: [ { with: { type: a, data: '${ .n > 1 }' } } ], until: '${ length > 2 }' }, read: envelope }
      foreach: { item: e, do: [ { s: { set: '${ $e }' } } ], output: { as: '${ . }' } }
  - e: { emit: { event: { with: { type: done, source: 'https://x', data: '${ . }' } } } }
  - r: { run: { workflow: { namespace: t, name: child, version: '1.0.0', input: { n: '${ .n }' } }, await: false } }
  - c: { run: { container: { image: x }, return: all } }
"#)
        .unwrap();
        let s = w.schedule.as_ref().unwrap();
        assert!(matches!(&s.on, Some(Strategy::All(f)) if f.len() == 2 && f[0].correlate.len() == 1));
        assert_eq!(s.read, Read::Envelope);
        assert_eq!(w.uses.extensions[0].1.extend, "call");
        let kinds: Vec<&str> = w.tasks.iter().map(|(_, t)| t.kind.name()).collect();
        assert_eq!(kinds, ["listen", "emit", "run", "run"]);
        let Kind::Listen(l) = &w.tasks[0].1.kind else { panic!() };
        assert!(matches!(&l.to, Strategy::Any { until: Some(Until::Condition(c)), .. } if c == "length > 2"));
        assert_eq!(l.foreach.as_ref().map(|f| f.item.as_str()), Some("e"));
        let Kind::Run(r) = &w.tasks[3].1.kind else { panic!() };
        assert!(
            matches!(&r.process, Process::External { kind, .. } if kind == "container") && r.return_ == Return::All
        );
        assert!(validate(&w, &Everything).is_ok(), "{:?}", validate(&w, &Everything).err().map(|e| e.to_string()));
        for (body, expected) in [
            (
                "do: [ { a: { listen: { to: { one: { with: { type: x } }, until: '${ true }' } } } } ]\n",
                "until applies",
            ),
            ("do: [ { a: { emit: { event: { with: { source: s } } } } } ]\n", "needs a `type`"),
            ("do: [ { a: { run: { shell: { command: ls }, script: { code: x } } } } ]\n", "exactly one"),
            ("schedule: { read: data }\ndo: [ { a: { set: { x: 1 } } } ]\n", "read applies"),
            (
                "use: { extensions: [ { x: { extend: nothing } } ] }\ndo: [ { a: { set: { x: 1 } } } ]\n",
                "expected one of",
            ),
        ] {
            let p = checked(body);
            assert!(p.iter().any(|p| p.contains(expected)), "{body}: {p:?}");
        }
    }

    #[test]
    fn validation_reports_expressions_calls_and_flow() {
        let p = problems("do: [ { a: { call: nope } } ]\n", &["list_news"]);
        assert_eq!(p, ["/do/0/a/call: unknown function `nope`"]);
        let p = problems("do: [ { a: { set: { x: '${ .[ }' } } } ]\n", &[]);
        assert!(p[0].starts_with("/do/0/a/set: invalid jq"), "{p:?}");
        let p = problems("do: [ { a: { set: { x: '${ $nowhere }' } } } ]\n", &[]);
        assert!(p[0].contains("invalid jq"), "{p:?}");
        let p = problems("do: [ { a: { set: { x: 'Hi ${ .name }' } } } ]\n", &[]);
        assert!(p[0].contains("embeds"), "{p:?}");
        let p =
            problems("do: [ { a: { do: [ { b: { set: { x: 1 }, then: c } } ] } }, { c: { set: { y: 1 } } } ]\n", &[]);
        assert!(p[0].contains("same list"), "{p:?}");
        let p = problems("do: [ { a: { raise: { error: missing } } } ]\n", &[]);
        assert!(p[0].contains("use.errors"), "{p:?}");
    }

    #[test]
    fn loop_and_catch_variables_are_in_scope_only_inside() {
        assert!(
            problems(
                "do: [ { l: { for: { each: c, in: '.xs' }, do: [ { s: { set: '${ [$c, $index] }' } } ] } } ]\n",
                &[]
            )
            .is_empty()
        );
        let p = problems("do: [ { l: { for: { each: c, in: '.xs' }, do: [] } }, { s: { set: '${ $c }' } } ]\n", &[]);
        assert!(p[0].contains("/do/1/s"), "{p:?}");
        assert!(
            problems("do: [ { t: { try: [], catch: { as: err, do: [ { s: { set: '${ $err }' } } ] } } } ]\n", &[])
                .is_empty()
        );
    }

    #[test]
    fn default_catalog_names_and_call_sites() {
        assert_eq!(function_name("log:1.0.0@default"), "log");
        assert_eq!(function_name("log"), "log");
        assert_eq!(function_name("log:1.0.0@global"), "log:1.0.0@global");
        let w = wf("use: { functions: { wrap: { call: send } } }\ndo: [ { a: { call: list_news } }, { b: { for: { in: '.x' }, do: [ { c: { call: 'wrap' } } ] } } ]\n").unwrap();
        let sites: Vec<(String, String)> = call_sites(&w).into_iter().map(|c| (c.reference, c.function)).collect();
        assert_eq!(
            sites,
            [
                ("/use/functions/wrap".to_string(), "send".to_string()),
                ("/do/0/a".to_string(), "list_news".to_string()),
                ("/do/1/b/do/0/c".to_string(), "wrap".to_string()),
            ]
        );
    }

    #[test]
    fn named_retries_and_timeouts_resolve_in_nested_tasks() {
        let w = wf(r#"
use:
  retries: { thrice: { limit: { attempt: { count: 3 } } } }
  timeouts: { short: { after: PT5S } }
  functions: { guarded: { call: list_news, timeout: short } }
do:
  - outer:
      do:
        - t: { try: [ { a: { call: list_news, timeout: short } } ], catch: { retry: thrice } }
"#)
        .unwrap();
        let Kind::Do(inner) = &w.tasks[0].1.kind else { panic!() };
        let Kind::Try { tasks, catch } = &inner[0].1.kind else { panic!() };
        assert_eq!(catch.retry.as_ref().and_then(|r| r.attempts), Some(3));
        assert!(matches!(tasks[0].1.timeout, Some(DurationSpec::Fixed(d)) if d.as_secs() == 5));
        assert!(matches!(w.uses.functions[0].1.timeout, Some(DurationSpec::Fixed(d)) if d.as_secs() == 5));
        assert!(
            problems("do: [ { a: { set: { x: 1 }, timeout: nope } } ]\n", &[])
                .iter()
                .any(|p| p.contains("use.timeouts"))
        );
        let w =
            wf("do: [ { t: { try: [], catch: { retry: { limit: { attempt: { duration: PT1S } } } } } } ]\n").unwrap();
        let Kind::Try { catch, .. } = &w.tasks[0].1.kind else { panic!() };
        assert!(
            matches!(catch.retry.as_ref().and_then(|r| r.attempt_duration.clone()), Some(DurationSpec::Fixed(d)) if d.as_secs() == 1)
        );
    }

    #[test]
    fn jump_cycles_need_a_conditional_way_out() {
        let cycle = |p: &[String]| p.iter().any(|p| p.contains("jump in a cycle"));
        // Rejected: a self-loop, a two-task loop, a loop whose only branch
        // stays inside, a loop inside a try body.
        for body in [
            "do: [ { a: { set: { x: 1 }, then: a } } ]\n",
            "do: [ { a: { set: { x: 1 } } }, { b: { set: { x: 2 }, then: a } } ]\n",
            "do: [ { a: { set: { x: 1 } } }, { b: { switch: [ { x: { when: .x, then: a } }, { y: { then: a } } ] } } ]\n",
            "do: [ { t: { try: [ { a: { call: f, then: b } }, { b: { call: f, then: a } } ], catch: {} } } ]\n",
        ] {
            assert!(cycle(&checked(body)), "{body}: {:?}", checked(body));
        }
        // Accepted: a conditional exit (a switch case, an `if`, a default
        // that leaves, a catch's `then`), a nested raise or end, a forward
        // jump, a reusable function that may leave.
        for body in [
            "do: [ { a: { set: { x: 1 } } }, { b: { switch: [ { done: { when: .x, then: exit } }, { more: { then: a } } ] } } ]\n",
            "do: [ { a: { set: { x: 1 } } }, { b: { set: { x: 2 }, then: a, if: .more } } ]\n",
            "do: [ { a: { switch: [ { again: { when: .x, then: a } } ] } } ]\n",
            "do: [ { a: { try: [ { c: { call: f } } ], catch: { then: end }, then: a } } ]\n",
            "do: [ { a: { do: [ { r: { raise: { error: { type: x, status: 400 } } } } ], then: a } } ]\n",
            "do: [ { a: { do: [ { s: { switch: [ { stop: { when: .x, then: end } } ] } } ], then: a } } ]\n",
            "do: [ { a: { set: { x: 1 }, then: c } }, { b: { set: { x: 2 } } }, { c: { set: { x: 3 } } } ]\n",
            "use: { functions: { g: { call: f } } }\ndo: [ { a: { call: g, then: a } } ]\n",
        ] {
            assert!(checked(body).is_empty(), "{body}: {:?}", checked(body));
        }
        let p = checked("do: [ { a: { set: { x: 1 } } }, { b: { set: { x: 2 }, then: a } } ]\n");
        assert!(p[0].starts_with("/do/0/a:") && p[0].contains("a, b"), "{p:?}");
    }

    #[test]
    fn limits_parse_and_the_handler_validates_alone() {
        let w = wf("do: [ { a: { set: { x: 1 } } } ]\n").unwrap();
        assert!(w.budget.max_jumps.is_none() && matches!(w.budget.on_exceeded, Exceeded::Raise));
        assert_eq!(w.budget.max_jumps(100), 100);
        let limited = |limits: &str| {
            format!(
                "document: {{ dsl: '1.0.4', namespace: t, name: w, version: '0.1.0', metadata: {{ limits: {limits} }} }}\ndo: [ {{ a: {{ set: {{ x: 1 }} }} }} ]\n"
            )
        };
        let w = parse_yaml(&limited("{ max_jumps: 30, on_exceeded: end }")).unwrap();
        assert_eq!((w.budget.max_jumps, w.budget.max_jumps(10)), (Some(30), 10));
        assert!(matches!(w.budget.on_exceeded, Exceeded::End));
        let w = parse_yaml(&limited("{ on_exceeded: { call: report, with: { s: '${ . }' } } }")).unwrap();
        assert!(matches!(&w.budget.on_exceeded, Exceeded::Task(t) if matches!(t.kind, Kind::Call { .. })));
        assert!(validate(&w, &Names(&["report"])).is_ok());
        let p = validate(&w, &Names(&[])).unwrap_err().to_string();
        assert!(p.contains("/document/metadata/limits/on_exceeded/call"), "{p}");
        for (limits, expected) in [
            ("{ max_jumps: -1 }", "non-negative integer"),
            ("{ max_jumps: 3, every: 1 }", "unknown property"),
            ("{ on_exceeded: stop }", "expected `end` or a task"),
        ] {
            let p = parse_yaml(&limited(limits)).unwrap_err().to_string();
            assert!(p.contains(expected), "{limits}: {p}");
        }
        let w = parse_yaml(&limited("{ on_exceeded: { set: { x: 1 }, then: a } }")).unwrap();
        assert!(validate(&w, &Names(&[])).unwrap_err().to_string().contains("same list"));
        // `$run` is a runtime argument no loop or catch variable hides.
        assert!(checked("do: [ { a: { set: { v: '${ $run.visits.a }' } } } ]\n").is_empty());
        let p = checked("do: [ { l: { for: { each: run, in: '.xs' }, do: [] } } ]\n");
        assert!(p.iter().any(|p| p.contains("/do/0/l/for/each") && p.contains("reserved")), "{p:?}");
    }

    #[test]
    fn iso_durations() {
        assert_eq!(parse_iso_duration("PT30S"), Some(Duration::from_secs(30)));
        assert_eq!(parse_iso_duration("P1DT2H"), Some(Duration::from_secs(86_400 + 7_200)));
        assert_eq!(parse_iso_duration("PT0.5S"), Some(Duration::from_millis(500)));
        assert_eq!(parse_iso_duration("PT1M30S"), Some(Duration::from_secs(90)));
        assert_eq!(parse_iso_duration("P"), None);
        assert_eq!(parse_iso_duration("PT"), None);
        assert_eq!(parse_iso_duration("30S"), None);
        assert_eq!(parse_iso_duration("PT30X"), None);
        assert_eq!(parse_iso_duration("PT1S2M"), None);
    }
}

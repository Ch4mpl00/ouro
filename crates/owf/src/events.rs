// CloudEvents in the DSL: 1. the envelope an `emit` publishes and how a
// `listen` reads a consumed event; 2. event filters (attribute matching and
// correlation); 3. consumption strategies, fed one event at a time; 4. the
// routing types a host subscribes with.
//
// Matching is pure: the same events offered in the same order give the same
// decisions, so a durable listen replays its consumed events through a fresh
// consumer and reaches the state it had before a crash.
use crate::expr::{self, Args};
use crate::model::{EventFilter, Read, Strategy, Until};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

// ── 1. envelopes ───────────────────────────────────────────────────────────

/// The CloudEvents version every emitted event declares.
pub const SPEC_VERSION: &str = "1.0";

/// What the runtime fills in when the author leaves an attribute out.
pub struct Defaults {
    pub id: String,
    pub source: String,
    pub time: String,
}

/// The envelope of an emitted event from its rendered `emit.event.with`:
/// the author's attributes (extension attributes included) over the
/// runtime's `specversion`, `id`, `source` and `time`.
pub fn envelope(with: Map<String, Value>, defaults: Defaults) -> Result<Value, String> {
    let mut event = Map::new();
    event.insert("specversion".into(), json!(SPEC_VERSION));
    event.insert("id".into(), json!(defaults.id));
    event.insert("source".into(), json!(defaults.source));
    event.insert("time".into(), json!(defaults.time));
    for (k, v) in with {
        // An expression that produced nothing leaves the default in place.
        if !v.is_null() || k == "data" {
            event.insert(k, v);
        }
    }
    for k in ["id", "source", "type", "time"] {
        if event.get(k).and_then(Value::as_str).is_none_or(str::is_empty) {
            return Err(format!("the emitted event's `{k}` must be a non-empty string"));
        }
    }
    for k in ["subject", "datacontenttype", "dataschema"] {
        if event.get(k).is_some_and(|v| !v.is_string()) {
            return Err(format!("the emitted event's `{k}` must be a string"));
        }
    }
    Ok(Value::Object(event))
}

/// A consumed event as `listen.read` (or `schedule.read`) asks for it.
/// CHOICE: `raw` is the payload as delivered; with JSON payloads (all this
/// runtime carries) it is the same value as `data`.
pub fn read(event: &Value, read: Read) -> Value {
    match read {
        Read::Data | Read::Raw => event.get("data").cloned().unwrap_or(Value::Null),
        Read::Envelope => event.clone(),
    }
}

// ── 2. filters ─────────────────────────────────────────────────────────────

/// Whether `event` (an envelope) passes `filter`.
///
/// CHOICE (the reference says only "supports both regular expressions and
/// runtime expressions"): each `with` attribute is compared to the event's
/// attribute of the same name (`data` is the payload).
/// - A `${ }` expression is evaluated with the attribute as `.` and the
///   usual arguments (`$input` is the task's input). A boolean is the
///   decision; any other result must equal the attribute.
/// - A literal string matches an equal string, or one it matches entirely
///   as a regular expression.
/// - A literal object matches when each of its properties matches; other
///   literals must be equal.
///
/// Correlations then extract a value from the envelope (`from`, with the
/// envelope as `.`) and compare it with `expect` (a constant, or a `${ }`
/// expression on the task input) or, without one, with the first value this
/// correlation extracted; that first value is recorded in `expectations`
/// only when the whole filter matches.
pub fn matches(
    filter: &EventFilter,
    event: &Value,
    input: &Value,
    args: &Args,
    expectations: &mut BTreeMap<String, Value>,
) -> Result<bool, String> {
    for (attribute, expected) in &filter.with {
        let actual = event.get(attribute).unwrap_or(&Value::Null);
        if !attribute_matches(expected, actual, args)? {
            return Ok(false);
        }
    }
    let mut found = Vec::new();
    for (name, c) in &filter.correlate {
        let value = expr::eval(&c.from, event, args)?;
        let expected = match c.expect.as_deref() {
            Some(text) => Some(match expr::strip(text) {
                Some(code) => expr::eval(code, input, args)?,
                None => Value::String(text.to_owned()),
            }),
            None => expectations.get(name).cloned(),
        };
        match expected {
            Some(e) if !same(&e, &value) => return Ok(false),
            Some(_) => {}
            None => found.push((name.clone(), value)),
        }
    }
    expectations.extend(found);
    Ok(true)
}

fn attribute_matches(expected: &Value, actual: &Value, args: &Args) -> Result<bool, String> {
    Ok(match expected {
        Value::String(s) => match expr::strip(s) {
            Some(code) => match expr::eval(code, actual, args)? {
                Value::Bool(b) => b,
                other => &other == actual,
            },
            None => actual.as_str().is_some_and(|a| a == s || full_match(s, a)),
        },
        Value::Object(fields) => match actual {
            Value::Object(_) => {
                let mut all = true;
                for (k, v) in fields {
                    all &= attribute_matches(v, actual.get(k).unwrap_or(&Value::Null), args)?;
                }
                all
            }
            _ => false,
        },
        other => other == actual,
    })
}

/// A string constant and an extracted scalar compare by their text.
fn same(expected: &Value, value: &Value) -> bool {
    expected == value
        || match (expected, value) {
            (Value::String(e), Value::Number(_) | Value::Bool(_)) => e.as_str() == value.to_string().as_str(),
            _ => false,
        }
}

/// `text` matched entirely by the regular expression `pattern` (jq's).
fn full_match(pattern: &str, text: &str) -> bool {
    let args = Args::new().with("p", Value::String(format!("^(?:{pattern})$")));
    expr::eval("test($p)", &Value::String(text.to_owned()), &args) == Ok(Value::Bool(true))
}

// ── 3. consumption strategies ──────────────────────────────────────────────

/// What a consumer did with an offered event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// No filter wanted it: it is dropped (it was taken all the same).
    Ignored,
    /// It belongs to the listen's output.
    Consumed,
    /// An `until` strategy took it: it counts towards stopping, not output.
    Until,
}

/// A strategy's progress over the events offered so far.
///
/// CHOICE where the reference is silent: `one` takes the first match;
/// `any` takes every match until its `until` (without one, the first match
/// ends it); `all` fills each filter with the first event that matches it
/// (in filter order), and events matching only filled filters are dropped.
/// Correlation expectations are shared by all filters of one listen.
pub struct Consumer<'s> {
    strategy: &'s Strategy,
    filled: Vec<bool>,
    expectations: BTreeMap<String, Value>,
    until: Option<Box<Consumer<'s>>>,
    done: bool,
}

impl<'s> Consumer<'s> {
    pub fn new(strategy: &'s Strategy) -> Self {
        let (filled, until) = match strategy {
            Strategy::All(filters) => (vec![false; filters.len()], None),
            Strategy::Any { until: Some(Until::Events(s)), .. } => (Vec::new(), Some(Box::new(Consumer::new(s)))),
            _ => (Vec::new(), None),
        };
        Self { strategy, filled, expectations: BTreeMap::new(), until, done: false }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    /// Ends the listen: its `until` condition held.
    pub fn stop(&mut self) {
        self.done = true;
    }

    pub fn offer(&mut self, event: &Value, input: &Value, args: &Args) -> Result<Offer, String> {
        if self.done {
            return Ok(Offer::Ignored);
        }
        match self.strategy {
            Strategy::One(f) => {
                if matches(f, event, input, args, &mut self.expectations)? {
                    self.done = true;
                    return Ok(Offer::Consumed);
                }
            }
            Strategy::All(filters) => {
                for (i, f) in filters.iter().enumerate() {
                    if !self.filled[i] && matches(f, event, input, args, &mut self.expectations)? {
                        self.filled[i] = true;
                        self.done = self.filled.iter().all(|f| *f);
                        return Ok(Offer::Consumed);
                    }
                }
            }
            Strategy::Any { filters, until } => {
                if let Some(u) = &mut self.until
                    && u.offer(event, input, args)? != Offer::Ignored
                {
                    self.done = u.done();
                    return Ok(Offer::Until);
                }
                let mut hit = filters.is_empty();
                for f in filters {
                    if hit {
                        break;
                    }
                    hit = matches(f, event, input, args, &mut self.expectations)?;
                }
                if hit {
                    self.done = until.is_none();
                    return Ok(Offer::Consumed);
                }
            }
        }
        Ok(Offer::Ignored)
    }
}

// ── 4. routing ─────────────────────────────────────────────────────────────

/// The event types a host routes to a listen: each filter's literal
/// `type`, or `*` (every published event) when a filter has none or makes
/// it a regular expression (`.` alone stays literal: types are dotted
/// names). A `type` expression that yields a string without reading the
/// attribute (`${ $context.done }`, evaluated on null) routes on that
/// string; one that reads it is a predicate and routes `*`. Routing only
/// narrows what is offered; `matches` decides.
pub fn routes(strategy: &Strategy, args: &Args) -> Vec<String> {
    fn filters<'a>(s: &'a Strategy, out: &mut Vec<&'a EventFilter>) -> bool {
        match s {
            Strategy::One(f) => out.push(f),
            Strategy::All(fs) => out.extend(fs),
            Strategy::Any { filters: fs, until } => {
                if fs.is_empty() {
                    return false;
                }
                out.extend(fs);
                if let Some(Until::Events(u)) = until
                    && !filters(u, out)
                {
                    return false;
                }
            }
        }
        true
    }
    let mut all = Vec::new();
    if !filters(strategy, &mut all) {
        return vec!["*".into()];
    }
    let mut out: Vec<String> = Vec::new();
    for f in all {
        let route = match f.with.get("type").and_then(Value::as_str) {
            Some(t) => match expr::strip(t) {
                Some(code) => match expr::eval(code, &Value::Null, args) {
                    Ok(Value::String(t)) if !t.is_empty() => Some(t),
                    _ => None,
                },
                None if !t.contains(|c| "\\^$*+?()[]{}|".contains(c)) => Some(t.to_owned()),
                None => None,
            },
            None => None,
        };
        match route {
            Some(t) if !out.contains(&t) => out.push(t),
            Some(_) => {}
            None => return vec!["*".into()],
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Correlation;

    fn filter(with: Value) -> EventFilter {
        EventFilter { with: with.as_object().unwrap().clone(), correlate: Vec::new() }
    }

    fn event(t: &str, data: Value) -> Value {
        json!({ "specversion": "1.0", "id": "1", "source": "/s", "type": t, "data": data, "roomid": "r1" })
    }

    #[test]
    fn literals_regexes_expressions_and_objects() {
        let args = Args::new().with("input", json!({"limit": 38}));
        let mut e = BTreeMap::new();
        let hot = event("com.x.temperature", json!({"temperature": 39}));
        let cases = [
            (json!({"type": "com.x.temperature"}), true),
            (json!({"type": "com\\.x\\..*"}), true),
            (json!({"type": "com.x.bpm"}), false),
            (json!({"data": "${ .temperature > $input.limit }"}), true),
            (json!({"data": "${ .temperature < 10 }"}), false),
            (json!({"type": "${ \"com.x.temperature\" }"}), true),
            (json!({"data": {"temperature": 39}}), true),
            (json!({"data": {"temperature": 40}}), false),
            (json!({"source": "/s", "type": "com.x.temperature"}), true),
            (json!({"subject": "absent"}), false),
        ];
        for (with, expected) in cases {
            assert_eq!(matches(&filter(with.clone()), &hot, &Value::Null, &args, &mut e), Ok(expected), "{with}");
        }
    }

    #[test]
    fn correlation_takes_the_first_value_or_an_expectation() {
        let mut f = filter(json!({"type": "reading"}));
        f.correlate.push(("room".into(), Correlation { from: ".roomid".into(), expect: None }));
        let args = Args::new();
        let mut e = BTreeMap::new();
        let mut other = event("reading", json!(1));
        other["roomid"] = json!("r2");
        assert_eq!(matches(&f, &event("reading", json!(1)), &Value::Null, &args, &mut e), Ok(true));
        assert_eq!(matches(&f, &other, &Value::Null, &args, &mut e), Ok(false));
        f.correlate[0].1.expect = Some("${ .room }".into());
        assert_eq!(matches(&f, &other, &json!({"room": "r2"}), &args, &mut BTreeMap::new()), Ok(true));
        f.correlate[0].1.expect = Some("r9".into());
        assert_eq!(matches(&f, &other, &Value::Null, &args, &mut BTreeMap::new()), Ok(false));
    }

    #[test]
    fn strategies_and_routes() {
        let args = Args::new();
        let all = Strategy::All(vec![filter(json!({"type": "a"})), filter(json!({"type": "b"}))]);
        let mut c = Consumer::new(&all);
        let offers: Vec<Offer> = ["a", "a", "c", "b"]
            .iter()
            .map(|t| c.offer(&event(t, Value::Null), &Value::Null, &args).unwrap())
            .collect();
        assert_eq!(offers, [Offer::Consumed, Offer::Ignored, Offer::Ignored, Offer::Consumed]);
        assert!(c.done());
        assert_eq!(routes(&all, &args), ["a", "b"]);

        let until = Strategy::Any {
            filters: vec![filter(json!({"type": "a"}))],
            until: Some(Until::Events(Box::new(Strategy::One(filter(json!({"type": "stop"})))))),
        };
        let mut c = Consumer::new(&until);
        for (t, offer) in
            [("a", Offer::Consumed), ("x", Offer::Ignored), ("a", Offer::Consumed), ("stop", Offer::Until)]
        {
            assert_eq!(c.offer(&event(t, Value::Null), &Value::Null, &args), Ok(offer));
        }
        assert!(c.done());
        assert_eq!(routes(&until, &args), ["a", "stop"]);
        let any = Strategy::Any { filters: Vec::new(), until: None };
        assert_eq!(routes(&any, &args), ["*"]);
        assert_eq!(routes(&Strategy::One(filter(json!({"type": "a.*"}))), &args), ["*"]);
        assert_eq!(routes(&Strategy::One(filter(json!({"source": "/s"}))), &args), ["*"]);
        let ctx = Args::new().with("context", json!({"done": "done:c1"}));
        assert_eq!(routes(&Strategy::One(filter(json!({"type": "${ $context.done }"}))), &ctx), ["done:c1"]);
        assert_eq!(routes(&Strategy::One(filter(json!({"type": "${ startswith(\"a\") }"}))), &ctx), ["*"]);
    }

    #[test]
    fn emitted_envelopes_take_the_authors_attributes_over_the_defaults() {
        let defaults = || Defaults { id: "id-1".into(), source: "/ns/w/1".into(), time: "2026-01-01T00:00:00Z".into() };
        let e = envelope(
            json!({"type": "t", "data": {"x": 1}, "source": "https://a"}).as_object().unwrap().clone(),
            defaults(),
        )
        .unwrap();
        assert_eq!(
            e,
            json!({"specversion": "1.0", "id": "id-1", "source": "https://a", "time": "2026-01-01T00:00:00Z", "type": "t", "data": {"x": 1}})
        );
        assert!(envelope(json!({"type": 3}).as_object().unwrap().clone(), defaults()).is_err());
        assert_eq!(read(&e, Read::Data), json!({"x": 1}));
        assert_eq!(read(&e, Read::Envelope), e);
    }
}

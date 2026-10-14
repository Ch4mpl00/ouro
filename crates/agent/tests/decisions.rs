// Port of src/decisions.test.ts and test/decisions-filter.test.ts.
use agent::{
    decisions::*,
    providers::{JsonApi, RetryOpts},
    tracing::null_scope,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
#[test]
fn choice_score_noul_are_typed_and_reject_malformed_answers() {
    let choice = choice("pick", json!({"a":"Alpha","b":"Beta"}).as_object().unwrap().clone()).unwrap();
    assert_eq!(choice.wire, json!({"type":"choice","instructions":"pick","criteria":{"a":"Alpha","b":"Beta"}}));
    let selected = choice
        .parse(&json!({"type":"choice","choice":"a","confidence":0.7,"probabilities":{"a":0.7,"b":0.3,"extra":0.0}}))
        .unwrap();
    assert_eq!(selected.choice, "a");
    assert_eq!(selected.confidence, 0.7);
    assert_eq!(selected.probabilities.len(), 2);
    assert!(choice.parse(&json!({"type":"choice","choice":"unknown","confidence":0.7,"probabilities":{}})).is_err());
    let score = score("rate", vec!["low".into(), "mid".into(), "high".into()]).unwrap();
    let rated = score
        .parse(&json!({"type":"score","score":1.5,"confidence":0.99,"probabilities":{"0":0.1,"1":0.2,"2":0.7}}))
        .unwrap();
    assert_eq!(rated.level, "high");
    assert_eq!(rated.score, 1.5);
    assert_eq!(rated.confidence, 0.7);
    assert_eq!(rated.probabilities, [0.1, 0.2, 0.7]);
    assert_eq!(
        score.parse(&json!({"type":"score","score":0.0,"confidence":0.9,"probabilities":{}})).unwrap().level,
        "low"
    );
    let yes = noul("passes", Some(("yes", "no")));
    assert_eq!(yes.wire["criteria"], json!({"true":"yes","false":"no"}));
    assert_eq!(yes.parse(&json!({"type":"noul","noul":0.3})).unwrap().probability, 0.3);
    for p in [-0.1, 1.1] {
        assert!(yes.parse(&json!({"type":"noul","noul":p})).is_err());
    }
    assert!(yes.parse(&json!({"type":"choice","noul":0.5})).is_err());
    assert!(agent::decisions::choice("pick", json!({"only":"one"}).as_object().unwrap().clone()).is_err());
}
struct Api {
    seen: Mutex<Vec<Value>>,
    malformed: bool,
}
#[async_trait]
impl JsonApi for Api {
    async fn post(&self, path: &str, body: Value, _: &CancellationToken) -> anyhow::Result<Value> {
        assert_eq!(path, OPENROUTER_DECISIONS_URL);
        self.seen.lock().unwrap().push(body.clone());
        let answers: serde_json::Map<_, _> = body["questions"]
            .as_object()
            .unwrap()
            .keys()
            .enumerate()
            .map(|(i, key)| {
                (key.clone(), json!({"type":"noul","noul":if self.malformed {1.1} else if i%2==0 {0.3} else {0.2}}))
            })
            .collect();
        Ok(
            json!({"model":"typesafe/dated","id":"request","answers":answers,"usage":{"input_tokens":10,"output_tokens":2,"cost":0.001}}),
        )
    }
}
fn api(malformed: bool) -> Arc<Api> {
    Arc::new(Api { seen: Default::default(), malformed })
}
#[tokio::test]
async fn client_sends_typed_questions_validates_every_answer_and_reports_usage() {
    let api = api(false);
    let client = decision_client("jev".into(), api.clone());
    let question = noul("matches", None);
    let decision = client
        .decide(
            DecisionRequest::new(json!({"text":"input"}), null_scope(), CancellationToken::new())
                .question("keep", &question),
        )
        .await
        .unwrap();
    assert_eq!(decision.answer("keep", &question).unwrap().probability, 0.3);
    assert_eq!(decision.model, "typesafe/dated");
    assert_eq!(decision.id.as_deref(), Some("request"));
    assert_eq!(decision.usage.unwrap().total, 12);
    assert_eq!(decision.cost_usd, Some(0.001));
    assert_eq!(api.seen.lock().unwrap()[0]["questions"]["keep"], question.wire);
    let bad =
        DecisionClient::with_retry("jev".into(), self::api(true), RetryOpts { base_delay_ms: 0, ..Default::default() });
    assert!(
        bad.decide(
            DecisionRequest::new(json!("input"), null_scope(), CancellationToken::new()).question("keep", &question)
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("answer \"keep\"")
    );
}
#[tokio::test]
async fn filter_packs_batches_keeps_input_order_and_inclusive_threshold() {
    let api = api(false);
    let client = decision_client("jev".into(), api.clone());
    let items: Vec<_> = (0..60).map(|i| json!({"id":i,"title":"Новости 🦊"})).collect();
    let result = filter(&client, &items, "rules", None, &null_scope(), &CancellationToken::new()).await.unwrap();
    assert_eq!(api.seen.lock().unwrap().len(), 3);
    assert_eq!(result.probabilities.len(), 60);
    assert_eq!(
        result.kept,
        items.iter().enumerate().filter(|(i, _)| (i % 25) % 2 == 0).map(|(_, item)| item.clone()).collect::<Vec<_>>()
    );
    assert!(result.kept.windows(2).all(|w| w[0]["id"].as_i64() < w[1]["id"].as_i64()));
    let none = filter(&client, &items, "rules", Some(0.31), &null_scope(), &CancellationToken::new()).await.unwrap();
    assert!(none.kept.is_empty());
    let before = api.seen.lock().unwrap().len();
    assert!(
        filter(&client, &[], "rules", None, &null_scope(), &CancellationToken::new()).await.unwrap().kept.is_empty()
    );
    assert_eq!(api.seen.lock().unwrap().len(), before);
}
#[test]
fn batching_counts_utf16_and_gives_oversized_items_their_own_batch() {
    let items = vec![json!("🦊".repeat(6000)), json!("x".repeat(12000)), json!("big".repeat(9000)), json!("end")];
    assert_eq!(pack_filter_batches(&items), [vec![0], vec![1], vec![2], vec![3]]);
    let ascii = vec![json!("x".repeat(11998)), json!("x".repeat(11998))];
    assert_eq!(pack_filter_batches(&ascii), [vec![0, 1]]);
}
#[tokio::test]
async fn malformed_probability_fails_the_whole_filter() {
    let client = decision_client("jev".into(), api(true));
    assert!(filter(&client, &[json!("a")], "rules", None, &null_scope(), &CancellationToken::new()).await.is_err());
}

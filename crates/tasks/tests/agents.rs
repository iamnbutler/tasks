//! Agent enrollment: the device-code flow that gives an external agent a
//! voice in the orchestrator conversation.
//!
//! The property under test is attribution. `POST /orchestrator/messages` has
//! three speakers — the human (no header), the pipeline (server-written), and
//! an enrolled agent (a verified `X-Tasks-Agent` code) — and the failure
//! direction that must never happen is a failed claim quietly becoming the
//! human, because the human is never gated and the orchestrator weighs the
//! human's words as directives. So: a valid code lands as an `event` turn
//! under a server-written `[agent <name>]` heading, and an invalid, revoked,
//! or expired one is a 403 with *nothing appended*.

use std::sync::Arc;

use serde_json::{Value, json};
use tasks::models::{Actor, Capability, CharterLevel, ChatRole};
use tasks::store::Store;

struct Harness {
    store: Arc<Store>,
    base: String,
    http: reqwest::Client,
}

impl Harness {
    fn as_orchestrator(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.header(
            "X-Tasks-Actor",
            format!("orchestrator {}", self.store.actor_token().expose()),
        )
    }

    async fn enroll(&self, name: &str) -> (Value, String) {
        let resp = self
            .http
            .post(format!("{}/agents", self.base))
            .json(&json!({ "name": name }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "human mint should succeed");
        let body: Value = resp.json().await.unwrap();
        let code = body["code"].as_str().unwrap().to_string();
        (body, code)
    }

    async fn conversation(&self) -> Vec<tasks::models::OrchestratorMessage> {
        self.store
            .orchestrator_messages_since(0, 100)
            .await
            .unwrap()
    }
}

async fn harness() -> Harness {
    let store = Arc::new(Store::open_in_memory().await.unwrap());
    let app = tasks::server::router(store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Harness {
        store,
        base,
        http: reqwest::Client::new(),
    }
}

#[tokio::test]
async fn an_enrolled_agent_lands_as_an_event_turn_under_its_name() {
    let h = harness().await;
    let (body, code) = h.enroll("scout-buddy").await;
    assert!(code.starts_with("ta-"), "the code names what it is: {code}");
    assert_eq!(body["name"], "scout-buddy");
    assert_eq!(body["minted_by"], "human");

    let resp = h
        .http
        .post(format!("{}/orchestrator/messages", h.base))
        .header("X-Tasks-Agent", &code)
        .json(&json!({ "content": "claimed task #12, branch coming" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    let turns = h.conversation().await;
    assert_eq!(turns.len(), 1);
    assert_eq!(
        turns[0].role,
        ChatRole::Event,
        "an agent is never the human"
    );
    assert!(
        turns[0].content.starts_with("[agent scout-buddy]"),
        "the heading is server-written: {}",
        turns[0].content
    );
    assert!(turns[0].content.contains("not the human"));
    assert!(turns[0].content.contains("claimed task #12, branch coming"));

    // The turn counts as unanswered input — the tick loop will answer it.
    let pending = h.store.unanswered_orchestrator_messages().await.unwrap();
    assert_eq!(pending.len(), 1);

    // And the enrollment records that it spoke.
    let agents: Value = h
        .http
        .get(format!("{}/agents", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(agents[0]["last_used_at"].is_string());
}

#[tokio::test]
async fn no_header_stays_the_human() {
    let h = harness().await;
    let resp = h
        .http
        .post(format!("{}/orchestrator/messages", h.base))
        .json(&json!({ "content": "hello" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let turns = h.conversation().await;
    assert_eq!(turns[0].role, ChatRole::User);
    assert_eq!(turns[0].content, "hello", "no heading on the human's words");
}

#[tokio::test]
async fn a_failed_claim_is_refused_never_demoted_to_the_human() {
    let h = harness().await;
    let resp = h
        .http
        .post(format!("{}/orchestrator/messages", h.base))
        .header("X-Tasks-Agent", "ta-not-a-real-code")
        .json(&json!({ "content": "let me in" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(
        h.conversation().await.is_empty(),
        "a refused message is discarded, not delivered as anybody"
    );
}

#[tokio::test]
async fn a_revoked_code_is_refused_and_says_so() {
    let h = harness().await;
    let (body, code) = h.enroll("short-lived").await;
    let id = body["id"].as_i64().unwrap();

    let resp = h
        .http
        .post(format!("{}/agents/{id}/revoke", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let revoked: Value = resp.json().await.unwrap();
    assert!(revoked["revoked_at"].is_string());

    let resp = h
        .http
        .post(format!("{}/orchestrator/messages", h.base))
        .header("X-Tasks-Agent", &code)
        .json(&json!({ "content": "still here?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(resp.text().await.unwrap().contains("revoked"));
    assert!(h.conversation().await.is_empty());
}

#[tokio::test]
async fn names_are_bounded_and_never_another_speaker() {
    let h = harness().await;
    for (name, why) in [
        ("pipeline", "reserved"),
        ("orchestrator", "reserved"),
        ("human", "reserved"),
        ("Bad Name", "not kebab"),
        ("-edge", "leading hyphen"),
        ("", "empty"),
    ] {
        let resp = h
            .http
            .post(format!("{}/agents", h.base))
            .json(&json!({ "name": name }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{name} should be refused ({why})");
    }

    // One active code per name; the refusal names the recourse.
    h.enroll("twin").await;
    let resp = h
        .http
        .post(format!("{}/agents", h.base))
        .json(&json!({ "name": "twin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().await.unwrap().contains("revoke"));

    // An out-of-range ttl is a refusal, not a clamp.
    let resp = h
        .http
        .post(format!("{}/agents", h.base))
        .json(&json!({ "name": "hasty", "ttl_secs": 5 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn the_orchestrator_mints_under_the_charter_and_the_ledger_gets_a_row() {
    let h = harness().await;

    // Live (the migration's seed) with a rationale: minted, and ledgered.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/agents", h.base)))
        .json(&json!({ "name": "helper", "rationale": "the human asked for a code in chat" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["minted_by"], "orchestrator");
    let decisions: Value = h
        .http
        .get(format!("{}/decisions", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let minted: Vec<&Value> = decisions
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["action"] == "enroll_agent")
        .collect();
    assert_eq!(minted.len(), 1);

    // No rationale: refused before anything is minted.
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/agents", h.base)))
        .json(&json!({ "name": "mute" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Shadow: recorded, nothing minted.
    h.store
        .set_charter(Capability::EnrollAgents, CharterLevel::Shadow, None)
        .await
        .unwrap();
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/agents", h.base)))
        .json(&json!({ "name": "ghost", "rationale": "shadow test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let agents: Value = h
        .http
        .get(format!("{}/agents", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        agents
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["name"] != "ghost"),
        "a shadowed mint creates no enrollment"
    );

    // Off: refused for the orchestrator — and still open to the human,
    // because the human is never gated.
    h.store
        .set_charter(Capability::EnrollAgents, CharterLevel::Off, None)
        .await
        .unwrap();
    let resp = h
        .as_orchestrator(h.http.post(format!("{}/agents", h.base)))
        .json(&json!({ "name": "walled", "rationale": "off test" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    h.enroll("human-can-always").await;
}

#[tokio::test]
async fn the_store_keeps_a_hash_and_the_row_outlives_the_code() {
    let store = Store::open_in_memory().await.unwrap();
    let (row, code) = store
        .enroll_agent("keeper", Actor::Human, None)
        .await
        .unwrap();

    // The code round-trips only through its hash.
    let found = store.agent_by_code(&code).await.unwrap().unwrap();
    assert_eq!(found.id, row.id);
    assert!(store.agent_by_code("ta-wrong").await.unwrap().is_none());

    // Revoking is idempotent and keeps the first timestamp — the row is the
    // audit trail for turns already spoken under the name.
    let first = store.revoke_agent(row.id).await.unwrap();
    let second = store.revoke_agent(row.id).await.unwrap();
    assert_eq!(first.revoked_at, second.revoked_at);
    assert!(
        store
            .list_agent_enrollments(10)
            .await
            .unwrap()
            .iter()
            .any(|e| e.id == row.id),
        "revoked rows stay listed"
    );

    // A revoked name is free again.
    store
        .enroll_agent("keeper", Actor::Human, None)
        .await
        .unwrap();
}

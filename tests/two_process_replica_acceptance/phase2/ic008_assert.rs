use super::*;

fn fingerprint(value: &serde_json::Value) {
    let value = value.as_str().expect("IC-008 fingerprint string");
    let digest = value
        .strip_prefix("sha256:")
        .expect("IC-008 SHA-256 fingerprint prefix");
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
}

pub(super) fn execution_identity(started: &serde_json::Value) {
    let incarnation = started["incarnation_id"]
        .as_str()
        .expect("IC-008 incarnation id");
    let parsed = uuid::Uuid::parse_str(incarnation).expect("IC-008 UUID incarnation");
    assert!(!parsed.is_nil());
    assert_eq!(parsed.hyphenated().to_string(), incarnation);
    fingerprint(&started["source_fingerprint"]);
    fingerprint(&started["definition_fingerprint"]);
}

pub(super) fn message_identity(message: &serde_json::Value, started: &serde_json::Value) {
    assert_eq!(message["incarnation_id"], started["incarnation_id"]);
    assert_eq!(
        message["definition_fingerprint"],
        started["definition_fingerprint"]
    );
    assert!(
        message["revision"]
            .as_u64()
            .expect("IC-008 message revision")
            > started["revision"].as_u64().expect("IC-008 start revision")
    );
}

pub(super) fn history(
    history: &serde_json::Value,
    started: &serde_json::Value,
    latest: &serde_json::Value,
    conversation_id: &str,
    turns: u64,
    expected_contents: &[&str],
) {
    assert_eq!(history["conversation_id"], conversation_id);
    assert_eq!(history["flow"], FLOW);
    assert_eq!(history["agent"], "coordinator");
    assert_eq!(history["turn_count"], turns);
    assert_eq!(history["revision"], latest["revision"]);
    assert_eq!(history["incarnation_id"], started["incarnation_id"]);
    assert_eq!(history["source_fingerprint"], started["source_fingerprint"]);
    assert_eq!(
        history["definition_fingerprint"],
        started["definition_fingerprint"]
    );
    let messages = history["messages"]
        .as_array()
        .expect("IC-008 durable history messages");
    for content in expected_contents {
        assert!(
            messages
                .iter()
                .any(|message| message["content"] == *content),
            "IC-008 history omitted {content:?}"
        );
    }
}

pub(super) async fn deleted(response: Response, conversation_id: &str) {
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("parse IC-008 delete");
    assert_eq!(body["deleted"], conversation_id);
}

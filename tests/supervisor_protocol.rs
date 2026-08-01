use serde_json::Value;
use std::collections::BTreeSet;

const SUCCESS_REQUEST: &str =
    include_str!("../protocol/fixtures/supervisor-v1/request-success.json");
const MALFORMED_REQUEST: &str =
    include_str!("../protocol/fixtures/supervisor-v1/request-malformed.json");
const UNSUPPORTED_REQUEST: &str =
    include_str!("../protocol/fixtures/supervisor-v1/request-unsupported-version.json");

const SUCCESS_RESPONSES: &[(&str, &str)] = &[
    (
        include_str!("../protocol/fixtures/supervisor-v1/response-success.json"),
        "completed",
    ),
    (
        include_str!("../protocol/fixtures/supervisor-v1/response-timeout.json"),
        "timed_out",
    ),
    (
        include_str!("../protocol/fixtures/supervisor-v1/response-cancellation.json"),
        "cancelled",
    ),
    (
        include_str!("../protocol/fixtures/supervisor-v1/response-truncated-output.json"),
        "completed",
    ),
    (
        include_str!("../protocol/fixtures/supervisor-v1/response-start-failure.json"),
        "failed_to_start",
    ),
];

const ERROR_RESPONSES: &[(&str, &str)] = &[
    (
        include_str!("../protocol/fixtures/supervisor-v1/error-malformed-request.json"),
        "protocol_failure",
    ),
    (
        include_str!("../protocol/fixtures/supervisor-v1/error-unsupported-version.json"),
        "unsupported_protocol_version",
    ),
];

#[test]
fn shared_supervisor_fixtures_preserve_relay_v1_envelopes() {
    let request: Value =
        serde_json::from_str(SUCCESS_REQUEST).expect("valid success request fixture");
    assert_eq!(request["protocol_version"], 1);
    assert_eq!(request["arguments"][1], "literal;$(not-a-shell)");
    assert!(serde_json::from_str::<Value>(MALFORMED_REQUEST).is_err());

    let unsupported: Value =
        serde_json::from_str(UNSUPPORTED_REQUEST).expect("valid unsupported-version fixture");
    assert_eq!(unsupported["protocol_version"], 2);

    for (fixture, lifecycle) in SUCCESS_RESPONSES {
        let envelope: Value =
            serde_json::from_str(fixture).expect("valid success response fixture");
        assert_eq!(
            object_keys(&envelope),
            keys(&["command", "data", "ok", "output_schema_version"])
        );
        assert_eq!(envelope["output_schema_version"], 1);
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["command"], "supervise");
        assert_eq!(envelope["data"]["protocol_version"], 1);
        assert_eq!(envelope["data"]["supervisor_version"], "0.1.0");
        assert_eq!(envelope["data"]["lifecycle"], *lifecycle);
        assert_eq!(
            envelope["data"]["lifecycle_history"][0]["lifecycle"],
            "accepted"
        );
        let history = envelope["data"]["lifecycle_history"]
            .as_array()
            .expect("lifecycle_history array");
        assert_eq!(
            history.last().expect("final lifecycle")["lifecycle"],
            *lifecycle
        );
    }

    for (fixture, code) in ERROR_RESPONSES {
        let envelope: Value = serde_json::from_str(fixture).expect("valid error response fixture");
        assert_eq!(
            object_keys(&envelope),
            keys(&["command", "error", "ok", "output_schema_version"])
        );
        assert_eq!(object_keys(&envelope["error"]), keys(&["code", "message"]));
        assert_eq!(envelope["output_schema_version"], 1);
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["command"], "supervise");
        assert_eq!(envelope["error"]["code"], *code);
    }
}

fn object_keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .expect("JSON object")
        .keys()
        .map(String::as_str)
        .collect()
}

fn keys<'a>(values: &'a [&'a str]) -> BTreeSet<&'a str> {
    values.iter().copied().collect()
}

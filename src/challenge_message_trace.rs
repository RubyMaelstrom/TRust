//! Opt-in, redacted diagnostics over the existing structured-clone wire snapshot.
//!
//! HTML #window-post-message-steps serializes once at send time, checks the target
//! origin in the queued task, then deserializes and dispatches. Observe that wire,
//! never the author's original object: inspecting it again could invoke getters.
//! Local WHATWG HTML snapshot e5071a20 (2026-09-06), web-messaging.html.

use lumen::embed::{Ctx, Value};
use serde_json::{Value as Json, json};
use std::sync::atomic::{AtomicUsize, Ordering};

const CHALLENGE_ORIGIN: &str = "https://challenges.cloudflare.com";
const MAX_WIRE_BYTES: usize = 64 * 1024;
const MAX_RECORDS: usize = 4096;

/// Only a fixed vocabulary and bounded numeric metadata may reach the log.
/// Unknown fields, free-form reasons, widget IDs, tokens and cookie values do not.
fn summarize(wire: &str) -> Json {
    if wire.len() > MAX_WIRE_BYTES {
        return json!({"payload": "oversize"});
    }
    let Ok(record) = serde_json::from_str::<Json>(wire) else {
        return json!({"payload": "invalid"});
    };
    let Some(index) = record[0][1].as_u64().and_then(|n| usize::try_from(n).ok()) else {
        return json!({"payload": "not-object"});
    };
    let node = &record[1][index];
    if record[0][0] != "r" || node[0] != "O" {
        return json!({"payload": "not-object"});
    }
    let Some(entries) = node[1].as_array() else {
        return json!({"payload": "invalid"});
    };
    let mut out = serde_json::Map::new();
    for entry in entries {
        let Some(key) = entry[0].as_str() else {
            continue;
        };
        let encoded = &entry[1];
        let value = &encoded[1];
        match key {
            "event" => {
                let event = value.as_str().filter(|_| encoded[0] == "s").unwrap_or("");
                let known = matches!(
                    event,
                    "init"
                        | "translationInit"
                        | "requestExtraParams"
                        | "extraParams"
                        | "complete"
                        | "fail"
                        | "reject"
                        | "food"
                        | "meow"
                        | "interactiveBegin"
                        | "interactiveEnd"
                        | "interactiveTimeout"
                        | "overrunBegin"
                        | "overrunEnd"
                        | "tokenExpired"
                        | "widgetStale"
                        | "refreshRequest"
                        | "reloadRequest"
                        | "reloadApiJsRequest"
                        | "feedbackInit"
                        | "feedbackActivity"
                        | "requestFeedbackData"
                        | "feedbackData"
                        | "closeFeedbackReportIframe"
                        | "turnstileResults"
                        | "languageUnsupported"
                        | "execute"
                );
                out.insert(key.into(), json!(if known { event } else { "other" }));
            }
            "code" | "errorCode" => {
                let code = match encoded[0].as_str() {
                    Some("d") => value.as_u64().filter(|n| *n <= 999_999),
                    Some("s") => value
                        .as_str()
                        .filter(|s| {
                            !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_digit())
                        })
                        .and_then(|s| s.parse::<u64>().ok()),
                    _ => None,
                };
                out.insert(key.into(), code.map_or(json!("redacted"), |n| json!(n)));
            }
            "reason" | "trigger" | "retry" | "refresh-expired" | "refresh-timeout" => {
                let text = value.as_str().filter(|_| encoded[0] == "s").unwrap_or("");
                let known = matches!(
                    text,
                    "browser"
                        | "auto"
                        | "never"
                        | "manual"
                        | "new"
                        | "crashed_retry"
                        | "failure_retry"
                        | "stale_execute"
                        | "auto_expire"
                        | "auto_timeout"
                        | "manual_refresh"
                        | "feedback_refresh"
                        | "api"
                );
                out.insert(key.into(), json!(if known { text } else { "redacted" }));
            }
            "retry-interval" | "expiry-interval" => {
                let interval = value
                    .as_u64()
                    .filter(|n| encoded[0] == "d" && *n <= 86_400_000);
                out.insert(key.into(), interval.map_or(json!("redacted"), |n| json!(n)));
            }
            "token" | "sToken" | "cfChlOut" | "cfChlOutS" | "rcV" | "nextRcV" => {
                out.insert(format!("{key}_present"), json!(true));
            }
            _ => {}
        }
    }
    Json::Object(out)
}

fn direction(from: &str, to: &str) -> Option<&'static str> {
    if from == CHALLENGE_ORIGIN {
        Some("from-challenge")
    } else if to == CHALLENGE_ORIGIN {
        Some("to-challenge")
    } else {
        None
    }
}

pub(super) fn call(_ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    // The bootstrap-only caller supplies primitives. Do not coerce objects here.
    let [
        Value::Str(phase),
        Value::Str(wire),
        Value::Str(from),
        Value::Str(to),
        Value::Num(source_frame),
        Value::Num(receiver_frame),
        ..,
    ] = args
    else {
        return Ok(Value::Undefined);
    };
    let Some(direction) = direction(from, to) else {
        return Ok(Value::Undefined);
    };
    if !matches!(
        phase.as_ref(),
        "send" | "drop-origin" | "messageerror" | "dispatch" | "dispatched"
    ) {
        return Ok(Value::Undefined);
    }
    static RECORDS: AtomicUsize = AtomicUsize::new(0);
    let count = RECORDS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        (n <= MAX_RECORDS).then_some(n + 1)
    });
    match count {
        Ok(n) if n < MAX_RECORDS => eprintln!(
            "[challenge-message] at_ms={} phase={phase} direction={direction} source_frame={} receiver_frame={} fields={}",
            crate::http::trace_ms(),
            *source_frame as u64,
            *receiver_frame as u64,
            summarize(wire)
        ),
        Ok(_) => eprintln!("[challenge-message] limit reached; further records omitted"),
        Err(_) => {}
    }
    Ok(Value::Undefined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(entries: Json) -> String {
        json!([["r", 0], [["O", entries]]]).to_string()
    }

    #[test]
    fn challenge_message_trace_keeps_result_metadata_but_redacts_secrets() {
        let summary = summarize(&wire(json!([
            ["event", ["s", "fail"]],
            ["code", ["s", "600010"]],
            ["retry", ["s", "auto"]],
            ["retry-interval", ["d", 8000]],
            ["token", ["s", "secret-token"]],
            ["sToken", ["s", "secret-secondary"]],
            ["cfChlOut", ["s", "secret-output"]],
            ["cookie", ["s", "secret-cookie"]],
            ["secret-field-name", ["s", "secret-value"]],
            ["widgetId", ["s", "secret-id"]],
            ["reason", ["s", "secret-reason\nforged log"]],
            ["nested", ["r", 0]]
        ])));
        assert_eq!(
            summary,
            json!({"event":"fail", "code":600010, "retry":"auto",
            "retry-interval":8000, "token_present":true, "sToken_present":true,
            "cfChlOut_present":true, "reason":"redacted"})
        );
        assert!(!summary.to_string().contains("secret"));
        assert_eq!(
            summarize(&wire(json!([
                ["event", ["s", "secret-event"]],
                ["errorCode", ["s", "secret-error"]]
            ]))),
            json!({"event":"other", "errorCode":"redacted"})
        );
    }

    #[test]
    fn challenge_message_trace_bounds_malformed_wire_and_exact_origins() {
        for input in [
            "null",
            "[]",
            "{}",
            "garbage",
            "[[],[]]",
            "[[\"r\",999999999],[]]",
        ] {
            assert!(summarize(input).get("payload").is_some());
        }
        assert_eq!(
            summarize(&"x".repeat(MAX_WIRE_BYTES + 1)),
            json!({"payload":"oversize"})
        );
        assert_eq!(
            direction(CHALLENGE_ORIGIN, "https://example.com"),
            Some("from-challenge")
        );
        assert_eq!(
            direction("https://example.com", CHALLENGE_ORIGIN),
            Some("to-challenge")
        );
        assert_eq!(
            direction("https://challenges.cloudflare.com.evil.invalid", "null"),
            None
        );
        assert_eq!(direction("http://challenges.cloudflare.com", "null"), None);
        assert_eq!(
            direction("https://challenges.cloudflare.com:8443", "null"),
            None
        );
    }
}

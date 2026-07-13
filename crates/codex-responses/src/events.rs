//! Defensive interpretation of private Codex Responses streaming events.

use imagegen_bridge_core::{BridgeError, ErrorCode, Usage};
use serde_json::Value;

#[derive(Default)]
pub(crate) struct EventState {
    final_image: Option<String>,
    partial_image: Option<String>,
    revised_prompt: Option<String>,
    usage: Usage,
    failure: Option<BridgeError>,
    completed: bool,
}

impl EventState {
    pub(crate) fn finish(self) -> Result<CallResult, BridgeError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.completed {
            return Err(protocol_error(
                "Codex Responses stream ended before response.completed",
            ));
        }
        let b64_json = self
            .final_image
            .ok_or_else(|| protocol_error("Codex Responses completed without final image data"))?;
        Ok(CallResult {
            b64_json,
            revised_prompt: self.revised_prompt,
            usage: self.usage,
        })
    }
}

#[derive(Debug)]
pub(crate) struct CallResult {
    pub(crate) b64_json: String,
    pub(crate) revised_prompt: Option<String>,
    pub(crate) usage: Usage,
}

pub(crate) fn process_event(
    data: &str,
    state: &mut EventState,
    maximum_base64: usize,
) -> Result<(), BridgeError> {
    if data.trim() == "[DONE]" || data.trim().is_empty() {
        return Ok(());
    }
    let event: Value = serde_json::from_str(data)
        .map_err(|_| protocol_error("Codex Responses sent malformed JSON event"))?;
    match event["type"].as_str().unwrap_or_default() {
        "response.output_item.done" => {
            if event["item"]["type"] == "image_generation_call" {
                capture_image_item(&event["item"], state, maximum_base64)?;
            }
        }
        "response.image_generation_call.partial_image" => {
            if let Some(partial) = event["partial_image_b64"]
                .as_str()
                .or_else(|| event["partial_image"].as_str())
            {
                check_base64_size(partial, maximum_base64)?;
                state.partial_image = Some(partial.to_owned());
            }
        }
        "response.completed" => {
            state.completed = true;
            if let Some(output) = event["response"]["output"].as_array() {
                for item in output {
                    if item["type"] == "image_generation_call" {
                        capture_image_item(item, state, maximum_base64)?;
                    }
                }
            }
            capture_usage(&event["response"]["usage"], &mut state.usage);
        }
        "response.failed" | "response.incomplete" | "error" => {
            let error = event
                .get("error")
                .or_else(|| event.get("response").and_then(|value| value.get("error")));
            let code = error
                .and_then(|value| value.get("code"))
                .and_then(Value::as_str)
                .or_else(|| event["code"].as_str())
                .unwrap_or("upstream_failure");
            state.failure = Some(classified_upstream_error(code));
        }
        _ => {}
    }
    Ok(())
}

fn capture_image_item(
    item: &Value,
    state: &mut EventState,
    maximum_base64: usize,
) -> Result<(), BridgeError> {
    if let Some(result) = item["result"].as_str().filter(|value| !value.is_empty()) {
        check_base64_size(result, maximum_base64)?;
        if state
            .final_image
            .as_deref()
            .is_some_and(|existing| existing != result)
        {
            return Err(protocol_error(
                "Codex Responses returned multiple final images for one tool call",
            ));
        }
        state.final_image = Some(result.to_owned());
    }
    if let Some(revised) = item["revised_prompt"]
        .as_str()
        .or_else(|| item["revisedPrompt"].as_str())
        .filter(|value| !value.is_empty())
    {
        state.revised_prompt = Some(revised.to_owned());
    }
    Ok(())
}

fn check_base64_size(value: &str, maximum: usize) -> Result<(), BridgeError> {
    if value.len() > maximum {
        return Err(protocol_error(
            "Codex image base64 exceeds the configured limit",
        ));
    }
    Ok(())
}

fn capture_usage(value: &Value, usage: &mut Usage) {
    usage.input_tokens = value["input_tokens"].as_u64();
    usage.output_tokens = value["output_tokens"].as_u64();
    usage.total_tokens = value["total_tokens"].as_u64();
}

pub(crate) fn merge_usage(total: &mut Usage, additional: &Usage) {
    total.input_tokens = add_optional(total.input_tokens, additional.input_tokens);
    total.output_tokens = add_optional(total.output_tokens, additional.output_tokens);
    total.total_tokens = add_optional(total.total_tokens, additional.total_tokens);
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

fn classified_upstream_error(raw_code: &str) -> BridgeError {
    let safe_code = sanitize_code(raw_code);
    let lower = safe_code.to_ascii_lowercase();
    let (code, retryable) = if lower.contains("auth") || lower.contains("token") {
        (ErrorCode::Authentication, false)
    } else if lower.contains("permission")
        || lower.contains("entitlement")
        || lower.contains("forbidden")
    {
        (ErrorCode::PermissionDenied, false)
    } else if lower.contains("safety")
        || lower.contains("content_policy")
        || lower.contains("moderation")
    {
        (ErrorCode::SafetyRejected, false)
    } else if lower.contains("rate") || lower.contains("quota") {
        (ErrorCode::RateLimited, true)
    } else if lower.contains("unsupported")
        || lower.contains("capability")
        || lower.contains("invalid_model")
    {
        (ErrorCode::UnsupportedCapability, false)
    } else if lower.contains("schema") || lower.contains("protocol") {
        (ErrorCode::Protocol, false)
    } else if lower.contains("unavailable")
        || lower.contains("overloaded")
        || lower.contains("server_error")
    {
        (ErrorCode::Upstream, true)
    } else {
        (ErrorCode::Upstream, false)
    };
    BridgeError::new(code, "Codex Responses reported a failure")
        .retryable(retryable)
        .with_provider("codex-responses")
        .with_detail("upstream_code", safe_code)
}

fn sanitize_code(raw: &str) -> String {
    let value: String = raw
        .chars()
        .take(64)
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .collect();
    if value.is_empty() {
        "upstream_failure".to_owned()
    } else {
        value
    }
}

fn protocol_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message).with_provider("codex-responses")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn final_image_wins_over_partial_and_usage_is_extracted() {
        let mut state = EventState::default();
        process_event(
            r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"partial"}"#,
            &mut state,
            100,
        )
        .unwrap();
        process_event(
            r#"{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"final","revised_prompt":"revised"}}"#,
            &mut state,
            100,
        )
        .unwrap();
        process_event(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}"#,
            &mut state,
            100,
        )
        .unwrap();
        let result = state.finish().unwrap();
        assert_eq!(result.b64_json, "final");
        assert_eq!(result.revised_prompt.as_deref(), Some("revised"));
        assert_eq!(result.usage.total_tokens, Some(5));
    }

    #[test]
    fn partial_or_truncated_streams_are_never_accepted_as_final() {
        let mut partial = EventState::default();
        process_event(
            r#"{"type":"response.image_generation_call.partial_image","partial_image":"partial"}"#,
            &mut partial,
            100,
        )
        .unwrap();
        process_event(
            r#"{"type":"response.completed","response":{}}"#,
            &mut partial,
            100,
        )
        .unwrap();
        assert_eq!(partial.finish().unwrap_err().code, ErrorCode::Protocol);

        let mut truncated = EventState::default();
        process_event(
            r#"{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"final"}}"#,
            &mut truncated,
            100,
        )
        .unwrap();
        assert_eq!(truncated.finish().unwrap_err().code, ErrorCode::Protocol);
    }

    #[test]
    fn rejects_distinct_multiple_finals_but_tolerates_duplicate_summary() {
        let mut state = EventState::default();
        process_event(
            r#"{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"one"}}"#,
            &mut state,
            100,
        )
        .unwrap();
        process_event(
            r#"{"type":"response.completed","response":{"output":[{"type":"image_generation_call","result":"one"}]}}"#,
            &mut state,
            100,
        )
        .unwrap();
        assert_eq!(state.finish().unwrap().b64_json, "one");

        let mut state = EventState::default();
        process_event(
            r#"{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"one"}}"#,
            &mut state,
            100,
        )
        .unwrap();
        let error = process_event(
            r#"{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"two"}}"#,
            &mut state,
            100,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Protocol);
    }

    #[test]
    fn classifies_and_sanitizes_upstream_codes() {
        let mut state = EventState::default();
        process_event(
            r#"{"type":"response.failed","response":{"error":{"code":"content_policy_violation/<secret>"}}}"#,
            &mut state,
            100,
        )
        .unwrap();
        let error = state.finish().unwrap_err();
        assert_eq!(error.code, ErrorCode::SafetyRejected);
        assert!(!format!("{error:?}").contains('<'));
    }

    #[test]
    fn classifies_auth_entitlement_capability_rate_transient_and_schema_failures() {
        let cases = [
            ("invalid_token", ErrorCode::Authentication, false),
            ("missing_entitlement", ErrorCode::PermissionDenied, false),
            (
                "unsupported_capability",
                ErrorCode::UnsupportedCapability,
                false,
            ),
            ("rate_limit_exceeded", ErrorCode::RateLimited, true),
            ("service_unavailable", ErrorCode::Upstream, true),
            ("response_schema_changed", ErrorCode::Protocol, false),
        ];
        for (code, expected, retryable) in cases {
            let error = classified_upstream_error(code);
            assert_eq!(error.code, expected, "classification for {code}");
            assert_eq!(error.retryable, retryable, "retryability for {code}");
        }
    }
}

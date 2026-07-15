//! Defensive interpretation of private Codex Responses streaming events.

use imagegen_bridge_core::{BridgeError, ErrorCode, Usage};
use serde_json::Value;
use std::collections::BTreeSet;

pub(crate) const MAX_REVISED_PROMPT_BYTES: usize = 128 * 1024;

#[derive(Default)]
pub(crate) struct EventState {
    final_image: Option<String>,
    next_partial_index: u8,
    revised_prompt: Option<String>,
    usage: Usage,
    failure: Option<BridgeError>,
    completed: bool,
    output_item_types: BTreeSet<&'static str>,
    message_content_types: BTreeSet<&'static str>,
    image_call_statuses: BTreeSet<&'static str>,
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
        let missing_image_error = self.missing_image_error();
        let b64_json = self.final_image.ok_or(missing_image_error)?;
        Ok(CallResult {
            b64_json,
            revised_prompt: self.revised_prompt,
            usage: self.usage,
            attempts: 1,
        })
    }

    fn missing_image_error(&self) -> BridgeError {
        let refusal = self.message_content_types.contains("refusal");
        let mut error = if refusal {
            BridgeError::safety_rejected("Codex Responses declined the image request")
                .with_detail("upstream_code", "completed_with_refusal")
        } else {
            BridgeError::new(
                ErrorCode::Upstream,
                "Codex Responses completed without final image data",
            )
            .retryable(true)
            .with_detail("upstream_code", "completed_without_image")
        }
        .with_provider("codex-responses");
        if !self.output_item_types.is_empty() {
            error = error.with_detail("output_item_types", &self.output_item_types);
        }
        if !self.message_content_types.is_empty() {
            error = error.with_detail("message_content_types", &self.message_content_types);
        }
        if !self.image_call_statuses.is_empty() {
            error = error.with_detail("image_call_statuses", &self.image_call_statuses);
        }
        error
    }
}

#[derive(Debug)]
pub(crate) struct CallResult {
    pub(crate) b64_json: String,
    pub(crate) revised_prompt: Option<String>,
    pub(crate) usage: Usage,
    pub(crate) attempts: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PartialImageData {
    pub(crate) partial_index: u8,
    pub(crate) b64_json: String,
}

pub(crate) fn process_event(
    data: &str,
    state: &mut EventState,
    maximum_base64: usize,
) -> Result<Option<PartialImageData>, BridgeError> {
    if data.trim() == "[DONE]" || data.trim().is_empty() {
        return Ok(None);
    }
    let event: Value = serde_json::from_str(data)
        .map_err(|_| protocol_error("Codex Responses sent malformed JSON event"))?;
    match event["type"].as_str().unwrap_or_default() {
        "response.output_item.done" => {
            record_output_item(&event["item"], state);
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
                let explicit_index = event["partial_image_index"]
                    .as_u64()
                    .or_else(|| event["partialImageIndex"].as_u64())
                    .map(|value| {
                        u8::try_from(value).map_err(|_| {
                            protocol_error(
                                "Codex partial image index is outside the supported range",
                            )
                        })
                    })
                    .transpose()?;
                let partial_index = explicit_index.unwrap_or(state.next_partial_index);
                state.next_partial_index = partial_index.saturating_add(1);
                return Ok(Some(PartialImageData {
                    partial_index,
                    b64_json: partial.to_owned(),
                }));
            }
        }
        "response.completed" => {
            state.completed = true;
            if let Some(output) = event["response"]["output"].as_array() {
                for item in output {
                    record_output_item(item, state);
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
            state.failure = Some(with_moderation_details(
                classified_upstream_error(code),
                error,
            ));
        }
        _ => {}
    }
    Ok(None)
}

fn record_output_item(item: &Value, state: &mut EventState) {
    let item_type = match item["type"].as_str() {
        Some("image_generation_call") => "image_generation_call",
        Some("message") => "message",
        Some("reasoning") => "reasoning",
        Some("function_call") => "function_call",
        Some("web_search_call") => "web_search_call",
        Some("computer_call") => "computer_call",
        Some(_) => "other",
        None => "missing",
    };
    state.output_item_types.insert(item_type);

    if item_type == "image_generation_call" {
        let status = match item["status"].as_str() {
            Some("in_progress") => "in_progress",
            Some("generating") => "generating",
            Some("completed") => "completed",
            Some("failed") => "failed",
            Some(_) => "other",
            None => "missing",
        };
        state.image_call_statuses.insert(status);
    }

    if item_type == "message"
        && let Some(content) = item["content"].as_array()
    {
        for part in content {
            let content_type = match part["type"].as_str() {
                Some("refusal") => "refusal",
                Some("output_text") => "output_text",
                Some(_) => "other",
                None => "missing",
            };
            state.message_content_types.insert(content_type);
        }
    }
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
        if revised.len() > MAX_REVISED_PROMPT_BYTES {
            return Err(protocol_error(
                "Codex revised prompt exceeds the configured limit",
            ));
        }
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
    let error = if code == ErrorCode::SafetyRejected {
        BridgeError::safety_rejected("Codex Responses rejected the image request")
    } else {
        BridgeError::new(code, "Codex Responses reported a failure").retryable(retryable)
    };
    error
        .with_provider("codex-responses")
        .with_detail("upstream_code", safe_code)
}

fn with_moderation_details(mut error: BridgeError, upstream: Option<&Value>) -> BridgeError {
    if error.code != ErrorCode::SafetyRejected {
        return error;
    }
    let Some(details) = upstream.and_then(|value| {
        value
            .get("moderation_details")
            .or_else(|| value.get("moderationDetails"))
    }) else {
        return error;
    };
    if let Some(stage @ ("input" | "output" | "unknown")) = details
        .get("moderation_stage")
        .or_else(|| details.get("moderationStage"))
        .and_then(Value::as_str)
    {
        error = error.with_detail("moderation_stage", stage);
    }
    let public_categories = details
        .get("categories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|category| {
            matches!(
                *category,
                "harassment" | "self-harm" | "sexual" | "violence"
            )
        })
        .take(8)
        .collect::<Vec<_>>();
    if !public_categories.is_empty() {
        error = error.with_detail("moderation_categories", public_categories);
    }
    error
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
        let partial = process_event(
            r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"partial"}"#,
            &mut state,
            100,
        )
        .unwrap()
        .unwrap();
        assert_eq!(partial.partial_index, 0);
        assert_eq!(partial.b64_json, "partial");
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
        let empty_completed = partial.finish().unwrap_err();
        assert_eq!(empty_completed.code, ErrorCode::Upstream);
        assert!(empty_completed.retryable);
        assert_eq!(
            empty_completed.details["upstream_code"],
            "completed_without_image"
        );

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
    fn completed_without_image_reports_only_redaction_safe_output_shape() {
        let mut state = EventState::default();
        process_event(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","summary":[]}}"#,
            &mut state,
            100,
        )
        .unwrap();
        process_event(
            r#"{"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"private upstream text"}]}]}}"#,
            &mut state,
            100,
        )
        .unwrap();
        let error = state.finish().unwrap_err();
        assert_eq!(error.code, ErrorCode::Upstream);
        assert!(error.retryable);
        assert_eq!(
            error.details["output_item_types"],
            serde_json::json!(["message", "reasoning"])
        );
        assert_eq!(
            error.details["message_content_types"],
            serde_json::json!(["output_text"])
        );
        assert!(!format!("{error:?}").contains("private upstream text"));
    }

    #[test]
    fn completed_refusal_is_a_non_retryable_safety_rejection() {
        let mut state = EventState::default();
        process_event(
            r#"{"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"refusal","refusal":"private refusal"}]}]}}"#,
            &mut state,
            100,
        )
        .unwrap();
        let error = state.finish().unwrap_err();
        assert_eq!(error.code, ErrorCode::SafetyRejected);
        assert!(!error.retryable);
        assert_eq!(error.details["upstream_code"], "completed_with_refusal");
        assert_eq!(
            error.details["message_content_types"],
            serde_json::json!(["refusal"])
        );
        assert!(!format!("{error:?}").contains("private refusal"));
    }

    #[test]
    fn failed_image_call_status_is_reported_without_provider_payloads() {
        let mut state = EventState::default();
        process_event(
            r#"{"type":"response.output_item.done","item":{"type":"image_generation_call","status":"failed","result":null}}"#,
            &mut state,
            100,
        )
        .unwrap();
        process_event(
            r#"{"type":"response.completed","response":{}}"#,
            &mut state,
            100,
        )
        .unwrap();
        let error = state.finish().unwrap_err();
        assert_eq!(
            error.details["output_item_types"],
            serde_json::json!(["image_generation_call"])
        );
        assert_eq!(
            error.details["image_call_statuses"],
            serde_json::json!(["failed"])
        );
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
            r#"{"type":"response.failed","response":{"error":{"code":"content_policy_violation/<secret>","moderation_details":{"moderation_stage":"input","categories":["harassment","internal_classifier_x"]}}}}"#,
            &mut state,
            100,
        )
        .unwrap();
        let error = state.finish().unwrap_err();
        assert_eq!(error.code, ErrorCode::SafetyRejected);
        assert_eq!(error.details["recovery"], "revise_prompt_or_inputs");
        assert_eq!(error.details["retry_same_request"], false);
        assert_eq!(error.details["moderation_stage"], "input");
        assert_eq!(
            error.details["moderation_categories"],
            serde_json::json!(["harassment"])
        );
        assert!(!format!("{error:?}").contains("internal_classifier_x"));
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

    #[test]
    fn revised_prompt_has_an_independent_utf8_byte_limit() {
        for key in ["revised_prompt", "revisedPrompt"] {
            let accepted = "é".repeat(MAX_REVISED_PROMPT_BYTES / 2);
            let mut event = serde_json::json!({
                "type": "response.output_item.done",
                "item": {"type": "image_generation_call", "result": "final"}
            });
            event["item"][key] = serde_json::Value::String(accepted.clone());
            let mut state = EventState::default();
            process_event(&event.to_string(), &mut state, 16).unwrap();

            let rejected = format!("{accepted}x");
            let mut event = serde_json::json!({
                "type": "response.output_item.done",
                "item": {"type": "image_generation_call", "result": "final"}
            });
            event["item"][key] = serde_json::Value::String(rejected);
            let mut state = EventState::default();
            let error = process_event(&event.to_string(), &mut state, 16).unwrap_err();
            assert_eq!(error.code, ErrorCode::Protocol);
            assert!(error.message.contains("revised prompt"));
        }
    }
}

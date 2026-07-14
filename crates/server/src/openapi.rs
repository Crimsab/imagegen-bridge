//! Deterministic `OpenAPI` 3.1 document derived from the native JSON Schema.

use serde_json::{Map, Value, json};

/// Generates the current `OpenAPI` 3.1 document with compatibility extensions and examples.
#[must_use]
pub fn openapi_document() -> Value {
    let contract = imagegen_bridge_core::contract_schema();
    let mut contract = serde_json::to_value(contract).unwrap_or_else(|_| json!({}));
    rewrite_references(&mut contract);
    let mut schemas = contract
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    add_compatibility_schemas(&mut schemas);

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Imagegen Bridge API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Provider-neutral image generation over Codex OAuth with native and OpenAI-familiar surfaces."
        },
        "servers": [{"url": "/"}],
        "tags": [
            {"name":"health","description":"Liveness and provider readiness"},
            {"name":"images","description":"Native lossless image operations"},
            {"name":"jobs","description":"Durable asynchronous image operations and history"},
            {"name":"artifacts","description":"Authenticated verified image delivery"},
            {"name":"compatibility","description":"OpenAI-familiar Images API"},
            {"name":"providers","description":"Provider discovery and capability negotiation"},
            {"name":"sessions","description":"Persistent session lifecycle"},
            {"name":"observability","description":"Opt-in low-cardinality operational metrics"}
        ],
        "paths": {
            "/health/live": {
                "get": {
                    "operationId": "getLiveness",
                    "tags": ["health"],
                    "responses": {"200": json_response("Process is live", json!({"type":"object","required":["status"],"properties":{"status":{"const":"live"}}}), json!({"status":"live"}))}
                }
            },
            "/health/ready": {
                "get": {
                    "operationId": "getReadiness",
                    "tags": ["health"],
                    "responses": {
                        "200": readiness_response("Providers are ready"),
                        "503": readiness_response("One or more providers are not ready")
                    }
                }
            },
            "/v1/providers": {
                "get": {
                    "operationId": "listProviders",
                    "tags": ["providers"],
                    "security": [{"bridgeBearer": []}],
                    "parameters": [
                        {"name":"limit","in":"query","schema":{"type":"integer","minimum":1,"maximum":100,"default":20}},
                        {"name":"cursor","in":"query","schema":{"type":"string","maxLength":256}}
                    ],
                    "responses": {
                        "200": json_response("Provider page", json!({"$ref":"#/components/schemas/ProviderPage"}), json!({"items":[{"name":"codex-app-server","display_name":"Codex app-server","version":"0.1.0","experimental":false}]})),
                        "400": error_response("Invalid provider cursor"),
                        "401": error_response("Bridge authentication required")
                    }
                }
            },
            "/v1/providers/{provider}/capabilities": {
                "get": {
                    "operationId": "getProviderCapabilities",
                    "tags": ["providers"],
                    "security": [{"bridgeBearer": []}],
                    "parameters": [
                        {"name":"provider","in":"path","required":true,"schema":{"type":"string","example":"codex-app-server"}},
                        {"name":"model","in":"query","schema":{"type":"string"}}
                    ],
                    "responses": {
                        "200": json_response("Provider capabilities", json!({"$ref":"#/components/schemas/ProviderCapabilities"}), json!({"provider":"codex-app-server","model":"gpt-image-2","generation":true,"edits":true})),
                        "400": error_response("Provider is unavailable or invalid"),
                        "401": error_response("Bridge authentication required")
                    }
                }
            },
            "/v1/sessions/{key}": {
                "get": {
                    "operationId":"getSession",
                    "tags":["sessions"],
                    "security": [{"bridgeBearer": []}],
                    "parameters": session_parameters(),
                    "responses": {
                        "200": json_response("Persistent session", json!({"$ref":"#/components/schemas/SessionMetadata"}), json!({"key":"gallery","thread_id":"019f-thread","reused":true})),
                        "401": error_response("Bridge authentication required"),
                        "404": error_response("Session not found")
                    }
                },
                "delete": {
                    "operationId":"deleteSession",
                    "tags":["sessions"],
                    "security": [{"bridgeBearer": []}],
                    "parameters": session_parameters(),
                    "responses": {
                        "204":{"description":"Session deleted"},
                        "401": error_response("Bridge authentication required"),
                        "404": error_response("Session not found")
                    }
                }
            },
            "/v1/images": {
                "post": native_image_operation("executeImage", false)
            },
            "/v1/images/stream": {
                "post": native_image_operation("streamImage", true)
            },
            "/v1/images/generations": {
                "post": compatible_generation_operation()
            },
            "/v1/images/edits": {
                "post": compatible_edit_operation()
            },
            "/v1/jobs": {
                "post": {
                    "operationId":"createImageJob",
                    "tags":["jobs"],
                    "security":[{"bridgeBearer":[]}],
                    "description":"Persists and schedules an image operation. Durable jobs always use artifact delivery.",
                    "requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/ImageRequest"},"example":native_request_example()}}},
                    "responses": {
                        "202": json_response("Job accepted", json!({"$ref":"#/components/schemas/ImageJob"}), job_example("queued")),
                        "400": error_response("Invalid input or unsupported capability"),
                        "401": error_response("Bridge authentication required"),
                        "422": error_response("Request validation failed"),
                        "503": error_response("Durable queue is full")
                    }
                },
                "get": {
                    "operationId":"listImageJobs",
                    "tags":["jobs"],
                    "security":[{"bridgeBearer":[]}],
                    "parameters":[
                        {"name":"limit","in":"query","schema":{"type":"integer","minimum":1,"maximum":100,"default":20}},
                        {"name":"cursor","in":"query","schema":{"type":"string","maxLength":256}},
                        {"name":"status","in":"query","schema":{"$ref":"#/components/schemas/ImageJobStatus"}},
                        {"name":"include_deleted","in":"query","schema":{"type":"boolean","default":false}}
                    ],
                    "responses": {
                        "200": json_response("Job page", json!({"$ref":"#/components/schemas/ImageJobPage"}), json!({"items":[job_example("succeeded")]})),
                        "400": error_response("Invalid job query or cursor"),
                        "401": error_response("Bridge authentication required")
                    }
                }
            },
            "/v1/jobs/{id}": {
                "get": {
                    "operationId":"getImageJob",
                    "tags":["jobs"],
                    "security":[{"bridgeBearer":[]}],
                    "parameters":[job_id_parameter()],
                    "responses": {
                        "200": json_response("Job detail", json!({"$ref":"#/components/schemas/ImageJob"}), job_example("succeeded")),
                        "401": error_response("Bridge authentication required"),
                        "404": error_response("Job not found")
                    }
                },
                "delete": {
                    "operationId":"cancelImageJob",
                    "tags":["jobs"],
                    "security":[{"bridgeBearer":[]}],
                    "parameters":[job_id_parameter()],
                    "responses": {
                        "200": json_response("Cancellation state", json!({"$ref":"#/components/schemas/ImageJob"}), job_example("cancelled")),
                        "401": error_response("Bridge authentication required"),
                        "404": error_response("Job not found")
                    }
                },
                "patch": {
                    "operationId":"updateImageJobHistory",
                    "tags":["jobs"],
                    "security":[{"bridgeBearer":[]}],
                    "parameters":[job_id_parameter()],
                    "requestBody":{"required":true,"content":{"application/json":{
                        "schema":{"$ref":"#/components/schemas/ImageJobUpdate"},
                        "example":{"favorite":true,"deleted":false}
                    }}},
                    "responses": {
                        "200": json_response("Updated history item", json!({"$ref":"#/components/schemas/ImageJob"}), job_example("succeeded")),
                        "401": error_response("Bridge authentication required"),
                        "404": error_response("Job not found"),
                        "422": error_response("Invalid history update")
                    }
                }
            },
            "/v1/artifacts/{id}": {
                "get": {
                    "operationId":"getImageArtifact",
                    "tags":["artifacts"],
                    "security":[{"bridgeBearer":[]}],
                    "parameters":[artifact_id_parameter()],
                    "responses": {
                        "200":{"description":"Verified image bytes","headers":{"ETag":{"schema":{"type":"string"}}},"content":{
                            "image/png":{"schema":{"type":"string","contentEncoding":"binary"}},
                            "image/jpeg":{"schema":{"type":"string","contentEncoding":"binary"}},
                            "image/webp":{"schema":{"type":"string","contentEncoding":"binary"}}
                        }},
                        "401":error_response("Bridge authentication required"),
                        "404":error_response("Artifact not found or verification failed")
                    }
                }
            },
            "/v1/artifacts/{id}/thumbnail": {
                "get": {
                    "operationId":"getImageArtifactThumbnail",
                    "tags":["artifacts"],
                    "security":[{"bridgeBearer":[]}],
                    "parameters":[
                        artifact_id_parameter(),
                        {"name":"edge","in":"query","schema":{"type":"integer","minimum":32,"maximum":2048,"default":384}}
                    ],
                    "responses": {
                        "200":{"description":"Bounded PNG thumbnail","content":{"image/png":{"schema":{"type":"string","contentEncoding":"binary"}}}},
                        "400":error_response("Invalid thumbnail size"),
                        "401":error_response("Bridge authentication required"),
                        "404":error_response("Artifact not found or verification failed")
                    }
                }
            },
            "/metrics": {
                "get": {
                    "operationId":"getMetrics",
                    "tags":["observability"],
                    "security": [{"bridgeBearer": []}],
                    "description":"Available only when server.metrics.enabled is true.",
                    "responses": {
                        "200":{"description":"Prometheus text exposition","content":{"text/plain":{"schema":{"type":"string"},"example":"imagegen_bridge_requests_total{provider=\"codex-app-server\",result=\"success\",code=\"none\"} 1\n"}}},
                        "401": error_response("Bridge authentication required"),
                        "404": error_response("Metrics are disabled")
                    }
                }
            }
        },
        "components": {
            "securitySchemes": {"bridgeBearer": {"type":"http","scheme":"bearer","description":"Optional bridge token, separate from provider OAuth."}},
            "schemas": schemas,
            "responses": {"ErrorResponse": error_response_component()}
        }
    })
}

fn job_id_parameter() -> Value {
    json!({"name":"id","in":"path","required":true,"schema":{"type":"string","format":"uuid","example":"019f0000-0000-7000-8000-000000000000"}})
}

fn artifact_id_parameter() -> Value {
    json!({"name":"id","in":"path","required":true,"schema":{"type":"string","format":"uuid","example":"019f0000-0000-7000-8000-000000000002"}})
}

fn job_example(status: &str) -> Value {
    json!({
        "id":"019f0000-0000-7000-8000-000000000000",
        "status":status,
        "created":1_784_000_000,
        "updated":1_784_000_001,
        "favorite":false,
        "request":native_request_example(),
        "cancel_requested":status == "cancelled"
    })
}

fn native_image_operation(operation_id: &str, streaming: bool) -> Value {
    let success = if streaming {
        json!({
            "description":"Bounded image progress stream",
            "content":{"text/event-stream":{"schema":{"type":"string"},"example":"event: started\ndata: {\"type\":\"started\"}\n\nevent: completed\ndata: {\"type\":\"completed\",\"response\":{...}}\n\n"}}
        })
    } else {
        json_response(
            "Successful image operation",
            json!({"$ref":"#/components/schemas/ImageResponse"}),
            native_response_example(),
        )
    };
    json!({
        "operationId": operation_id,
        "tags": ["images"],
        "security": [{"bridgeBearer": []}],
        "parameters": [{"name":"Idempotency-Key","in":"header","schema":{"type":"string","maxLength":512}}],
        "requestBody": {"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/ImageRequest"},"example":native_request_example()}}},
        "responses": common_image_responses(success)
    })
}

fn compatible_generation_operation() -> Value {
    json!({
        "operationId":"generateImageCompatible",
        "tags":["compatibility"],
        "security": [{"bridgeBearer": []}],
        "parameters": [{"name":"Idempotency-Key","in":"header","schema":{"type":"string","maxLength":512}}],
        "requestBody":{"required":true,"content":{"application/json":{
            "schema":{"$ref":"#/components/schemas/CompatibleGenerationRequest"},
            "example":{
                "model":"gpt-image-2",
                "prompt":"A red origami fox on warm gray",
                "n":1,
                "size":"auto",
                "quality":"auto",
                "response_format":"b64_json",
                "imagegen_bridge":{"provider":"codex-app-server","revised_prompt":"include","session":{"mode":"persistent","key":"gallery"}}
            }
        }}},
        "responses": common_image_responses(json_response("OpenAI-familiar image response", json!({"$ref":"#/components/schemas/CompatibleImagesResponse"}), compatible_response_example()))
    })
}

fn compatible_edit_operation() -> Value {
    json!({
        "operationId":"editImageCompatible",
        "tags":["compatibility"],
        "security": [{"bridgeBearer": []}],
        "parameters": [{"name":"Idempotency-Key","in":"header","schema":{"type":"string","maxLength":512}}],
        "requestBody":{"required":true,"content":{"multipart/form-data":{
            "schema":{"$ref":"#/components/schemas/CompatibleEditRequest"},
            "encoding":{"image":{"contentType":"image/png, image/jpeg, image/webp"},"mask":{"contentType":"image/png"}}
        }}},
        "responses": common_image_responses(json_response("OpenAI-familiar image response", json!({"$ref":"#/components/schemas/CompatibleImagesResponse"}), compatible_response_example()))
    })
}

#[allow(clippy::needless_pass_by_value)]
fn common_image_responses(success: Value) -> Value {
    json!({
        "200": success,
        "400": error_response("Invalid input or unsupported capability"),
        "401": error_response("Bridge authentication required"),
        "403": error_response("Permission denied"),
        "409": error_response("Idempotency conflict"),
        "422": error_response("Validation or safety rejection"),
        "429": error_response("Rate limited"),
        "500": error_response("Internal bridge failure"),
        "502": error_response("Upstream provider failure"),
        "503": error_response("Bounded capacity exhausted"),
        "504": error_response("Deadline exceeded")
    })
}

fn error_response(description: &str) -> Value {
    json!({"$ref":"#/components/responses/ErrorResponse","description":description})
}

fn error_response_component() -> Value {
    json_response(
        "OpenAI-compatible error with stable bridge details",
        json!({"$ref":"#/components/schemas/OpenAIErrorEnvelope"}),
        json!({
            "error": {
                "message": "request validation failed",
                "type": "invalid_request_error",
                "param": "prompt",
                "code": "invalid_request",
                "imagegen_bridge": {
                    "code": "invalid_request",
                    "retryable": false,
                    "details": {"field":"prompt"}
                }
            },
            "request_id": "019f0000-0000-7000-8000-000000000000"
        }),
    )
}

fn readiness_response(description: &str) -> Value {
    json_response(
        description,
        json!({"$ref":"#/components/schemas/ReadinessResponse"}),
        json!({"status":"ready","providers":[{"provider":"codex-app-server","status":"ready"}]}),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn json_response(description: &str, schema: Value, example: Value) -> Value {
    json!({"description":description,"content":{"application/json":{"schema":schema,"example":example}}})
}

fn session_parameters() -> Value {
    json!([
        {"name":"key","in":"path","required":true,"schema":{"type":"string","maxLength":256,"example":"gallery"}},
        {"name":"provider","in":"query","schema":{"type":"string","example":"codex-app-server"}}
    ])
}

fn native_request_example() -> Value {
    json!({
        "prompt":"A red origami fox on warm gray",
        "operation":"generate",
        "parameters":{"n":1,"size":"auto","quality":"auto","output_format":"png","background":"auto","moderation":"auto","partial_images":0,"failure_policy":"fail_fast","action":"auto"},
        "routing":{"provider":"codex-app-server"},
        "session":{"mode":"persistent","key":"gallery"},
        "output":{"response_format":"artifact","filename_prefix":"fox"},
        "policies":{"compatibility":"strict","negative_prompt":"auto","revised_prompt":"include"}
    })
}

fn native_response_example() -> Value {
    json!({
        "id":"019f0000-0000-7000-8000-000000000000",
        "created":1_713_833_628,
        "provider":"codex-app-server",
        "model":"gpt-image-2",
        "requested":{"n":1,"size":"auto","quality":"auto","output_format":"png","background":"auto","moderation":"auto","partial_images":0,"failure_policy":"fail_fast","action":"auto"},
        "effective":{"n":1,"size":"auto","quality":"auto","output_format":"png","background":"auto","moderation":"auto","partial_images":0,"failure_policy":"fail_fast","action":"auto"},
        "data":[{"index":0,"type":"artifact","id":"019f-artifact","name":"fox-019f.png","format":"png","width":1024,"height":1024,"bytes":123_456,"sha256":"0000000000000000000000000000000000000000000000000000000000000000","generation_ms":28_000}],
        "revised_prompt":"A centered red origami fox on a warm gray background.",
        "session":{"key":"gallery","thread_id":"019f-thread","reused":false},
        "timings":{"queue_ms":0,"input_ms":0,"provider_ms":1000,"artifact_ms":10,"total_ms":1010}
    })
}

fn compatible_response_example() -> Value {
    json!({
        "created":1_713_833_628,
        "data":[{"b64_json":"...","revised_prompt":"A centered red origami fox on a warm gray background."}],
        "imagegen_bridge":{
            "id":"019f0000-0000-7000-8000-000000000000",
            "provider":"codex-app-server",
            "model":"gpt-image-2",
            "effective":{"n":1,"size":"auto","quality":"auto","output_format":"png","background":"auto","moderation":"auto","partial_images":0,"failure_policy":"fail_fast","action":"auto"},
            "normalizations":[],
            "session":{"key":"gallery","thread_id":"019f-thread","reused":false},
            "timings":{"queue_ms":0,"input_ms":0,"provider_ms":1000,"artifact_ms":10,"total_ms":1010},
            "warnings":[]
        }
    })
}

fn add_compatibility_schemas(schemas: &mut Map<String, Value>) {
    schemas.insert("OpenAIErrorEnvelope".to_owned(), json!({
        "type":"object","additionalProperties":false,"required":["error","request_id"],"properties":{
            "error":{"$ref":"#/components/schemas/OpenAIError"},
            "request_id":{"type":"string"}
        }
    }));
    schemas.insert("OpenAIError".to_owned(), json!({
        "type":"object","additionalProperties":false,"required":["message","type","param","code","imagegen_bridge"],"properties":{
            "message":{"type":"string"},
            "type":{"type":"string"},
            "param":{"type":["string","null"]},
            "code":{"type":"string"},
            "imagegen_bridge":{"$ref":"#/components/schemas/BridgeErrorExtension"}
        }
    }));
    schemas.insert("BridgeErrorExtension".to_owned(), json!({
        "type":"object","additionalProperties":false,"required":["code","retryable"],"properties":{
            "code":{"$ref":"#/components/schemas/ErrorCode"},
            "retryable":{"type":"boolean"},
            "provider":{"type":"string"},
            "upstream_request_id":{"type":"string"},
            "details":{"type":"object","additionalProperties":true}
        }
    }));
    schemas.insert(
        "ProviderPage".to_owned(),
        json!({
            "type":"object","additionalProperties":false,"required":["items"],"properties":{
                "items":{"type":"array","items":{"$ref":"#/components/schemas/ProviderDescriptor"}},
                "next_cursor":{"type":"string"}
            }
        }),
    );
    schemas.insert("ReadinessResponse".to_owned(), json!({
        "type":"object","additionalProperties":false,"required":["status","providers"],"properties":{
            "status":{"enum":["ready","not_ready"]},
            "providers":{"type":"array","items":{"type":"object","required":["provider","status"],"properties":{"provider":{"type":"string"},"status":{"enum":["ready","not_ready"]},"error":{"$ref":"#/components/schemas/BridgeError"}}}}
        }
    }));
    schemas.insert(
        "CompatibleGenerationRequest".to_owned(),
        compatible_generation_schema(),
    );
    schemas.insert(
        "CompatibleExtensions".to_owned(),
        compatible_extensions_schema(),
    );
    schemas.insert("CompatibleEditRequest".to_owned(), compatible_edit_schema());
    schemas.insert(
        "CompatibleImagesResponse".to_owned(),
        compatible_response_schema(),
    );
}

fn compatible_generation_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,"required":["prompt"],"properties":{
            "prompt":{"type":"string"},"model":{"type":"string"},"n":{"type":"integer","minimum":1,"default":1},
            "size":{"$ref":"#/components/schemas/ImageSize"},"quality":{"$ref":"#/components/schemas/Quality"},
            "output_format":{"$ref":"#/components/schemas/OutputFormat"},"output_compression":{"type":"integer","minimum":0,"maximum":100},
            "background":{"$ref":"#/components/schemas/Background"},"moderation":{"$ref":"#/components/schemas/Moderation"},
            "response_format":{"enum":["b64_json","url"],"default":"b64_json"},"user":{"type":"string"},
            "imagegen_bridge":{"$ref":"#/components/schemas/CompatibleExtensions"}
        }
    })
}

fn compatible_edit_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,"required":["prompt","image"],"properties":{
            "prompt":{"type":"string"},"image":{"type":"array","items":{"type":"string","contentMediaType":"image/*"}},
            "mask":{"type":"string","contentMediaType":"image/png"},"reference_image":{"type":"array","items":{"type":"string","contentMediaType":"image/*"}},
            "model":{"type":"string"},"n":{"type":"integer","minimum":1},"size":{"$ref":"#/components/schemas/ImageSize"},
            "quality":{"$ref":"#/components/schemas/Quality"},"output_format":{"$ref":"#/components/schemas/OutputFormat"},
            "output_compression":{"type":"integer","minimum":0,"maximum":100},"background":{"$ref":"#/components/schemas/Background"},
            "moderation":{"$ref":"#/components/schemas/Moderation"},"input_fidelity":{"$ref":"#/components/schemas/InputFidelity"},"response_format":{"enum":["b64_json","url"]},"user":{"type":"string"},
            "provider":{"type":"string"},"negative_prompt":{"type":"string"},"compatibility":{"$ref":"#/components/schemas/CompatibilityMode"},
            "revised_prompt":{"$ref":"#/components/schemas/RevisedPromptPolicy"},"session_key":{"type":"string"}
        }
    })
}

fn compatible_response_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,"required":["created","data","imagegen_bridge"],"properties":{
            "created":{"type":"integer"},
            "data":{"type":"array","items":{"type":"object","properties":{"b64_json":{"type":"string"},"url":{"type":"string","format":"uri"},"revised_prompt":{"type":"string"}}}},
            "usage":{"$ref":"#/components/schemas/Usage"},
            "imagegen_bridge":{"type":"object","required":["id","provider","model","effective","normalizations","timings","warnings"],"properties":{
                "id":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},
                "effective":{"$ref":"#/components/schemas/GenerationParameters"},"normalizations":{"type":"array","items":{"$ref":"#/components/schemas/Normalization"}},
                "session":{"$ref":"#/components/schemas/SessionMetadata"},"timings":{"$ref":"#/components/schemas/Timings"},"warnings":{"type":"array","items":{"type":"string"}}
            }}
        }
    })
}

fn compatible_extensions_schema() -> Value {
    json!({
        "type":"object","additionalProperties":false,"properties":{
            "provider":{"type":"string"},"negative_prompt":{"type":"string"},
            "compatibility":{"$ref":"#/components/schemas/CompatibilityMode"},"negative_prompt_mode":{"$ref":"#/components/schemas/NegativePromptMode"},
            "revised_prompt":{"$ref":"#/components/schemas/RevisedPromptPolicy"},"aspect_ratio":{"$ref":"#/components/schemas/AspectRatio"},
            "resolution":{"$ref":"#/components/schemas/Resolution"},"partial_images":{"type":"integer","minimum":0,"maximum":3},
            "input_fidelity":{"$ref":"#/components/schemas/InputFidelity"},"action":{"$ref":"#/components/schemas/ImageAction"},
            "session":{"$ref":"#/components/schemas/SessionOptions"},"reference_images":{"type":"array","items":{"$ref":"#/components/schemas/ImageInput"}},
            "filename_prefix":{"type":"string"}
        }
    })
}

fn rewrite_references(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "$ref"
                    && let Some(reference) = value.as_str()
                    && let Some(name) = reference.strip_prefix("#/$defs/")
                {
                    *value = Value::String(format!("#/components/schemas/{name}"));
                } else {
                    rewrite_references(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(rewrite_references),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn document_contains_examples_extensions_and_resolvable_component_refs() {
        let document = openapi_document();
        assert_eq!(document["openapi"], "3.1.0");
        assert!(document["paths"]["/v1/images"].is_object());
        assert!(document["components"]["schemas"]["ImageRequest"].is_object());
        assert!(
            document["components"]["schemas"]["CompatibleGenerationRequest"]["properties"]
                ["imagegen_bridge"]
                .is_object()
        );
        assert!(
            document["paths"]["/v1/images/generations"]["post"]["requestBody"]["content"]
                ["application/json"]["example"]
                .is_object()
        );
        assert!(document["components"]["responses"]["ErrorResponse"]["content"]
            ["application/json"]["example"]["error"]["imagegen_bridge"]
            .is_object());

        let mut references = BTreeSet::new();
        collect_references(&document, &mut references);
        for reference in references {
            let pointer = reference.strip_prefix('#').expect("local reference");
            assert!(
                document.pointer(pointer).is_some(),
                "unresolved reference {reference}"
            );
        }
    }

    fn collect_references(value: &Value, references: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    references.insert(reference.to_owned());
                }
                object
                    .values()
                    .for_each(|value| collect_references(value, references));
            }
            Value::Array(values) => values
                .iter()
                .for_each(|value| collect_references(value, references)),
            _ => {}
        }
    }
}

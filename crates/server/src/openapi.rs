//! Deterministic `OpenAPI` 3.1 document derived from the native JSON Schema.

use serde_json::{Value, json};

/// Generates the current `OpenAPI` 3.1 document.
#[must_use]
pub fn openapi_document() -> Value {
    let contract = imagegen_bridge_core::contract_schema();
    let mut contract = serde_json::to_value(contract).unwrap_or_else(|_| json!({}));
    rewrite_references(&mut contract);
    let schemas = contract
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Imagegen Bridge API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Provider-neutral image generation over Codex OAuth."
        },
        "servers": [{"url": "/"}],
        "paths": {
            "/health/live": {"get": {"operationId": "getLiveness", "responses": {"200": {"description": "Process is live"}}}},
            "/health/ready": {"get": {"operationId": "getReadiness", "responses": {"200": {"description": "Providers ready"}, "503": {"description": "One or more providers not ready"}}}},
            "/v1/providers": {"get": {"operationId": "listProviders", "parameters": [
                {"name":"limit","in":"query","schema":{"type":"integer","minimum":1,"maximum":100}},
                {"name":"cursor","in":"query","schema":{"type":"string","maxLength":256}}
            ], "responses": {"200": {"description": "Provider page"}}}},
            "/v1/providers/{provider}/capabilities": {"get": {"operationId": "getProviderCapabilities", "parameters": [
                {"name":"provider","in":"path","required":true,"schema":{"type":"string"}},
                {"name":"model","in":"query","schema":{"type":"string"}}
            ], "responses": {"200": {"description":"Capabilities","content":{"application/json":{"schema":{"$ref":"#/components/schemas/ProviderCapabilities"}}}}}}},
            "/v1/sessions/{key}": {
                "get": {"operationId":"getSession","parameters":[{"name":"key","in":"path","required":true,"schema":{"type":"string"}},{"name":"provider","in":"query","schema":{"type":"string"}}],"responses":{"200":{"description":"Persistent session","content":{"application/json":{"schema":{"$ref":"#/components/schemas/SessionMetadata"}}}},"404":{"description":"Session not found"}}},
                "delete": {"operationId":"deleteSession","parameters":[{"name":"key","in":"path","required":true,"schema":{"type":"string"}},{"name":"provider","in":"query","schema":{"type":"string"}}],"responses":{"204":{"description":"Session deleted"},"404":{"description":"Session not found"}}}
            },
            "/v1/images": {"post": image_operation("executeImage", "#/components/schemas/ImageRequest", "#/components/schemas/ImageResponse")},
            "/v1/images/stream": {"post": image_operation("streamImage", "#/components/schemas/ImageRequest", "#/components/schemas/ProviderEvent")},
            "/v1/images/generations": {"post": {"operationId":"generateImageCompatible","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object"}}}},"responses":{"200":{"description":"OpenAI-familiar image response"}}}},
            "/v1/images/edits": {"post": {"operationId":"editImageCompatible","requestBody":{"required":true,"content":{"multipart/form-data":{"schema":{"type":"object","required":["prompt","image"]}}}},"responses":{"200":{"description":"OpenAI-familiar image response"}}}}
        },
        "components": {
            "securitySchemes": {"bridgeBearer": {"type":"http","scheme":"bearer"}},
            "schemas": schemas
        }
    })
}

fn image_operation(operation_id: &str, request: &str, response: &str) -> Value {
    json!({
        "operationId": operation_id,
        "security": [{"bridgeBearer": []}],
        "parameters": [{"name":"Idempotency-Key","in":"header","schema":{"type":"string","maxLength":512}}],
        "requestBody": {"required":true,"content":{"application/json":{"schema":{"$ref":request}}}},
        "responses": {
            "200":{"description":"Successful image operation","content":{"application/json":{"schema":{"$ref":response}}}},
            "400":{"description":"Invalid input"},
            "401":{"description":"Bridge authentication required"},
            "409":{"description":"Idempotency conflict"},
            "422":{"description":"Validation or safety rejection"},
            "429":{"description":"Rate limited"},
            "503":{"description":"Overloaded"},
            "504":{"description":"Deadline exceeded"}
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
    use super::*;

    #[test]
    fn document_contains_versioned_routes_and_resolvable_component_refs() {
        let document = openapi_document();
        assert_eq!(document["openapi"], "3.1.0");
        assert!(document["paths"]["/v1/images"].is_object());
        assert!(document["components"]["schemas"]["ImageRequest"].is_object());
        let rendered = serde_json::to_string(&document).unwrap_or_default();
        assert!(!rendered.contains("#/$defs/"));
    }
}

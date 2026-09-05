use serde_json::{json, Map, Value};

use crate::tools::{is_allowed_tool, MUTATING_TOOLS};

pub fn build_openapi(tools: &[Value], public_base_url: &str, auth_type: &str) -> Value {
    let mut paths = Map::new();
    let use_api_key = auth_type == "api_key";

    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !is_allowed_tool(name) {
            continue;
        }

        let input_schema = tool
            .get("inputSchema")
            .filter(|schema| schema.is_object())
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "type": "object",
                    "additionalProperties": true
                })
            });
        let output_schema = tool
            .get("outputSchema")
            .filter(|schema| schema.is_object())
            .cloned()
            .unwrap_or_else(default_structured_content_schema);

        let description_raw = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("Call coding tool");
        let description: String = description_raw.chars().take(700).collect();
        let summary: String = description.chars().take(300).collect();

        let mut operation = json!({
            "operationId": format!("coding_{name}"),
            "summary": summary,
            "description": description,
            "requestBody": {
                "required": false,
                "content": {
                    "application/json": {
                        "schema": input_schema
                    }
                }
            },
            "responses": {
                "200": {
                    "description": "Tool execution result",
                    "content": {
                        "application/json": {
                            "schema": tool_execution_response_schema(output_schema)
                        }
                    }
                },
                "400": { "description": "Invalid request or policy rejection" },
                "401": { "description": "Invalid API key" },
                "422": { "description": "Tool execution failed" },
                "502": { "description": "MCP backend failure" }
            },
            "x-openai-isConsequential": MUTATING_TOOLS.contains(&name)
        });

        if use_api_key {
            operation
                .as_object_mut()
                .expect("operation object")
                .insert("security".to_string(), json!([{ "bearerAuth": [] }]));
        }

        paths.insert(format!("/actions/{name}"), json!({ "post": operation }));
    }

    let mut document = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Coding Tools Actions",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Read, modify and test a workspace through coding-tools-mcp."
        },
        "servers": [{ "url": public_base_url.trim_end_matches('/') }],
        "paths": paths,
        "components": {
            "schemas": {
                "ContentPart": content_part_schema(),
                "ToolError": tool_error_schema(),
                "StructuredContent": default_structured_content_schema(),
                "ToolExecutionResponse": tool_execution_response_schema(
                    json!({ "$ref": "#/components/schemas/StructuredContent" })
                )
            }
        }
    });

    if use_api_key {
        document
            .as_object_mut()
            .expect("document object")
            .get_mut("components")
            .and_then(Value::as_object_mut)
            .expect("components object")
            .insert(
                "securitySchemes".to_string(),
                json!({
                    "bearerAuth": {
                        "type": "http",
                        "scheme": "bearer"
                    }
                }),
            );
    }

    document
}

fn content_part_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": { "type": "string" },
            "text": { "type": "string" },
            "mimeType": { "type": "string" },
            "data": { "type": "string" }
        },
        "required": ["type"],
        "additionalProperties": true
    })
}

fn tool_error_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": { "type": "string" },
            "message": { "type": "string" },
            "category": { "type": "string" },
            "retryable": { "type": "boolean" },
            "details": {
                "type": "object",
                "properties": {},
                "additionalProperties": true
            }
        },
        "additionalProperties": true
    })
}

fn tool_execution_response_schema(structured_content: Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            "ok": { "type": "boolean" },
            "tool": { "type": "string" },
            "structured_content": structured_content,
            "content": {
                "type": "array",
                "items": { "$ref": "#/components/schemas/ContentPart" }
            },
            "is_error": { "type": "boolean" }
        },
        "required": ["ok", "tool", "is_error"],
        "additionalProperties": true
    })
}

fn default_structured_content_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ok": { "type": "boolean" },
            "error": tool_error_schema(),
            "diagnostics": {
                "type": "object",
                "properties": {},
                "additionalProperties": true
            },
            "permission_request": {
                "type": "object",
                "properties": {
                    "tool_name": { "type": "string" },
                    "permission": { "type": "string" },
                    "status": { "type": "string" },
                    "retryable": { "type": "boolean" }
                },
                "additionalProperties": true
            }
        },
        "additionalProperties": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openapi_without_auth_has_no_security_scheme() {
        let tools = [json!({
            "name": "read_file",
            "description": "Read a file",
            "inputSchema": { "type": "object" }
        })];
        let schema = build_openapi(&tools, "https://actions.example.com", "none");
        assert!(schema["paths"]["/actions/read_file"]["post"]["security"].is_null());
        assert!(schema["components"]["securitySchemes"].is_null());
    }

    #[test]
    fn openapi_api_key_includes_bearer_security() {
        let tools = [json!({
            "name": "read_file",
            "description": "Read a file",
            "inputSchema": { "type": "object" }
        })];
        let schema = build_openapi(&tools, "https://actions.example.com", "api_key");
        assert_eq!(
            schema["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        assert_eq!(
            schema["paths"]["/actions/read_file"]["post"]["security"],
            json!([{ "bearerAuth": [] }])
        );
    }

    #[test]
    fn core_openapi_exposes_grep_text_as_read_only() {
        let tools = crate::tools::list_tools_for_profile("core");
        let schema = build_openapi(&tools, "https://actions.example.com", "none");
        let operation = &schema["paths"]["/actions/grep_text"]["post"];

        assert_eq!(operation["operationId"], "coding_grep_text");
        assert_eq!(operation["x-openai-isConsequential"], false);
        assert_eq!(
            operation["requestBody"]["content"]["application/json"]["schema"],
            crate::tools::registry::input_schema("grep_text")
        );
    }

    #[test]
    fn core_openapi_uses_tool_output_schema_for_apply_patch() {
        let tools = crate::tools::list_tools_for_profile("core");
        let schema = build_openapi(&tools, "https://actions.example.com", "none");
        let structured = &schema["paths"]["/actions/apply_patch"]["post"]["responses"]["200"]
            ["content"]["application/json"]["schema"]["properties"]["structured_content"];

        assert_eq!(structured["type"], "object");
        assert_eq!(structured["required"], json!(["ok"]));
        assert_eq!(
            structured["properties"]["affected_files"]["items"]["properties"]["operation"]["enum"],
            json!(["add", "update", "delete"])
        );
    }
}

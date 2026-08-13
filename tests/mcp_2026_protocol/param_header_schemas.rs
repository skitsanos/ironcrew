use serde_json::{Value, json};

pub(super) fn valid_tool() -> Value {
    json!({
        "name": "promote",
        "description": "Promote statically reachable primitive arguments.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "plain": primitive("Plain"),
                "x-mcp-header": primitive("Literal"),
                "literal_data": {
                    "type": "object",
                    "default": {"x-mcp-header": "literal-data"},
                    "const": {"x-mcp-header": "literal-const"},
                    "enum": [{"x-mcp-header": "literal-enum"}],
                    "examples": [{"x-mcp-header": "literal-example"}]
                },
                "tenant": {
                    "type": "object",
                    "properties": {
                        "region": primitive("Region"),
                        "enabled": {"type": "boolean", "x-mcp-header": "Enabled"},
                        "quota": {"type": "integer", "x-mcp-header": "Quota"},
                        "omitted": primitive("Omitted"),
                        "nullable": primitive("Nullable"),
                        "sentinel": primitive("Sentinel"),
                        "unicode": primitive("Unicode"),
                        "control": primitive("Control"),
                        "padded": primitive("Padded")
                    }
                }
            }
        }
    })
}

pub(super) fn invalid_and_valid_tools() -> Value {
    json!([
        {
            "name": "valid_sibling",
            "description": "Unannotated schema constructs do not invalidate a promoted sibling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "value": primitive("Valid"),
                    "items_are_legal": {"type": "array", "items": {"type": "string"}},
                    "composition_is_legal": {
                        "oneOf": [{"type": "string"}, {"type": "null"}]
                    },
                    "reference_is_legal": {"$ref": "#/$defs/Plain"}
                },
                "$defs": {"Plain": {"type": "string"}}
            }
        },
        tool("under_items", json!({"values": {
            "type": "array", "items": primitive("Items")
        }})),
        tool("under_prefix_items", json!({"values": {
            "type": "array", "prefixItems": [primitive("Prefix")]
        }})),
        tool("under_one_of", json!({"value": {
            "oneOf": [primitive("Choice"), {"type": "null"}]
        }})),
        tool("under_any_of", json!({"value": {
            "anyOf": [primitive("Alternative"), {"type": "null"}]
        }})),
        tool("under_all_of", json!({"value": {
            "allOf": [primitive("Conjunction")]
        }})),
        tool("under_not", json!({"value": {"not": primitive("Negated")}})),
        tool("under_additional_properties", json!({"value": {
            "type": "object", "additionalProperties": primitive("Additional")
        }})),
        tool("under_conditional", json!({"value": {
            "if": {"type": "string"}, "then": primitive("Conditional")
        }})),
        tool("under_else", json!({"value": {
            "if": {"type": "string"}, "else": primitive("Else")
        }})),
        tool("malformed_properties", json!({"outer": {
            "type": "object",
            "properties": [{"type": "string", "x-mcp-header": "Malformed"}]
        }})),
        tool("under_pattern_properties", json!({"value": {
            "type": "object", "patternProperties": {".*": primitive("Pattern")}
        }})),
        tool("under_dependent_schemas", json!({"value": {
            "type": "object", "dependentSchemas": {"mode": primitive("Dependent")}
        }})),
        {
            "name": "under_ref", "description": "invalid ref annotation",
            "inputSchema": {
                "type": "object",
                "properties": {"value": {"$ref": "#/$defs/Promoted"}},
                "$defs": {"Promoted": primitive("Referenced")}
            }
        },
        {
            "name": "under_definitions", "description": "invalid legacy definition annotation",
            "inputSchema": {
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "definitions": {"Promoted": primitive("Definition")}
            }
        },
        tool("duplicate_case", json!({
            "first": primitive("Tenant"),
            "nested": {"type": "object", "properties": {
                "second": primitive("tenant")
            }}
        })),
        tool("number_type", json!({
            "amount": {"type": "number", "x-mcp-header": "Amount"}
        })),
        tool("bad_token", json!({
            "value": {"type": "string", "x-mcp-header": "bad:name"}
        })),
        tool("empty_name", json!({
            "value": {"type": "string", "x-mcp-header": ""}
        })),
        tool("non_string_name", json!({
            "value": {"type": "string", "x-mcp-header": true}
        })),
        {
            "name": "root_annotation",
            "description": "The root is not reached through properties.",
            "inputSchema": {
                "type": "object",
                "x-mcp-header": "Root",
                "properties": {}
            }
        }
    ])
}

pub(super) fn integer_tool() -> Value {
    tool(
        "integer",
        json!({
            "minimum": {"type": "integer", "x-mcp-header": "Minimum"},
            "maximum": {"type": "integer", "x-mcp-header": "Maximum"}
        }),
    )
}

fn primitive(header: &str) -> Value {
    json!({"type": "string", "x-mcp-header": header})
}

fn tool(name: &str, properties: Value) -> Value {
    json!({
        "name": name,
        "description": format!("Fixture tool {name}."),
        "inputSchema": {"type": "object", "properties": properties}
    })
}

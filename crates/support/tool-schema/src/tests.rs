use super::*;
use serde_json::json;

fn write_file() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path."},
            "content": {"type": "string"},
            "mode": {"enum": ["replace", "append"]},
        },
        "required": ["path", "content"],
        "additionalProperties": false,
    })
}

// Validation ----------------------------------------------------------------

#[test]
fn valid_arguments_pass() {
    assert_eq!(validate_args(&write_file(), &json!({"path": "a", "content": "b"})), Ok(()));
    assert_eq!(validate_args(&write_file(), &json!({"path": "a", "content": "b", "mode": "append"})), Ok(()));
}

#[test]
fn every_problem_is_reported_with_field_and_expected_type() {
    let err = validate_args(&write_file(), &json!({"path": 3, "extra": true})).unwrap_err();
    assert_eq!(
        err,
        "missing required field `content` (string); \
         unknown field `extra`; expected one of: content, mode, path; \
         `path` must be string, got a number (3)",
    );
}

#[test]
fn the_arguments_themselves_must_have_the_right_shape() {
    let err = validate_args(&write_file(), &json!(["a"])).unwrap_err();
    assert!(err.starts_with("the arguments must be {"), "{err}");
    assert!(err.ends_with("got an array"), "{err}");
}

#[test]
fn enums_integers_and_nested_paths() {
    let schema = json!({
        "type": "object",
        "properties": {
            "mode": {"enum": ["a", "b"]},
            "count": {"type": "integer"},
            "items": {"type": "array", "items": {
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
            }},
        },
    });
    let err = validate_args(&schema, &json!({
        "mode": "c", "count": 1.5, "items": [{"name": "x"}, {}],
    })).unwrap_err();
    assert_eq!(
        err,
        "`count` must be number, got a number (1.5); \
         missing required field `items[1].name` (string); \
         `mode` must be one of \"a\", \"b\", got \"c\"",
    );
    assert_eq!(validate_args(&schema, &json!({"count": 2.0})), Ok(()));
}

#[test]
fn unions_and_open_objects() {
    let schema = json!({
        "type": "object",
        "properties": {
            "id": {"anyOf": [{"type": "string"}, {"type": "number"}]},
            "tags": {"type": "object", "additionalProperties": {"type": "string"}},
            "maybe": {"type": ["string", "null"]},
        },
    });
    assert_eq!(validate_args(&schema, &json!({"id": 4, "tags": {"a": "b"}, "maybe": null})), Ok(()));
    let err = validate_args(&schema, &json!({"id": true, "tags": {"a": 1}})).unwrap_err();
    assert_eq!(err, "`id` must be string | number, got a boolean (true); `tags.a` must be string, got a number (1)");
}

#[test]
fn an_empty_or_missing_schema_accepts_anything() {
    assert_eq!(validate_args(&json!({}), &json!({"x": 1})), Ok(()));
    assert_eq!(validate_args(&Value::Null, &json!(42)), Ok(()));
}

// TypeScript ----------------------------------------------------------------

#[test]
fn declarations_cover_required_optional_nested_and_arrays() {
    let read = json!({
        "type": "object",
        "properties": {"path": {"type": "string", "description": "File path, relative to the project root."}},
        "required": ["path"],
    });
    let list = json!({"type": "object", "properties": {"path": {"type": "string"}}});
    let search = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "limit": {"type": "integer"},
            "filters": {
                "type": "object",
                "properties": {"ext": {"type": "array", "items": {"type": "string"}}, "hidden": {"type": "boolean"}},
                "required": ["ext"],
            },
            "mode": {"enum": ["fast", "exact"]},
            "weird-key": {"type": ["string", "null"]},
        },
        "required": ["query"],
    });
    let tools = [
        ToolSignature { name: "read_file", description: "Read a text file.", parameters: &read },
        ToolSignature { name: "list_dir", description: "List a directory.", parameters: &list },
        ToolSignature { name: "search", description: "Search */ files.", parameters: &search },
    ];
    assert_eq!(render_ts_declarations(&tools), r#"declare const tools: {
  /** Read a text file. */
  read_file(args: {
    /** File path, relative to the project root. */
    path: string;
  }): string;
  /** List a directory. */
  list_dir(args?: {
    path?: string;
  }): string;
  /** Search *\/ files. */
  search(args: {
    filters?: {
      ext: string[];
      hidden?: boolean;
    };
    limit?: number;
    mode?: "fast" | "exact";
    query: string;
    "weird-key"?: string | null;
  }): string;
};"#);
}

#[test]
fn no_tools_and_schemaless_tools_still_render() {
    assert_eq!(render_ts_declarations(&[]), "declare const tools: {};");
    let empty = json!({"type": "object"});
    let out = render_ts_declarations(&[ToolSignature { name: "ping", description: "", parameters: &empty }]);
    assert_eq!(out, "declare const tools: {\n  ping(args?: Record<string, unknown>): string;\n};");
}

#[test]
fn array_of_unions_is_parenthesised() {
    assert_eq!(ts_type(&json!({"type": "array", "items": {"type": ["string", "number"]}}), 0), "(string | number)[]");
    assert_eq!(ts_type(&json!({"type": "array"}), 0), "unknown[]");
}

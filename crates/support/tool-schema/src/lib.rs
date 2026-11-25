//! Tool argument schemas: validation and TypeScript rendering.
//!
//! Covers the JSON Schema subset tools declare: `type` (one or a list),
//! `properties`, `required`, `additionalProperties`, `items`, `enum`, `const`,
//! `anyOf` and `oneOf`. Keywords outside that subset are ignored, never
//! guessed at. Types are shown to the model in TypeScript notation in both
//! places, so a validation message and a declaration use the same words.
//!
//! A technical crate: no domain types, no workspace dependencies.

use serde_json::{Map, Value};

/// What the renderer needs to know about one tool.
#[derive(Debug, Clone, Copy)]
pub struct ToolSignature<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: &'a Value,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Check `args` against `schema`. `Err` lists every mismatch, each naming the
/// field and the expected type, separated by `; `.
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    let mut problems = Vec::new();
    check(schema, args, "", &mut problems);
    if problems.is_empty() { Ok(()) } else { Err(problems.join("; ")) }
}

fn check(schema: &Value, value: &Value, path: &str, problems: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else { return };

    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        if !choices.contains(value) {
            problems.push(format!("{} must be one of {}, got {}", subject(path), literals(choices), value));
        }
        return;
    }
    if let Some(constant) = schema.get("const") {
        if constant != value {
            problems.push(format!("{} must be {constant}, got {value}", subject(path)));
        }
        return;
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(key).and_then(Value::as_array) {
            let fits = variants.iter().any(|v| {
                let mut inner = Vec::new();
                check(v, value, path, &mut inner);
                inner.is_empty()
            });
            if !fits {
                problems.push(format!("{} must be {}, got {}", subject(path), ts_type(&Value::Object(schema.clone()), 0), describe(value)));
            }
            return;
        }
    }

    let allowed = declared_types(schema);
    if !allowed.is_empty() && !allowed.iter().any(|t| has_type(value, t)) {
        problems.push(format!("{} must be {}, got {}", subject(path), ts_type(&Value::Object(schema.clone()), 0), describe(value)));
        return;
    }

    match value {
        Value::Object(fields) => check_object(schema, fields, path, problems),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(item_schema, item, &format!("{path}[{i}]"), problems);
                }
            }
        }
        _ => {}
    }
}

fn check_object(schema: &Map<String, Value>, fields: &Map<String, Value>, path: &str, problems: &mut Vec<String>) {
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !fields.contains_key(name) {
                let expected = properties.and_then(|p| p.get(name)).map_or("a value".into(), |s| ts_type(s, 0));
                problems.push(format!("missing required field `{}` ({expected})", join(path, name)));
            }
        }
    }
    for (name, value) in fields {
        let field_path = join(path, name);
        match properties.and_then(|p| p.get(name)) {
            Some(field_schema) => check(field_schema, value, &field_path, problems),
            None => match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    let known: Vec<&str> = properties.map(|p| p.keys().map(String::as_str).collect()).unwrap_or_default();
                    problems.push(if known.is_empty() {
                        format!("unknown field `{field_path}`; no fields are accepted")
                    } else {
                        format!("unknown field `{field_path}`; expected one of: {}", known.join(", "))
                    });
                }
                Some(extra @ Value::Object(_)) => check(extra, value, &field_path, problems),
                _ => {}
            },
        }
    }
}

fn declared_types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

fn has_type(value: &Value, json_type: &str) -> bool {
    match json_type {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        // An unknown type name constrains nothing rather than rejecting everything.
        _ => true,
    }
}

fn subject(path: &str) -> String {
    if path.is_empty() { "the arguments".into() } else { format!("`{path}`") }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.into() } else { format!("{path}.{name}") }
}

fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("a boolean ({b})"),
        Value::Number(n) => format!("a number ({n})"),
        Value::String(s) => format!("a string ({})", Value::String(s.clone())),
        Value::Array(_) => "an array".into(),
        Value::Object(_) => "an object".into(),
    }
}

fn literals(choices: &[Value]) -> String {
    choices.iter().map(Value::to_string).collect::<Vec<_>>().join(", ")
}

// ---------------------------------------------------------------------------
// TypeScript
// ---------------------------------------------------------------------------

/// `declare const tools: { ... };` for the given tools. Each tool takes one
/// argument object (optional when nothing in it is required) and returns its
/// text output.
pub fn render_ts_declarations(tools: &[ToolSignature<'_>]) -> String {
    if tools.is_empty() {
        return "declare const tools: {};".into();
    }
    let mut out = String::from("declare const tools: {\n");
    for tool in tools {
        push_doc(&mut out, tool.description, 2);
        let optional = !has_required(tool.parameters);
        out.push_str(&format!(
            "  {}(args{}: {}): string;\n",
            key(tool.name),
            if optional { "?" } else { "" },
            ts_type(tool.parameters, 2),
        ));
    }
    out.push_str("};");
    out
}

/// The TypeScript type for `schema`. Object types are laid out over several
/// lines, indented from column `indent`.
pub fn ts_type(schema: &Value, indent: usize) -> String {
    let Some(schema) = schema.as_object() else { return "unknown".into() };
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        return literals(choices).replace(", ", " | ");
    }
    if let Some(constant) = schema.get("const") {
        return constant.to_string();
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(key).and_then(Value::as_array) {
            return union(variants.iter().map(|v| ts_type(v, indent)));
        }
    }
    let types = declared_types(schema);
    if types.len() > 1 {
        return union(types.iter().map(|t| {
            let mut one = schema.clone();
            one.insert("type".into(), Value::String((*t).into()));
            ts_type(&Value::Object(one), indent)
        }));
    }
    match types.first().copied() {
        Some("string") => "string".into(),
        Some("number" | "integer") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => {
            let item = schema.get("items").map_or("unknown".into(), |s| ts_type(s, indent));
            if item.contains(' ') && !item.starts_with('{') { format!("({item})[]") } else { format!("{item}[]") }
        }
        Some("object") => object_type(schema, indent),
        None if schema.contains_key("properties") => object_type(schema, indent),
        _ => "unknown".into(),
    }
}

fn object_type(schema: &Map<String, Value>, indent: usize) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object).filter(|p| !p.is_empty()) else {
        return match schema.get("additionalProperties") {
            Some(extra @ Value::Object(_)) => format!("Record<string, {}>", ts_type(extra, indent)),
            _ => "Record<string, unknown>".into(),
        };
    };
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let mut out = String::from("{\n");
    for (name, field) in properties {
        if let Some(doc) = field.get("description").and_then(Value::as_str) {
            push_doc(&mut out, doc, indent + 2);
        }
        out.push_str(&format!(
            "{}{}{}: {};\n",
            " ".repeat(indent + 2),
            key(name),
            if required.contains(&name.as_str()) { "" } else { "?" },
            ts_type(field, indent + 2),
        ));
    }
    out.push_str(&" ".repeat(indent));
    out.push('}');
    out
}

fn union(parts: impl Iterator<Item = String>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for p in parts {
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen.join(" | ")
}

fn has_required(schema: &Value) -> bool {
    schema.get("required").and_then(Value::as_array).is_some_and(|r| !r.is_empty())
}

/// A property or method name, quoted when it is not a plain identifier.
fn key(name: &str) -> String {
    let mut chars = name.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain { name.into() } else { Value::String(name.into()).to_string() }
}

fn push_doc(out: &mut String, text: &str, indent: usize) {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ").replace("*/", "*\\/");
    if !text.is_empty() {
        out.push_str(&format!("{}/** {text} */\n", " ".repeat(indent)));
    }
}

#[cfg(test)]
mod tests;

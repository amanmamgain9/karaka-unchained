//! The JSON Schema subset dsh enforces, and its validator, with dsh's exact
//! wording. (dsh: `tools/src/json-schema.ts`: `assertSupportedJsonSchema`,
//! `assertObjectJsonSchema` and `validateJsonSchemaValue`.)
//!
//! The subset is any JSON root; an annotation-only schema, which takes any
//! value; one scalar `type`; object `properties`, `required` and a boolean
//! `additionalProperties`; array `items`; scalar `enum` and `const` of the
//! node's own type; and exact-one `oneOf` with at least two branches.
//! Annotations (`description`, `title`, `default`, `examples`) are ignored
//! when validating. Any other keyword is refused when the schema is checked,
//! rather than accepted and then ignored.
//!
//! dsh walks with explicit stacks so a deep schema can't overflow the call
//! stack. Here the walk recurses: its depth is the schema's, which a tool's
//! author writes, not the model's arguments. dsh's checks for values
//! JavaScript has and JSON doesn't (`undefined`, `NaN`, sparse arrays,
//! getters, other realms' prototypes, cycles) have nothing to check here: a
//! `serde_json::Value` is always plain JSON.

use std::error::Error;
use std::fmt;

use serde_json::{Map, Value};

/// The keywords that constrain a value.
const CONSTRAINT_KEYWORDS: [&str; 8] = [
    "type",
    "oneOf",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
];

/// The keywords that only describe.
const ANNOTATION_KEYWORDS: [&str; 4] = ["description", "title", "default", "examples"];

/// The `type`s of the subset, in dsh's order.
const SCHEMA_TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// The keywords refused beside `oneOf`, and without `type` or `oneOf`.
const ONE_OF_SIBLING_KEYWORDS: [&str; 6] = [
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
];

/// A schema outside the subset, with every violation found, in walk order.
/// (dsh: `JsonSchemaError`, code `UNSUPPORTED_SCHEMA`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonSchemaError {
    pub violations: Vec<String>,
}

impl JsonSchemaError {
    /// dsh's code for it.
    pub const CODE: &'static str = "UNSUPPORTED_SCHEMA";
}

impl fmt::Display for JsonSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported JSON schema: {}", self.violations.join("; "))
    }
}

impl Error for JsonSchemaError {}

/// Checks that `schema` uses only the subset. (dsh:
/// `assertSupportedJsonSchema`.)
///
/// # Errors
///
/// Every violation, each named by its path from `schema`.
pub fn check_schema(schema: &Value) -> Result<(), JsonSchemaError> {
    let mut violations = Vec::new();
    check_node(schema, "schema", &mut violations);
    if violations.is_empty() {
        Ok(())
    } else {
        Err(JsonSchemaError { violations })
    }
}

/// [`check_schema`], and an object root. (dsh: `assertObjectJsonSchema`,
/// for structured outputs.)
///
/// # Errors
///
/// As [`check_schema`], or the root isn't `type: "object"`.
pub fn check_object_schema(schema: &Value) -> Result<(), JsonSchemaError> {
    check_schema(schema)?;
    if schema.get("type") != Some(&Value::String("object".into())) {
        return Err(JsonSchemaError {
            violations: vec![
                "schema.type must be \"object\" (structured output is object-rooted)".into(),
            ],
        });
    }
    Ok(())
}

/// Whether `value` is a JSON integer, as JavaScript's `Number.isInteger`
/// reads one: `1.0` is.
fn is_integer(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number.is_i64()
                || number.is_u64()
                || number
                    .as_f64()
                    .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
        }
        _ => false,
    }
}

/// Whether a scalar is of the scalar `kind`. (dsh: `scalarMatches`.)
fn scalar_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => is_integer(value),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn check_node(node: &Value, path: &str, violations: &mut Vec<String>) {
    let Some(node) = node.as_object() else {
        violations.push(format!("{path} must be a schema object"));
        return;
    };
    for key in node.keys() {
        if CONSTRAINT_KEYWORDS.contains(&key.as_str())
            || ANNOTATION_KEYWORDS.contains(&key.as_str())
        {
            continue;
        }
        violations.push(format!(
            "{path}.{key} is not a supported keyword (subset: \
             type/oneOf/properties/required/additionalProperties/items/enum/const + annotations)"
        ));
    }
    for key in ["description", "title"] {
        if node.get(key).is_some_and(|value| !value.is_string()) {
            violations.push(format!("{path}.{key} must be a string"));
        }
    }
    let has = |key: &str| node.contains_key(key);
    match (has("type"), has("oneOf")) {
        (true, true) => {
            violations.push(format!("{path} cannot declare both type and oneOf"));
        }
        (false, false) => {
            for key in ONE_OF_SIBLING_KEYWORDS {
                if has(key) {
                    violations.push(format!("{path}.{key} requires type or oneOf"));
                }
            }
        }
        (false, true) => {
            match node["oneOf"]
                .as_array()
                .filter(|branches| branches.len() >= 2)
            {
                None => violations.push(format!(
                    "{path}.oneOf must be an array of at least two schemas"
                )),
                Some(branches) => {
                    for (index, branch) in branches.iter().enumerate() {
                        check_node(branch, &format!("{path}.oneOf[{index}]"), violations);
                    }
                }
            }
            for key in ONE_OF_SIBLING_KEYWORDS {
                if has(key) {
                    violations.push(format!("{path}.{key} is not supported beside oneOf"));
                }
            }
        }
        (true, false) => check_typed(node, path, violations),
    }
}

/// A node with a `type`.
fn check_typed(node: &Map<String, Value>, path: &str, violations: &mut Vec<String>) {
    let kind = match &node["type"] {
        Value::String(kind) if SCHEMA_TYPES.contains(&kind.as_str()) => kind.as_str(),
        Value::Array(_) => {
            violations.push(format!(
                "{path}.type must be a single type string (type arrays are not supported)"
            ));
            return;
        }
        _ => {
            violations.push(format!(
                "{path}.type must be one of {}",
                SCHEMA_TYPES.join("/")
            ));
            return;
        }
    };
    let scalars = ["string", "number", "integer", "boolean", "null"];
    for (key, allowed) in [
        ("properties", &["object"][..]),
        ("required", &["object"][..]),
        ("additionalProperties", &["object"][..]),
        ("items", &["array"][..]),
        ("enum", &scalars[..]),
        ("const", &scalars[..]),
    ] {
        if node.contains_key(key) && !allowed.contains(&kind) {
            violations.push(format!("{path}.{key} is not supported on type \"{kind}\""));
        }
    }
    match kind {
        "object" => {
            let properties = node.get("properties");
            if let Some(properties) = properties {
                match properties.as_object() {
                    None => {
                        violations.push(format!("{path}.properties must be an object of schemas"))
                    }
                    Some(properties) => {
                        for (key, child) in properties {
                            check_node(child, &format!("{path}.properties.{key}"), violations);
                        }
                    }
                }
            }
            // dsh checks these after the property schemas.
            if let Some(required) = node.get("required") {
                match required
                    .as_array()
                    .filter(|names| names.iter().all(Value::is_string))
                {
                    None => violations.push(format!("{path}.required must be an array of strings")),
                    Some(names) => {
                        let declared = properties.and_then(Value::as_object);
                        for name in names.iter().filter_map(Value::as_str) {
                            if !declared.is_some_and(|declared| declared.contains_key(name)) {
                                violations.push(format!(
                                    "{path}.required names \"{name}\" which is not in properties"
                                ));
                            }
                        }
                    }
                }
            }
            if node
                .get("additionalProperties")
                .is_some_and(|open| !open.is_boolean())
            {
                violations.push(format!("{path}.additionalProperties must be a boolean"));
            }
        }
        "array" => {
            if let Some(items) = node.get("items") {
                check_node(items, &format!("{path}.items"), violations);
            }
        }
        _ => {
            let allowed = node.get("enum");
            let enum_valid = allowed.and_then(Value::as_array).is_some_and(|allowed| {
                !allowed.is_empty() && allowed.iter().all(|entry| scalar_matches(kind, entry))
            });
            if allowed.is_some() && !enum_valid {
                violations.push(format!(
                    "{path}.enum must be a non-empty array of {kind} values"
                ));
            }
            if let Some(constant) = node.get("const") {
                if !scalar_matches(kind, constant) {
                    violations.push(format!("{path}.const must be a {kind} value"));
                } else if enum_valid
                    && !allowed
                        .and_then(Value::as_array)
                        .is_some_and(|allowed| allowed.contains(constant))
                {
                    violations.push(format!(
                        "{path}.const must be one of {path}.enum when both are declared"
                    ));
                }
            }
        }
    }
}

/// The violations of `value` against `schema`, which [`check_schema`]
/// accepted, each named from `path`: an empty path names the root
/// `arguments` and its properties bare, as dsh names a tool's arguments.
/// Empty means valid. (dsh: `validateJsonSchemaValue`.)
///
/// The walk order is dsh's: an object's missing required keys, then each
/// declared property's own violations, in declaration order, then its
/// undeclared keys when it is closed.
///
/// # Panics
///
/// On a `type` outside the subset, which [`check_schema`] refuses. (dsh:
/// `assertNever`, its backstop for a forged schema.)
pub fn validate(schema: &Value, value: &Value, path: &str) -> Vec<String> {
    let shown = if path.is_empty() { "arguments" } else { path };
    let Some(node) = schema.as_object() else {
        return Vec::new();
    };
    if let Some(branches) = node.get("oneOf").and_then(Value::as_array) {
        let matches = branches
            .iter()
            .filter(|branch| validate(branch, value, path).is_empty())
            .count();
        return if matches == 1 {
            Vec::new()
        } else {
            vec![format!(
                "\"{shown}\" must match exactly one oneOf branch (matched {matches})"
            )]
        };
    }
    let Some(kind) = node.get("type") else {
        // An annotation-only schema takes any JSON.
        return Vec::new();
    };
    let mismatch = |what: &str| vec![format!("\"{shown}\" must be {what}")];
    match kind.as_str() {
        Some("object") => {
            let Some(object) = value.as_object() else {
                return mismatch("an object");
            };
            let empty = Map::new();
            let properties = node
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let mut found = Vec::new();
            for key in node
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !object.contains_key(key) {
                    found.push(format!(
                        "missing required property \"{}\"",
                        property(path, key)
                    ));
                }
            }
            for (key, child) in properties {
                if let Some(entry) = object.get(key) {
                    found.extend(validate(child, entry, &property(path, key)));
                }
            }
            if node.get("additionalProperties") == Some(&Value::Bool(false)) {
                for key in object.keys().filter(|key| !properties.contains_key(*key)) {
                    found.push(format!(
                        "\"{}\" is not a declared property (additionalProperties: false)",
                        property(path, key)
                    ));
                }
            }
            found
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                return mismatch("an array");
            };
            match node.get("items") {
                Some(item) => items
                    .iter()
                    .enumerate()
                    .flat_map(|(index, entry)| validate(item, entry, &format!("{path}[{index}]")))
                    .collect(),
                None => Vec::new(),
            }
        }
        Some("string") if !value.is_string() => mismatch("a string"),
        Some("number") if !value.is_number() => mismatch("a number"),
        Some("integer") if !is_integer(value) => mismatch("an integer"),
        Some("boolean") if !value.is_boolean() => mismatch("a boolean"),
        Some("null") if !value.is_null() => vec![format!("\"{shown}\" must be null")],
        Some("string" | "number" | "integer" | "boolean" | "null") => scalar(node, value, shown),
        other => panic!("unsupported JSON schema type {other:?}: check the schema first"),
    }
}

/// A scalar of the right type against its `enum` and `const`. (dsh:
/// `checkScalarValue`.)
fn scalar(node: &Map<String, Value>, value: &Value, shown: &str) -> Vec<String> {
    if let Some(allowed) = node.get("enum").and_then(Value::as_array)
        && !allowed.iter().any(|entry| same_scalar(entry, value))
    {
        return vec![format!(
            "\"{shown}\" must be one of {}",
            Value::Array(allowed.clone())
        )];
    }
    if let Some(constant) = node.get("const")
        && !same_scalar(constant, value)
    {
        return vec![format!("\"{shown}\" must be {constant}")];
    }
    Vec::new()
}

/// JavaScript's `===` on JSON scalars: `1` and `1.0` are the same number.
fn same_scalar(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => a == b,
    }
}

/// dsh's `propertyPath`.
fn property(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

//! The part of dsh's argument validator a tool here needs, with its exact
//! wording: `type` (object, array, string, integer, number, boolean, null),
//! `properties`, `required`, `additionalProperties: false`, `items` and
//! `enum`. (dsh: `validateJsonSchemaValue` in `tools/src/json-schema.ts`.)
//!
//! dsh's registry runs it before every typed tool; Karaka's registry
//! doesn't validate yet, so the tool runs it itself.

use serde_json::Value;

/// The violations of `value` against `schema`, in dsh's walk order: an
/// object's missing required keys, then each declared property's own, in
/// declaration order, then its undeclared keys. Empty means valid.
pub(crate) fn violations(schema: &Value, value: &Value) -> Vec<String> {
    check(schema, value, "")
}

/// dsh's `diagnosticPath`: the root is `arguments`.
fn shown(path: &str) -> &str {
    if path.is_empty() { "arguments" } else { path }
}

/// dsh's `propertyPath`.
fn property(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn check(schema: &Value, value: &Value, path: &str) -> Vec<String> {
    let kind = schema.get("type").and_then(Value::as_str);
    let mismatch = |what: &str| vec![format!("\"{}\" must be {what}", shown(path))];
    match kind {
        Some("object") => {
            let Some(object) = value.as_object() else {
                return mismatch("an object");
            };
            let empty = serde_json::Map::new();
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let mut found = Vec::new();
            for key in schema
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
                    found.extend(check(child, entry, &property(path, key)));
                }
            }
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
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
            match schema.get("items") {
                Some(item) => items
                    .iter()
                    .enumerate()
                    .flat_map(|(index, entry)| check(item, entry, &format!("{path}[{index}]")))
                    .collect(),
                None => Vec::new(),
            }
        }
        Some("string") if !value.is_string() => mismatch("a string"),
        Some("number") if !value.is_number() => mismatch("a number"),
        Some("integer") if !(value.is_i64() || value.is_u64()) => mismatch("an integer"),
        Some("boolean") if !value.is_boolean() => mismatch("a boolean"),
        Some("null") if !value.is_null() => vec![format!("\"{}\" must be null", shown(path))],
        _ => match schema.get("enum").and_then(Value::as_array) {
            Some(allowed) if !allowed.contains(value) => vec![format!(
                "\"{}\" must be one of {}",
                shown(path),
                Value::Array(allowed.clone())
            )],
            _ => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::violations;

    #[test]
    fn the_root_is_named_arguments_and_scalars_are_typed() {
        let schema = json!({ "type": "object" });
        assert_eq!(
            violations(&schema, &json!([])),
            [r#""arguments" must be an object"#]
        );
        let numbers = json!({ "type": "object", "properties": {
            "n": { "type": "integer" }, "b": { "type": "boolean" }, "z": { "type": "null" },
        } });
        assert_eq!(
            violations(&numbers, &json!({ "n": 1.5, "b": 1, "z": 0 })),
            [
                r#""n" must be an integer"#,
                r#""b" must be a boolean"#,
                r#""z" must be null"#
            ]
        );
    }
}

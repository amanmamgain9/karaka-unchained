//! A skill file's YAML frontmatter, and the fields read from it. (dsh:
//! `parseFrontmatter`, `findClosingFrontmatter`, `stringField`,
//! `optionalString`, `parseInvocationPolicy`, `frontmatterBoolean` and
//! `optionalMetadata` in `skill-filesystem/src/index.ts`.)

use karaka_skill::SkillInvocationPolicy;
use serde_json::{Map, Number, Value};
use yaml_rust2::{Yaml, YamlLoader};

/// A file's frontmatter, as JSON, and the body after it.
#[derive(Debug, PartialEq)]
pub(crate) struct Frontmatter {
    pub(crate) data: Map<String, Value>,
    pub(crate) body: String,
}

/// The frontmatter of `raw`: a first line of exactly `---`, YAML, and a
/// closing line of exactly `---`, either line ending in `\r\n` or `\n`.
/// `Ok(None)` when there is none, or its YAML isn't a mapping; `Err` with
/// the parser's message when the YAML doesn't parse. (dsh:
/// `parseFrontmatter`.)
pub(crate) fn parse_frontmatter(raw: &str) -> Result<Option<Frontmatter>, String> {
    let Some(first_end) = raw.find('\n') else {
        return Ok(None);
    };
    if raw[..first_end]
        .strip_suffix('\r')
        .unwrap_or(&raw[..first_end])
        != "---"
    {
        return Ok(None);
    }
    let start = first_end + 1;
    let Some((yaml_end, body_start)) = closing(raw, start) else {
        return Ok(None);
    };
    let documents =
        YamlLoader::load_from_str(&raw[start..yaml_end]).map_err(|error| error.to_string())?;
    let Some(Value::Object(data)) = documents.first().map(json) else {
        return Ok(None);
    };
    Ok(Some(Frontmatter {
        data,
        body: raw[body_start..].to_owned(),
    }))
}

/// Where the closing `---` line starts, and where the body after it does.
/// (dsh: `findClosingFrontmatter`.)
fn closing(raw: &str, start: usize) -> Option<(usize, usize)> {
    let mut line_start = start;
    while line_start <= raw.len() {
        let next = raw[line_start..].find('\n').map(|at| line_start + at);
        let line_end = next.unwrap_or(raw.len());
        let line = &raw[line_start..line_end];
        if line.strip_suffix('\r').unwrap_or(line) == "---" {
            return Some((line_start, next.map_or(raw.len(), |at| at + 1)));
        }
        line_start = next? + 1;
    }
    None
}

/// A YAML value as the JSON the `yaml` package gives: a mapping's keys
/// become strings, and an unresolvable value is null.
fn json(yaml: &Yaml) -> Value {
    match yaml {
        Yaml::Real(text) => text
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map_or(Value::Null, Value::Number),
        Yaml::Integer(value) => Value::Number((*value).into()),
        Yaml::String(text) => Value::String(text.clone()),
        Yaml::Boolean(value) => Value::Bool(*value),
        Yaml::Array(items) => Value::Array(items.iter().map(json).collect()),
        Yaml::Hash(entries) => Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key_text(key), json(value)))
                .collect(),
        ),
        Yaml::Alias(_) | Yaml::Null | Yaml::BadValue => Value::Null,
    }
}

/// A mapping key as JavaScript's property name for it.
fn key_text(key: &Yaml) -> String {
    match key {
        Yaml::String(text) | Yaml::Real(text) => text.clone(),
        Yaml::Integer(value) => value.to_string(),
        Yaml::Boolean(value) => value.to_string(),
        Yaml::Null => "null".into(),
        other => json(other).to_string(),
    }
}

/// A non-empty string field. (dsh: `stringField`.)
pub(crate) fn string_field(data: &Map<String, Value>, key: &str) -> Option<String> {
    match data.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// The `metadata` mapping, if it is one. (dsh: `optionalMetadata`.)
pub(crate) fn metadata(data: &Map<String, Value>) -> Option<Map<String, Value>> {
    data.get("metadata").and_then(Value::as_object).cloned()
}

/// The invocation policy: `disable-model-invocation` and `user-invocable`,
/// with the legacy spellings refused. An error reads as dsh renders the
/// one it throws, with its `Error:` or `TypeError:` name. (dsh:
/// `parseInvocationPolicy`.)
pub(crate) fn invocation_policy(
    data: &Map<String, Value>,
) -> Result<SkillInvocationPolicy, String> {
    for (legacy, canonical) in [
        ("disableModelInvocation", "disable-model-invocation"),
        ("modelInvocable", "disable-model-invocation"),
        ("userInvocable", "user-invocable"),
    ] {
        if data.contains_key(legacy) {
            return Err(format!(
                "Error: frontmatter field \"{legacy}\" is unsupported; use \"{canonical}\""
            ));
        }
    }
    let disable_model = boolean(data, "disable-model-invocation")?;
    let user = boolean(data, "user-invocable")?;
    Ok(SkillInvocationPolicy {
        model_invocable: disable_model != Some(true),
        user_invocable: user != Some(false),
    })
}

/// A boolean field, in any of the spellings dsh takes: a boolean, 1 or 0,
/// and, in any case, `true`, `yes`, `on`, `false`, `no` or `off`. (dsh:
/// `frontmatterBoolean`.)
fn boolean(data: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    let Some(value) = data.get(key) else {
        return Ok(None);
    };
    let refused = || {
        Err(format!(
            "TypeError: frontmatter field \"{key}\" must be a boolean"
        ))
    };
    match value {
        Value::Bool(value) => Ok(Some(*value)),
        Value::Number(number) => match number.as_f64() {
            Some(1.0) => Ok(Some(true)),
            Some(0.0) => Ok(Some(false)),
            _ => refused(),
        },
        Value::String(text) => match text.to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => refused(),
        },
        _ => refused(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_frontmatter;

    #[test]
    fn frontmatter_needs_both_delimiter_lines_and_a_mapping() {
        let parsed = parse_frontmatter("---\nname: a\n---\nbody\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::Value::Object(parsed.data),
            json!({ "name": "a" })
        );
        assert_eq!(parsed.body, "body\n");
        let crlf = parse_frontmatter("---\r\nname: a\r\n---\r\nbody")
            .unwrap()
            .unwrap();
        assert_eq!(crlf.body, "body");
        // A closing line with nothing after it.
        assert_eq!(
            parse_frontmatter("---\nname: a\n---")
                .unwrap()
                .unwrap()
                .body,
            ""
        );
        for raw in [
            "No frontmatter.",
            "---",
            "--- \nname: a\n---\n",
            "---\nname: a",
            "---\n[]\n---\n",
            "---\n---\n",
        ] {
            assert_eq!(parse_frontmatter(raw).unwrap(), None, "{raw:?}");
        }
        assert!(parse_frontmatter("---\nname: [unclosed\n---\n").is_err());
    }
}

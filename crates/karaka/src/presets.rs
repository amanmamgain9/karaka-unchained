//! `KARAKA_PRESETS`: agent presets read from a YAML file of preset rows.
//!
//! The file is a list of presets, each shaped like a dsh `agent-preset`
//! row's config, as Play This Life declares its presets:
//!
//! ```yaml
//! - id: coach
//!   name: Coach
//!   description: Keeps a person on track.
//!   order: 1
//!   plugins:
//!     - name: persona
//!       config:
//!         prefix: You are a patient coach.
//!     - name: tool-policy
//!       config:
//!         allow: [todo_write]
//! ```
//!
//! A plugin row names one of three plugins, by its short name or its
//! package's, and may give an `id` for diagnostics:
//!
//! - `persona` (`@deepseek-ai/dsh-persona`): `prefix` (required), `suffix`,
//!   `complete`, `includeRuntimeContext`;
//! - `tool-policy` (`@karaka-ai/agent/tool-policy`): `mode` (`native`),
//!   `allow`, `deny`;
//! - `skill-filesystem` (`@deepseek-ai/dsh-skill-filesystem`):
//!   `includeDefaultRoots`, `customSkillDirs`, `watch`. A relative
//!   directory is read from the file's own directory.
//!
//! Anything else, an unknown key included, fails the start with a message
//! that says where. `KARAKA_PRESET` names the default preset; without it,
//! the file's first preset is the default.

use std::path::Path;

use karaka_agent_presets::{
    AgentPresetsConfig, AgentPresetsPlugin, PresetContext, PresetDefinition, PresetFuture,
    PresetPlugin,
};
use karaka_kernel::PortId;
use karaka_persona::Persona;
use karaka_skill_filesystem::{SkillFilesystemConfig, SkillFilesystemPresetPlugin};
use karaka_tool_policy::{ToolMode, ToolPolicy};
use serde_json::{Map, Number, Value};
use yaml_rust2::{Yaml, YamlLoader};

/// The registry's plugin for the presets in the file at `path`, with
/// `default` as the default preset, or the file's first. `watch_skills` is
/// a `skill-filesystem` row's `watch` when it gives none.
///
/// # Errors
///
/// A file that doesn't read or parse, a row that doesn't fit, or a default
/// the file has no preset for, each said with where it is.
pub fn load(
    path: &Path,
    default: Option<&str>,
    watch_skills: bool,
) -> Result<AgentPresetsPlugin, String> {
    let at = format!("KARAKA_PRESETS ({})", path.display());
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("{at}: couldn't read it: {error}"))?;
    let base = path.parent().unwrap_or(Path::new("."));
    parse(&text, &at, base, default, watch_skills)
}

/// [`load`] for the file's `text`; `at` names the file in errors, and
/// `base` is where relative directories are read from.
pub fn parse(
    text: &str,
    at: &str,
    base: &Path,
    default: Option<&str>,
    watch_skills: bool,
) -> Result<AgentPresetsPlugin, String> {
    let documents =
        YamlLoader::load_from_str(text).map_err(|error| format!("{at}: not YAML: {error}"))?;
    let rows = match documents.first().map(json) {
        Some(Value::Array(rows)) => rows,
        _ => return Err(format!("{at}: must be a list of presets")),
    };
    if rows.is_empty() {
        return Err(format!("{at}: holds no presets"));
    }
    let mut definitions = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        definitions.push(preset(
            row,
            &format!("{at}: preset {}", index + 1),
            base,
            watch_skills,
        )?);
    }
    let ids: Vec<String> = definitions
        .iter()
        .map(|definition| definition.id.clone())
        .collect();
    let default = match default {
        Some(id) if ids.iter().any(|known| known == id) => id.to_owned(),
        Some(id) => {
            return Err(format!(
                "KARAKA_PRESET is \"{id}\", but {at} has no such preset; its presets are {}",
                ids.join(", ")
            ));
        }
        None => ids[0].clone(),
    };
    Ok(definitions.into_iter().fold(
        AgentPresetsPlugin::new(AgentPresetsConfig::new(default)),
        AgentPresetsPlugin::preset,
    ))
}

fn preset(
    row: &Value,
    at: &str,
    base: &Path,
    watch_skills: bool,
) -> Result<PresetDefinition, String> {
    let row = object(row, at, &["id", "name", "description", "order", "plugins"])?;
    let id = string(row, "id", at)?.ok_or(format!("{at} needs an \"id\""))?;
    let at = format!("{at} (\"{id}\")");
    let mut plugins: Vec<Box<dyn PresetPlugin>> = Vec::new();
    let rows = match row.get("plugins") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(rows)) => rows.clone(),
        Some(_) => return Err(format!("{at}: \"plugins\" must be a list")),
    };
    for (index, plugin) in rows.iter().enumerate() {
        plugins.push(plugin_row(
            plugin,
            &format!("{at}, plugin {}", index + 1),
            base,
            watch_skills,
        )?);
    }
    let order = match row.get("order") {
        None | Some(Value::Null) => None,
        Some(Value::Number(order)) => order.as_f64(),
        Some(_) => return Err(format!("{at}: \"order\" must be a number")),
    };
    Ok(PresetDefinition {
        name: string(row, "name", &at)?,
        description: string(row, "description", &at)?,
        order,
        plugins,
        id,
    })
}

fn plugin_row(
    row: &Value,
    at: &str,
    base: &Path,
    watch_skills: bool,
) -> Result<Box<dyn PresetPlugin>, String> {
    let row = object(row, at, &["id", "name", "config"])?;
    let name = string(row, "name", at)?.ok_or(format!("{at} needs a \"name\""))?;
    let empty = Value::Object(Map::new());
    let config = match row.get("config") {
        None | Some(Value::Null) => &empty,
        Some(config) => config,
    };
    let at = format!("{at} ({name})");
    let plugin: Box<dyn PresetPlugin> = match name.as_str() {
        "persona" | "@deepseek-ai/dsh-persona" => Box::new(persona(config, &at)?),
        "tool-policy" | "@karaka-ai/agent/tool-policy" => Box::new(tool_policy(config, &at)?),
        "skill-filesystem" | "@deepseek-ai/dsh-skill-filesystem" => {
            Box::new(skill_filesystem(config, &at, base, watch_skills)?)
        }
        other => {
            return Err(format!(
                "{at}: unknown plugin \"{other}\"; a preset may use persona, tool-policy and \
                 skill-filesystem"
            ));
        }
    };
    Ok(match string(row, "id", &at)? {
        Some(id) => Box::new(Labelled { id, plugin }),
        None => plugin,
    })
}

fn persona(config: &Value, at: &str) -> Result<Persona, String> {
    let config = object(
        config,
        at,
        &["prefix", "suffix", "complete", "includeRuntimeContext"],
    )?;
    let mut persona =
        Persona::new(string(config, "prefix", at)?.ok_or(format!("{at} needs a \"prefix\""))?);
    if let Some(suffix) = string(config, "suffix", at)? {
        persona.suffix = suffix;
    }
    if let Some(complete) = boolean(config, "complete", at)? {
        persona.complete = complete;
    }
    if let Some(include) = boolean(config, "includeRuntimeContext", at)? {
        persona.include_runtime_context = include;
    }
    Ok(persona)
}

fn tool_policy(config: &Value, at: &str) -> Result<ToolPolicy, String> {
    let config = object(config, at, &["mode", "allow", "deny"])?;
    match string(config, "mode", at)?.as_deref() {
        None | Some("native") => {}
        Some("ptc") => {
            return Err(format!(
                "{at}: mode \"ptc\" isn't supported yet; it waits on the MCP client"
            ));
        }
        Some(other) => {
            return Err(format!(
                "{at}: \"mode\" must be \"native\", not \"{other}\""
            ));
        }
    }
    // An omitted list stays omitted, so it masks nothing.
    Ok(ToolPolicy {
        mode: ToolMode::Native,
        allow: strings(config, "allow", at)?,
        deny: strings(config, "deny", at)?,
    })
}

fn skill_filesystem(
    config: &Value,
    at: &str,
    base: &Path,
    watch_skills: bool,
) -> Result<SkillFilesystemPresetPlugin, String> {
    let config = object(
        config,
        at,
        &["includeDefaultRoots", "customSkillDirs", "watch"],
    )?;
    let mut skills = SkillFilesystemConfig {
        watch: watch_skills,
        ..SkillFilesystemConfig::default()
    };
    if let Some(include) = boolean(config, "includeDefaultRoots", at)? {
        skills.include_default_roots = include;
    }
    if let Some(watch) = boolean(config, "watch", at)? {
        skills.watch = watch;
    }
    if let Some(dirs) = strings(config, "customSkillDirs", at)? {
        skills.custom_skill_dirs = dirs
            .into_iter()
            .map(|dir| base.join(dir).to_string_lossy().into_owned())
            .collect();
    }
    skills
        .validate()
        .map_err(|error| format!("{at}: {error}"))?;
    Ok(SkillFilesystemPresetPlugin::new(skills))
}

/// A plugin row with an `id` of its own, which its diagnostics name.
struct Labelled {
    id: String,
    plugin: Box<dyn PresetPlugin>,
}

impl PresetPlugin for Labelled {
    fn name(&self) -> &str {
        self.plugin.name()
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn requires(&self) -> Vec<PortId> {
        self.plugin.requires()
    }

    fn setup<'a, 'p>(&'a self, ctx: &'a PresetContext<'a, 'p>) -> PresetFuture<'a> {
        self.plugin.setup(ctx)
    }
}

/// `value` as a mapping holding only `keys`.
fn object<'a>(value: &'a Value, at: &str, keys: &[&str]) -> Result<&'a Map<String, Value>, String> {
    let Value::Object(map) = value else {
        return Err(format!("{at} must be a mapping"));
    };
    if let Some(unknown) = map.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(format!(
            "{at}: unknown key \"{unknown}\"; it may have {}",
            keys.join(", ")
        ));
    }
    Ok(map)
}

fn string(map: &Map<String, Value>, key: &str, at: &str) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{at}: \"{key}\" must be a string")),
    }
}

fn boolean(map: &Map<String, Value>, key: &str, at: &str) -> Result<Option<bool>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("{at}: \"{key}\" must be true or false")),
    }
}

fn strings(map: &Map<String, Value>, key: &str, at: &str) -> Result<Option<Vec<String>>, String> {
    let wrong = || format!("{at}: \"{key}\" must be a list of strings");
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(wrong))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(wrong()),
    }
}

/// A YAML value as JSON, a mapping's keys as strings.
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
                .map(|(key, value)| {
                    let key = match key {
                        Yaml::String(text) | Yaml::Real(text) => text.clone(),
                        other => json(other).to_string(),
                    };
                    (key, json(value))
                })
                .collect(),
        ),
        Yaml::Alias(_) | Yaml::Null | Yaml::BadValue => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::parse;

    fn refused(text: &str) -> String {
        match parse(text, "presets.yml", Path::new("/base"), None, false) {
            Ok(_) => panic!("expected {text:?} to be refused"),
            Err(message) => message,
        }
    }

    #[test]
    fn a_file_of_rows_parses() {
        let text = "
- id: coach
  name: Coach
  order: 2
  plugins:
    - name: persona
      config: { prefix: Coach., suffix: '', complete: false, includeRuntimeContext: true }
    - id: policy
      name: '@karaka-ai/agent/tool-policy'
      config: { mode: native, allow: [todo_write] }
    - name: skill-filesystem
      config: { includeDefaultRoots: false, customSkillDirs: [skills], watch: false }
- id: plain
";
        assert!(parse(text, "presets.yml", Path::new("/base"), None, false).is_ok());
        assert!(
            parse(
                text,
                "presets.yml",
                Path::new("/base"),
                Some("plain"),
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn mistakes_are_said_with_where_they_are() {
        for (text, message) in [
            ("{}", "presets.yml: must be a list of presets"),
            ("[]", "presets.yml: holds no presets"),
            ("- name: x", "presets.yml: preset 1 needs an \"id\""),
            (
                "- id: a\n  colour: red",
                "presets.yml: preset 1: unknown key \"colour\"; it may have id, name, description, order, plugins",
            ),
            (
                "- id: a\n  plugins: [{ name: shell }]",
                "presets.yml: preset 1 (\"a\"), plugin 1 (shell): unknown plugin \"shell\"; a preset may use persona, tool-policy and skill-filesystem",
            ),
            (
                "- id: a\n  plugins: [{ name: persona }]",
                "presets.yml: preset 1 (\"a\"), plugin 1 (persona) needs a \"prefix\"",
            ),
            (
                "- id: a\n  plugins: [{ name: persona, config: { prefix: 1 } }]",
                "presets.yml: preset 1 (\"a\"), plugin 1 (persona): \"prefix\" must be a string",
            ),
            (
                "- id: a\n  plugins: [{ name: tool-policy, config: { allow: read } }]",
                "presets.yml: preset 1 (\"a\"), plugin 1 (tool-policy): \"allow\" must be a list of strings",
            ),
            (
                "- id: a\n  plugins: [{ name: tool-policy, config: { mode: ptc } }]",
                "presets.yml: preset 1 (\"a\"), plugin 1 (tool-policy): mode \"ptc\" isn't supported yet; it waits on the MCP client",
            ),
        ] {
            assert_eq!(refused(text), message, "{text}");
        }
        let Err(message) = parse(
            "- id: a\n- id: b",
            "presets.yml",
            Path::new("/"),
            Some("c"),
            false,
        ) else {
            panic!("expected an unknown default to be refused");
        };
        assert_eq!(
            message,
            "KARAKA_PRESET is \"c\", but presets.yml has no such preset; its presets are a, b"
        );
    }
}

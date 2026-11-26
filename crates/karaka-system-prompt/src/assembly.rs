//! An assembly, and how it renders: the prompt's sections joined, and the
//! runtime context's snapshot. (dsh: `PromptAssembly`, `renderPrompt`,
//! `renderContextSections`, `joinContextSections` and
//! `renderContextSnapshot` in `core/system-prompt/src/index.ts`.)

use std::fmt;

use karaka_model::ToolSchema;

/// Why an assembly or its rendering failed, in dsh's words. A malformed
/// prompt is worse than a loud failure, so nothing is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptError(pub(crate) String);

impl PromptError {
    /// A failure with `message`, such as a listener's.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PromptError {}

/// One section of an assembly, its text resolved but not yet interpolated.
/// (dsh: `AssembledSection`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembledSection {
    pub name: String,
    pub text: String,
    /// `false` keeps the text literal, `{{` and all.
    pub interpolate: bool,
}

impl AssembledSection {
    /// A section whose text is interpolated.
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
            interpolate: true,
        }
    }
}

/// One runtime-context contribution of an assembly. (dsh:
/// `AssembledContext`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembledContext {
    pub name: String,
    pub text: String,
    pub interpolate: bool,
}

impl AssembledContext {
    /// A context whose text is interpolated.
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
            interpolate: true,
        }
    }
}

/// The prompt variables of one assembly, in the order they were first set.
/// A name with `None` is registered but has no value here. (dsh: the
/// assembly's `variables` record.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Variables(Vec<(String, Option<String>)>);

impl Variables {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `name`, keeping its place if it was set before.
    pub fn set(&mut self, name: impl Into<String>, value: Option<String>) {
        let name = name.into();
        match self.0.iter_mut().find(|(known, _)| *known == name) {
            Some(entry) => entry.1 = value,
            None => self.0.push((name, value)),
        }
    }

    /// `Some(value)` for a registered name, `None` for an unknown one.
    pub fn get(&self, name: &str) -> Option<Option<&str>> {
        self.0
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_deref())
    }

    /// The registered names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What one model step is told: the prompt's sections, the runtime
/// context, the tools and the variables. Sections and contexts stay
/// uninterpolated until rendered; tools are in their canonical order.
/// (dsh: `PromptAssembly`.)
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromptAssembly {
    pub sections: Vec<AssembledSection>,
    pub contexts: Vec<AssembledContext>,
    pub tools: Vec<ToolSchema>,
    pub variables: Variables,
}

/// One named part of a runtime-context snapshot, rendered. (dsh:
/// `ContextSnapshotSection`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextSection {
    pub name: String,
    pub text: String,
}

/// The system prompt: every section interpolated, the empty ones left out,
/// joined with blank lines; `""` when all are empty. (dsh: `renderPrompt`.)
///
/// # Errors
///
/// A malformed reference, an unknown variable, or a variable with no value.
pub fn render_prompt(assembly: &PromptAssembly) -> Result<String, PromptError> {
    let mut rendered = Vec::new();
    for section in &assembly.sections {
        let text = if section.interpolate {
            interpolate(&section.name, &section.text, &assembly.variables, "section")?
        } else {
            section.text.clone()
        };
        if !text.is_empty() {
            rendered.push(text);
        }
    }
    Ok(rendered.join("\n\n"))
}

/// The runtime context, one entry per contribution that rendered to some
/// text. (dsh: `renderContextSections`.)
///
/// # Errors
///
/// As [`render_prompt`].
pub fn render_context_sections(
    assembly: &PromptAssembly,
) -> Result<Vec<ContextSection>, PromptError> {
    let mut sections = Vec::new();
    for context in &assembly.contexts {
        let text = if context.interpolate {
            interpolate(&context.name, &context.text, &assembly.variables, "context")?
        } else {
            context.text.clone()
        };
        if !text.is_empty() {
            sections.push(ContextSection {
                name: context.name.clone(),
                text,
            });
        }
    }
    Ok(sections)
}

/// The snapshot the model reads for rendered `sections`, or `""` when there
/// are none. (dsh: `joinContextSections`.)
pub fn join_context_sections(sections: &[ContextSection]) -> String {
    let body: Vec<&str> = sections
        .iter()
        .map(|section| section.text.as_str())
        .collect();
    let body = body.join("\n\n");
    if body.is_empty() {
        return body;
    }
    format!(
        "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\n\n{body}"
    )
}

/// The whole runtime-context snapshot. (dsh: `renderContextSnapshot`.)
///
/// # Errors
///
/// As [`render_prompt`].
pub fn render_context_snapshot(assembly: &PromptAssembly) -> Result<String, PromptError> {
    Ok(join_context_sections(&render_context_sections(assembly)?))
}

/// A variable name, as it is written between the braces: `[a-z][a-z0-9_]*`.
pub(crate) fn valid_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Replaces each `{{name}}` in `text` with its variable's value, strictly.
/// A lone `{{` with no `}}` anywhere after it is prose; a substituted value
/// is never scanned again. (dsh: `interpolate`.)
fn interpolate(
    owner: &str,
    text: &str,
    variables: &Variables,
    kind: &str,
) -> Result<String, PromptError> {
    let mut result = String::new();
    let mut last = 0;
    while let Some(found) = text[last..].find("{{") {
        let open = last + found;
        let rest = &text[open + 2..];
        // A complete group: `{{`, no braces, then `}}`.
        let group = rest
            .find(['{', '}'])
            .filter(|end| rest[*end..].starts_with("}}"))
            .map(|end| &rest[..end]);
        let Some(name) = group else {
            // A later closing brace makes this malformed; otherwise it is
            // literal prose.
            if rest.contains("}}") {
                // dsh shows 16 UTF-16 code units of it.
                let units: Vec<u16> = text[open..].encode_utf16().take(16).collect();
                let shown = String::from_utf16_lossy(&units);
                return Err(PromptError::new(format!(
                    "malformed prompt variable reference at \"{shown}…\" in {kind} \"{owner}\" \
                     (references are complete simple {{{{name}}}} groups)"
                )));
            }
            result.push_str(&text[last..open + 2]);
            last = open + 2;
            continue;
        };
        if !valid_variable_name(name) {
            return Err(PromptError::new(format!(
                "malformed prompt variable reference \"{{{{{name}}}}}\" in {kind} \"{owner}\" \
                 (variable names match /^[a-z][a-z0-9_]*$/)"
            )));
        }
        let value = match variables.get(name) {
            None => {
                let known: Vec<&str> = variables.names().collect();
                let known = if known.is_empty() {
                    "(none)".to_owned()
                } else {
                    known.join(", ")
                };
                return Err(PromptError::new(format!(
                    "unknown prompt variable \"{{{{{name}}}}}\" in {kind} \"{owner}\"; registered \
                     variables: {known}"
                )));
            }
            Some(None) => {
                return Err(PromptError::new(format!(
                    "prompt variable \"{{{{{name}}}}}\" has no value for this assembly ({kind} \
                     \"{owner}\")"
                )));
            }
            Some(Some(value)) => value,
        };
        result.push_str(&text[last..open]);
        result.push_str(value);
        last = open + 2 + name.len() + 2;
    }
    result.push_str(&text[last..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assembly(sections: &[(&str, &str)], variables: &[(&str, Option<&str>)]) -> PromptAssembly {
        let mut values = Variables::new();
        for (name, value) in variables {
            values.set(*name, value.map(str::to_owned));
        }
        PromptAssembly {
            sections: sections
                .iter()
                .map(|(name, text)| AssembledSection::new(*name, *text))
                .collect(),
            variables: values,
            ..PromptAssembly::default()
        }
    }

    fn rendered(
        sections: &[(&str, &str)],
        variables: &[(&str, Option<&str>)],
    ) -> Result<String, String> {
        render_prompt(&assembly(sections, variables)).map_err(|error| error.to_string())
    }

    // "filters out empty section text from renderPrompt"
    #[test]
    fn empty_sections_are_left_out() {
        assert_eq!(
            rendered(&[("empty", ""), ("real", "content")], &[]),
            Ok("content".into())
        );
    }

    // "names "(none)" when no variables are registered at all"
    #[test]
    fn an_unknown_variable_names_the_registered_ones() {
        assert_eq!(
            rendered(&[("s", "{{x}}")], &[]),
            Err(
                "unknown prompt variable \"{{x}}\" in section \"s\"; registered variables: (none)"
                    .into()
            )
        );
        assert_eq!(
            rendered(&[("persona", "on {{modle}}")], &[("model", Some("m"))]),
            Err("unknown prompt variable \"{{modle}}\" in section \"persona\"; registered variables: model".into())
        );
        // "rejects {{constructor}} as UNKNOWN": no prototype to fall back on.
        assert!(
            rendered(&[("s", "on {{constructor}}")], &[("model", Some("m"))])
                .unwrap_err()
                .contains("unknown prompt variable \"{{constructor}}\"")
        );
        assert_eq!(
            rendered(
                &[("s", "{{constructor}}")],
                &[("constructor", Some("own-value"))]
            ),
            Ok("own-value".into())
        );
    }

    // "throws when a referenced variable has no value for this assembly"
    #[test]
    fn a_variable_with_no_value_fails() {
        assert_eq!(
            rendered(&[("persona", "in {{cwd}}")], &[("cwd", None)]),
            Err(
                "prompt variable \"{{cwd}}\" has no value for this assembly (section \"persona\")"
                    .into()
            )
        );
    }

    // "throws on a malformed complete reference, e.g. inner spaces", and
    // "throws on a mangled reference with a }} still following"
    #[test]
    fn malformed_references_fail() {
        assert!(
            rendered(&[("s", "on {{ model }}")], &[("model", Some("m"))])
                .unwrap_err()
                .contains("malformed prompt variable reference \"{{ model }}\" in section \"s\"")
        );
        for text in ["{{{model}}}", "x {{a{b}} y {{model}}"] {
            assert!(
                rendered(&[("s", text)], &[("model", Some("m"))])
                    .unwrap_err()
                    .contains("malformed prompt variable reference at"),
                "{text}"
            );
        }
    }

    // "leaves a lone {{ verbatim only when NO }} follows anywhere after it"
    #[test]
    fn a_lone_open_brace_is_prose() {
        assert_eq!(
            rendered(&[("s", "shell ${X:-{{fallback} stays")], &[]),
            Ok("shell ${X:-{{fallback} stays".into())
        );
    }

    // "never re-scans substituted values"
    #[test]
    fn a_value_is_never_scanned_again() {
        assert_eq!(
            rendered(
                &[("s", "v = {{model}}!")],
                &[("model", Some("literal {{sneaky}} inside"))]
            ),
            Ok("v = literal {{sneaky}} inside!".into())
        );
    }

    // "attributes context interpolation failures to the contributing context"
    #[test]
    fn a_context_failure_names_the_context() {
        let assembly = PromptAssembly {
            contexts: vec![AssembledContext::new("policy", "Mode: {{missing}}.")],
            ..PromptAssembly::default()
        };
        assert_eq!(
            render_context_snapshot(&assembly).unwrap_err().to_string(),
            "unknown prompt variable \"{{missing}}\" in context \"policy\"; registered variables: (none)"
        );
    }

    #[test]
    fn variable_names_are_lowercase_identifiers() {
        assert!(valid_variable_name("response_language"));
        assert!(valid_variable_name("a1"));
        assert!(!valid_variable_name("Not Valid"));
        assert!(!valid_variable_name("1a"));
        assert!(!valid_variable_name(""));
    }
}

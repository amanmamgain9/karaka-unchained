//! The session's skill catalog, and user-explicit `/name` invocations, both
//! added to a step's messages by the pre-step hook. (dsh: the two
//! `agent/pre-step` listeners of `tool-skill/src/index.ts`, and
//! `renderCatalogMessage`, `renderCatalogUpdate`, `catalogHistory`,
//! `catalogMessage`, `catalogDescription` and `invokedSkillNames`.)
//!
//! # The catalog
//!
//! Before each step, the model-invocable skills the calling agent sees are
//! compared with the catalog it last saw: the latest readable
//! `skill-catalog` message still on the session's surface. When they
//! differ, a catalog joins the step: the first one this session, or a
//! complete replacement, an empty one included, after that. A catalog
//! already among the step's messages is kept when it matches, replaced
//! when it doesn't, and dropped when the visible one already matches. A
//! session that never had skills gets none. While any provider's discovery
//! is incomplete, nothing changes, so the last good catalog stands.
//!
//! A catalog is identified by its source's entries, `{ name, description }`
//! each, never by its prose. dsh compares digests of the entries; this
//! compares the entries.
//!
//! # Invocations
//!
//! A `/name` token, bounded by whitespace or the text's ends, in a text
//! block of a message the person sent, names a skill to load. Each named
//! skill that is user-invocable joins the step, after everything else, as a
//! `skill-invocation` message holding its rendered body. Any other token is
//! plain prose.

use std::collections::HashSet;
use std::sync::Arc;

use karaka_agent::{PreStep, PreStepDecision, PreStepFuture, PreStepListener, PreStepNext};
use karaka_session::{ContentBlock, Session, SessionEvent, SessionStore, UserMessage, UserSource};
use karaka_skill::{
    Invocable, SkillCatalogSnapshot, SkillLookupOptions, SkillRegistry, escape_text, is_skill_name,
    render_skill_content, skill_invocation_source,
};
use karaka_tools::{Tool, ToolRegistry};
use karaka_working_directory::WorkingDirectory;
use serde_json::{Value, json};

/// The `kind` of a catalog's message source. (dsh: `SkillCatalogSource`.)
pub const CATALOG_KIND: &str = "skill-catalog";

/// One catalog line: a skill's name and its normalized description.
pub type Entry = (String, String);

/// What both listeners need.
pub(crate) struct Shared {
    pub(crate) skills: Arc<SkillRegistry>,
    /// The registry the tool is in, and the tool this plugin registered:
    /// the catalog is published only while the agent sees that very tool.
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) directories: Arc<WorkingDirectory>,
    pub(crate) sessions: Arc<SessionStore>,
    pub(crate) description_max_length: usize,
}

impl Shared {
    /// The calling agent's lookup: its working directory and its scope.
    async fn lookup(
        &self,
        step: &PreStep,
    ) -> Result<SkillLookupOptions, karaka_agent::HandlerError> {
        let cwd = self.directories.ensure(step.agent.as_ref(), None).await?;
        Ok(SkillLookupOptions {
            cwd: Some(cwd),
            signal: None,
            scope: step.agent.scope(),
        })
    }
}

/// Adds the catalog. (dsh: the second `agent/pre-step` listener.)
pub(crate) struct CatalogListener(pub(crate) Arc<Shared>);

impl PreStepListener for CatalogListener {
    fn decide<'a>(&'a self, step: &'a PreStep, next: PreStepNext<'a>) -> PreStepFuture<'a> {
        Box::pin(async move {
            let PreStepDecision::Enter { mut messages } = next.run().await? else {
                return Ok(PreStepDecision::Reject);
            };
            let shared = &self.0;
            // dsh's `toolVisible`: a restricted agent, or one with a scoped
            // tool of the same name, gets no catalog, and one that had a
            // catalog gets the empty replacement.
            let visible = shared
                .tools
                .get(crate::NAME, &step.agent.scope())
                .is_some_and(|seen| {
                    std::ptr::addr_eq(Arc::as_ptr(&seen), Arc::as_ptr(&shared.tool))
                });
            let snapshot = if visible {
                let lookup = shared.lookup(step).await?;
                shared.skills.snapshot(&lookup).await?
            } else {
                SkillCatalogSnapshot {
                    skills: Vec::new(),
                    complete: true,
                }
            };
            if !snapshot.complete {
                return Ok(PreStepDecision::Enter { messages });
            }
            let entries: Vec<Entry> = snapshot
                .skills
                .iter()
                .filter(|skill| skill.is_model_invocable())
                .map(|skill| {
                    (
                        skill.name.clone(),
                        catalog_description(&skill.description, shared.description_max_length),
                    )
                })
                .collect();
            let session = shared.sessions.get(step.agent.id());
            let history = session.as_ref().map(catalog_history).unwrap_or_default();
            let existing = messages.iter().enumerate().find_map(|(index, message)| {
                catalog_entries_of(&message.source).map(|entries| (index, entries))
            });
            let drop_existing = |mut messages: Vec<UserMessage>| {
                if let Some((index, _)) = existing {
                    messages.remove(index);
                }
                Ok(PreStepDecision::Enter { messages })
            };
            if history.visible.as_ref() == Some(&entries) {
                return drop_existing(messages);
            }
            if existing.as_ref().is_some_and(|(_, held)| *held == entries) {
                return Ok(PreStepDecision::Enter { messages });
            }
            if !history.published && entries.is_empty() {
                return drop_existing(messages);
            }
            let id = format!("skill-catalog-{}", session.as_ref().map_or(0, Session::len));
            let catalog = if history.published {
                catalog_update(id, &entries)
            } else {
                catalog_message(id, &entries)
            };
            match existing {
                Some((index, _)) => messages[index] = catalog,
                None => messages.push(catalog),
            }
            Ok(PreStepDecision::Enter { messages })
        })
    }
}

/// Adds the skills a person invoked by `/name`. (dsh: the first
/// `agent/pre-step` listener.)
pub(crate) struct InvocationListener(pub(crate) Arc<Shared>);

impl PreStepListener for InvocationListener {
    fn decide<'a>(&'a self, step: &'a PreStep, next: PreStepNext<'a>) -> PreStepFuture<'a> {
        Box::pin(async move {
            let decision = next.run().await?;
            let PreStepDecision::Enter { mut messages } = decision else {
                return Ok(PreStepDecision::Reject);
            };
            let names = invoked_skill_names(&step.messages);
            if names.is_empty() {
                return Ok(PreStepDecision::Enter { messages });
            }
            let shared = &self.0;
            let lookup = shared.lookup(step).await?;
            let length = shared
                .sessions
                .get(step.agent.id())
                .map_or(0, |session| session.len());
            for name in names {
                // Unknown names and skills a person may not invoke stay
                // plain prose.
                let Some(skill) = shared.skills.get(&name, &lookup).await? else {
                    continue;
                };
                if !skill.is_user_invocable() {
                    continue;
                }
                messages.push(UserMessage::text(
                    format!("skill-invocation-{name}-{length}"),
                    skill_invocation_source(&name),
                    render_skill_content(
                        &skill.name,
                        &skill.provider,
                        skill.resource_base.as_ref(),
                        &skill.content,
                    ),
                ));
            }
            Ok(PreStepDecision::Enter { messages })
        })
    }
}

/// The catalog the session published: whether it ever did, and the entries
/// of the latest one still on its surface. (dsh: `catalogHistory`.)
#[derive(Debug, Default)]
struct History {
    visible: Option<Vec<Entry>>,
    published: bool,
}

fn catalog_history(session: &Session) -> History {
    let visible: HashSet<u64> = session.surface_nodes().iter().map(|seq| seq.0).collect();
    let mut history = History::default();
    for entry in session.events().into_iter().rev() {
        let SessionEvent::UserMessage { message, .. } = &entry.event else {
            continue;
        };
        let Some(entries) = catalog_entries_of(&message.source) else {
            continue;
        };
        history.published = true;
        if visible.contains(&entry.seq.0) {
            history.visible = Some(entries);
            return history;
        }
    }
    history
}

/// A catalog source's entries, or `None` when the message isn't a readable
/// catalog: a source of another kind, or entries missing or misshapen.
/// (dsh: `readCatalogEntries`.)
fn catalog_entries_of(source: &UserSource) -> Option<Vec<Entry>> {
    let UserSource::Other(source) = source else {
        return None;
    };
    if source.get("kind").and_then(Value::as_str) != Some(CATALOG_KIND) {
        return None;
    }
    source
        .get("entries")?
        .as_array()?
        .iter()
        .map(|entry| {
            let name = entry
                .get("name")?
                .as_str()
                .filter(|name| !name.is_empty())?;
            let description = entry.get("description")?.as_str()?;
            Some((name.to_owned(), description.to_owned()))
        })
        .collect()
}

fn source_entries(entries: &[Entry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|(name, description)| json!({ "name": name, "description": description }))
            .collect(),
    )
}

/// The catalog lines, escaped for the frame. (dsh: `renderCatalogEntries`.)
fn catalog_lines(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .map(|(name, description)| format!("- `{name}`: {}", escape_text(description)))
        .collect()
}

/// A session's first catalog. (dsh: `renderCatalogMessage`.)
pub fn catalog_message(id: String, entries: &[Entry]) -> UserMessage {
    let mut lines = vec![
        "<system-reminder>".to_owned(),
        "A skill is a reusable set of task-specific instructions. The following skills are \
         available in this session:"
            .to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];
    lines.extend(catalog_lines(entries));
    lines.extend([
        "</available_skills>".to_owned(),
        String::new(),
        "If the user names a skill, or the task clearly matches a skill's description, call the \
         `skill` tool with the exact skill name before taking task actions. Load all applicable \
         skills, then follow their full instructions. This catalog contains summaries only; do \
         not infer or follow a skill's instructions until it has been loaded."
            .to_owned(),
        "A user may also invoke a skill directly; its <skill_content> block then appears in this \
         conversation. Follow it, and do not call the `skill` tool again for that skill."
            .to_owned(),
        "</system-reminder>".to_owned(),
    ]);
    UserMessage::text(
        id,
        UserSource::Other(json!({
            "kind": CATALOG_KIND,
            "form": "catalog",
            "entries": source_entries(entries),
        })),
        lines.join("\n"),
    )
}

/// A replacement catalog, or the empty one that withdraws them all. (dsh:
/// `renderCatalogUpdate`.)
pub fn catalog_update(id: String, entries: &[Entry]) -> UserMessage {
    let availability = if entries.is_empty() {
        [
            "No skills are currently available through the `skill` tool. Do not use names from \
             earlier skill catalogs.",
            "A user may still invoke a skill directly; its <skill_content> block then appears in \
             this conversation. Follow it, and do not call the `skill` tool for it.",
        ]
    } else {
        [
            "Use only names in this replacement catalog. If the user names a listed skill, or the \
             task clearly matches its description, call the `skill` tool with the exact name \
             before acting.",
            "A user may also invoke a skill directly; its <skill_content> block then appears in \
             this conversation. Follow it, and do not call the `skill` tool again for that skill.",
        ]
    };
    let mut lines = vec![
        "<system-reminder>".to_owned(),
        "The available skill catalog changed. This complete catalog replaces every earlier \
         available-skills list in this session:"
            .to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];
    lines.extend(catalog_lines(entries));
    lines.push("</available_skills>".to_owned());
    lines.push(String::new());
    lines.extend(availability.map(str::to_owned));
    lines.push("</system-reminder>".to_owned());
    UserMessage::text(
        id,
        UserSource::Other(json!({
            "kind": CATALOG_KIND,
            "form": "catalog",
            "update": true,
            "entries": source_entries(entries),
        })),
        lines.join("\n"),
    )
}

/// A description on one line, cut to `max_length` with `...`. Whitespace
/// and lengths are JavaScript's: lengths in UTF-16 units. (dsh: `catalogDescription`.)
pub fn catalog_description(value: &str, max_length: usize) -> String {
    let normalized = value
        .split(js_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.encode_utf16().count() <= max_length {
        return normalized;
    }
    let mut cut = String::new();
    let mut units = 0;
    for character in normalized.chars() {
        units += character.len_utf16();
        if units > max_length - 3 {
            break;
        }
        cut.push(character);
    }
    format!("{cut}...")
}

/// The skills invoked by `/name` in the person's own messages, in the
/// order first seen, once each. Only text blocks of `user` messages count,
/// so no other source can forge one. (dsh: `invokedSkillNames`.)
pub fn invoked_skill_names(messages: &[UserMessage]) -> Vec<String> {
    let mut names = Vec::new();
    for message in messages {
        if !matches!(message.source, UserSource::User(_)) {
            continue;
        }
        for block in &message.content {
            let ContentBlock::Text(text) = block else {
                continue;
            };
            for name in gestures(&text.text) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// The `/name` tokens of `text`: a `/` at the start or after whitespace, a
/// skill name, then whitespace or the end. (dsh: `SKILL_GESTURE`.)
fn gestures(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut previous: Option<char> = None;
    for (at, character) in text.char_indices() {
        let starts = character == '/' && previous.is_none_or(js_space);
        previous = Some(character);
        if !starts {
            continue;
        }
        let rest = &text[at + 1..];
        let end = rest.find(js_space).unwrap_or(rest.len());
        let token = &rest[..end];
        if is_skill_name(token) {
            found.push(token.to_owned());
        }
    }
    found
}

/// Whether `character` is whitespace as JavaScript's `\s` sees it, which
/// differs from Unicode's `White_Space` in two characters: it counts U+FEFF
/// and not U+0085.
fn js_space(character: char) -> bool {
    match character {
        '\u{FEFF}' => true,
        '\u{0085}' => false,
        other => other.is_whitespace(),
    }
}

#[cfg(test)]
mod tests {
    use super::{catalog_description, gestures};

    #[test]
    fn a_description_is_one_line_and_cut_with_dots() {
        assert_eq!(catalog_description("  a \n\t b  ", 500), "a b");
        assert_eq!(catalog_description("abcdefgh", 8), "abcdefgh");
        assert_eq!(catalog_description("abcdefghi", 8), "abcde...");
        assert_eq!(catalog_description("aaaa😀b", 6), "aaa...");
    }

    #[test]
    fn a_gesture_is_a_whole_slash_token() {
        assert_eq!(gestures("/demo go"), ["demo"]);
        assert_eq!(gestures("please use /demo to answer"), ["demo"]);
        assert_eq!(gestures("ends with /demo"), ["demo"]);
        assert_eq!(gestures("/one\u{FEFF}/two /one"), ["one", "two", "one"]);
        for text in [
            "look under /demo/refs",
            "the odds are 5/8",
            "see foo/demo too",
            "/Demo",
            "/",
            "/demo\u{85}",
        ] {
            assert!(gestures(text).is_empty(), "{text}");
        }
    }
}

//! The model-visible surface: which surface events the model sees, in
//! order. (dsh v4: `foldSurface` and `SurfaceManager` in `surface.ts`.)
//!
//! The surface is a list of seqs, built by replaying each event's
//! [`SurfaceOp`] in log order:
//!
//! - `Append` adds the event's seq to the end.
//! - `Replace { start, end }` takes out the nodes from `start` to `end` and
//!   puts the event's seq in their place.
//!
//! Lifecycle and passthrough events never join it. Derivation then reads
//! the events at these seqs, so a compaction that replaced a range removes
//! that range from the history without deleting a single event.
//!
//! Each event is checked as it joins, so a log that was admitted always
//! folds. The checks ported from v4 are listed on [`SurfaceFold::admit`].

use serde_json::Value;

use crate::event::{Entry, Seq, SessionEvent, SurfaceOp};

/// The surface of a log, kept up to date one event at a time.
#[derive(Clone, Debug, Default)]
pub(crate) struct SurfaceFold {
    nodes: Vec<Seq>,
    /// How many replacements were admitted. (dsh v4: `contentGeneration`,
    /// which also counts plugin projections Karaka doesn't have.)
    generation: u64,
}

impl SurfaceFold {
    /// The seqs on the surface, in the order the model sees them.
    pub(crate) fn nodes(&self) -> &[Seq] {
        &self.nodes
    }

    /// How many replacements were admitted so far.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Checks `entry` against `log`, the events before it, and adds it to
    /// the surface. On an error the surface is unchanged.
    ///
    /// The checks, each from v4's `surface.ts`:
    ///
    /// - Cited sources are unique and earlier than the event
    ///   (`assertSourceEventReferences`).
    /// - A replacement's ends are earlier events that are on the surface,
    ///   in order, and its sources cite every node it takes out
    ///   (`replacementRange`).
    /// - A tool result replaces exactly one tool result and changes only
    ///   its content (`assertToolResultRewrite`).
    /// - While node 0 is a system prompt, only a system message over exactly
    ///   that node may replace it (`assertSystemHeadRewrite`).
    pub(crate) fn admit(&mut self, log: &[Entry], entry: &Entry) -> Result<(), String> {
        let Some((op, sources)) = intent(&entry.event) else {
            return Ok(());
        };
        let seq = entry.seq;
        check_sources(seq, sources)?;
        let SurfaceOp::Replace { start, end } = op else {
            self.nodes.push(seq);
            return Ok(());
        };

        if start >= seq || end >= seq {
            return Err(format!(
                "surface replace at seq {seq}: startSeq and endSeq must reference earlier events"
            ));
        }
        let position = |target: Seq, which: &str| {
            self.nodes
                .iter()
                .position(|node| *node == target)
                .ok_or_else(|| {
                    format!("surface replace: {which} seq {target} not found in surface")
                })
        };
        let start_index = position(start, "start")?;
        let end_index = position(end, "end")?;
        if start_index > end_index {
            return Err(format!(
                "surface replace: start seq {start} (index {start_index}) is after end seq \
                 {end} (index {end_index})"
            ));
        }
        let shadowed = &self.nodes[start_index..=end_index];
        let missing: Vec<String> = shadowed
            .iter()
            .filter(|node| !sources.contains(node))
            .map(Seq::to_string)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "surface replace: sourceEventSeqs must include every shadowed surface node; \
                 missing {}",
                missing.join(", ")
            ));
        }
        check_tool_result_rewrite(log, entry, shadowed)?;
        if start_index == 0
            && matches!(
                event_at(log, self.nodes[0]),
                Some(SessionEvent::SystemMessage { .. })
            )
            && !(matches!(entry.event, SessionEvent::SystemMessage { .. }) && shadowed.len() == 1)
        {
            return Err(
                "surface replace: node 0 holds the system prompt and may be rewritten \
                 only by a system/message over exactly that node"
                    .into(),
            );
        }

        self.nodes.splice(start_index..=end_index, [seq]);
        self.generation += 1;
        Ok(())
    }
}

/// How a surface event joins the surface, or `None` for any other event.
fn intent(event: &SessionEvent) -> Option<(SurfaceOp, &[Seq])> {
    match event {
        SessionEvent::SystemMessage { surface, .. }
        | SessionEvent::DeveloperMessage { surface, .. }
        | SessionEvent::UserMessage { surface, .. }
        | SessionEvent::ToolResult { surface, .. } => Some((surface.op, &surface.sources)),
        SessionEvent::AssistantMessage { op, .. } => Some((*op, &[])),
        _ => None,
    }
}

fn check_sources(seq: Seq, sources: &[Seq]) -> Result<(), String> {
    for (index, source) in sources.iter().enumerate() {
        if *source >= seq {
            return Err(format!(
                "sourceEventSeqs must reference earlier events: {source} >= current seq {seq}"
            ));
        }
        if sources[..index].contains(source) {
            return Err("sourceEventSeqs must not contain duplicates".into());
        }
    }
    Ok(())
}

fn check_tool_result_rewrite(log: &[Entry], entry: &Entry, shadowed: &[Seq]) -> Result<(), String> {
    let SessionEvent::ToolResult { data, .. } = &entry.event else {
        return Ok(());
    };
    let [original] = shadowed else {
        return Err("tool/result surface replacement must rewrite exactly one current node".into());
    };
    let Some(SessionEvent::ToolResult { data: original, .. }) = event_at(log, *original) else {
        return Err("tool/result surface replacement must target a current tool/result".into());
    };
    // Compare everything but the content, as JSON, the way v4 does.
    let without_content = |data: &crate::data::ToolResultData| {
        let mut value = serde_json::to_value(data).unwrap_or(Value::Null);
        if let Some(message) = value.get_mut("message").and_then(Value::as_object_mut) {
            message.insert("content".into(), Value::Null);
        }
        value
    };
    if without_content(original) != without_content(data) {
        return Err("tool/result surface replacement may change only content".into());
    }
    Ok(())
}

fn event_at(log: &[Entry], seq: Seq) -> Option<&SessionEvent> {
    usize::try_from(seq.0)
        .ok()
        .and_then(|index| log.get(index))
        .map(|entry| &entry.event)
}

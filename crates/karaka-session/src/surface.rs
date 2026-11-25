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

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::event::{Entry, Seq, SessionEvent, SurfaceOp};

/// The surface of a log, kept up to date one event at a time.
#[derive(Clone, Debug, Default)]
pub(crate) struct SurfaceFold {
    nodes: Vec<Seq>,
    /// How many replacements were admitted. (dsh v4: `contentGeneration`,
    /// which also counts plugin projections Karaka doesn't have.)
    generation: u64,
    /// The images each `image/offload` has marked, by the surface node that
    /// holds them, as indexes among the message's image blocks.
    offloaded: HashMap<Seq, BTreeSet<u64>>,
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
    ///
    /// And an `image/offload` must name images on the surface, as dsh's
    /// stored-log reader projects it (`imageOffloadProjection`).
    pub(crate) fn admit(&mut self, log: &[Entry], entry: &Entry) -> Result<(), String> {
        if let SessionEvent::Passthrough(passthrough) = &entry.event
            && passthrough.event_type == IMAGE_OFFLOAD
        {
            return self.offload(log, &passthrough.data);
        }
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

/// The one event type whose effect on messages dsh projects: images taken
/// out of the history. (dsh v4: `MESSAGE_PROJECTION_EVENT_TYPES`.)
const IMAGE_OFFLOAD: &str = "image/offload";

impl SurfaceFold {
    /// Checks one `image/offload` and records the images it marks, all or
    /// nothing. (dsh: `imageOffloadProjection.project` and
    /// `offloadMessageImages`, in its words.)
    ///
    /// Karaka sends no images to models, so the marks change no history;
    /// only what the log admits follows dsh.
    fn offload(&mut self, log: &[Entry], data: &Value) -> Result<(), String> {
        let targets = data
            .as_object()
            .filter(|data| data.len() == 1)
            .and_then(|data| data.get("targets"))
            .and_then(Value::as_array)
            .filter(|targets| !targets.is_empty())
            .ok_or("image/offload: data must contain a nonempty targets array")?;
        let mut marks: HashMap<Seq, BTreeSet<u64>> = HashMap::new();
        for target in targets {
            let shape = target.as_object().filter(|target| target.len() == 2);
            let seq = shape.and_then(|target| index(target.get("seq")));
            let indexes = shape
                .and_then(|target| target.get("imageIndexes"))
                .and_then(Value::as_array)
                .filter(|indexes| !indexes.is_empty());
            let (Some(seq), Some(indexes)) = (seq, indexes) else {
                return Err(
                    "image/offload: each target must contain a seq and nonempty imageIndexes"
                        .into(),
                );
            };
            let seq = Seq(seq);
            if marks.contains_key(&seq) {
                return Err(format!("image/offload: duplicate target seq {seq}"));
            }
            if !self.nodes.contains(&seq) {
                return Err(format!(
                    "image/offload: target seq {seq} is not a current surface node"
                ));
            }
            let message = match event_at(log, seq) {
                Some(SessionEvent::UserMessage { message, .. }) => serde_json::to_value(message),
                Some(SessionEvent::ToolResult { data, .. }) => serde_json::to_value(&data.message),
                _ => {
                    return Err(format!(
                        "image/offload: target seq {seq} must be user/message or tool/result"
                    ));
                }
            }
            .unwrap_or(Value::Null);
            let mut chosen = Vec::new();
            for value in indexes {
                match index(Some(value)) {
                    Some(image) if chosen.last().is_none_or(|last| image > *last) => {
                        chosen.push(image)
                    }
                    _ => {
                        return Err("image/offload: imageIndexes must be strictly increasing \
                                    non-negative safe integers"
                            .into());
                    }
                }
            }
            // The message's image blocks, in order, and whether each is
            // offloaded already, in the log or by an earlier offload.
            let earlier = self.offloaded.get(&seq);
            let images: Vec<bool> = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
                .enumerate()
                .map(|(image, block)| {
                    block.get("offloaded") == Some(&Value::Bool(true))
                        || earlier.is_some_and(|marked| marked.contains(&(image as u64)))
                })
                .collect();
            for image in &chosen {
                match usize::try_from(*image).ok().and_then(|at| images.get(at)) {
                    None => {
                        return Err(format!("image/offload: image index {image} does not exist"));
                    }
                    Some(true) => {
                        return Err(format!(
                            "image/offload: image index {image} is already offloaded"
                        ));
                    }
                    Some(false) => {}
                }
            }
            marks.insert(seq, chosen.into_iter().collect());
        }
        for (seq, images) in marks {
            self.offloaded.entry(seq).or_default().extend(images);
        }
        self.generation += 1;
        Ok(())
    }
}

/// A canonical index or seq: a non-negative safe integer.
fn index(value: Option<&Value>) -> Option<u64> {
    value
        .and_then(Value::as_u64)
        .filter(|index| *index < (1 << 53))
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

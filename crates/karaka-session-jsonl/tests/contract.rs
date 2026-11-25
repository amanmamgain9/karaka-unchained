//! The seam's contract, held against this backend in both encodings.
//! (dsh: `runPersistenceContract` in `session-persistence/tests/contract.ts`,
//! as `jsonl.spec.ts` runs it for `compression: 'none'` and `zstd.spec.ts`
//! for the default.)
//!
//! Left out: read arguments that are negative or fractional, and event data
//! that isn't JSON, which Rust's types rule out; and `await using`, which is
//! a handle dropped without closing here.

mod common;

/// Raw lines.
mod none {
    use super::common::*;
    use karaka_session::SessionId;
    use karaka_session_jsonl::{Compression, JsonlPersistence};
    use karaka_session_persistence::{
        CreateOptions, PersistenceError, SessionAccess, SessionPersistence,
    };
    use serde_json::json;

    fn backend(root: &TempRoot) -> JsonlPersistence {
        root.backend_in(Compression::None)
    }

    /// Half a record with no newline: a crash mid-write.
    fn corrupt_tail(root: &TempRoot, cwd: Option<&str>, name: &str) {
        append_bytes(
            &root.log_in(cwd, name, Compression::None),
            b"{\"type\":\"assistant/chunk\",\"seq\":8,\"ti",
        );
    }

    include!("contract/cases.rs");
}

/// Zstandard frames, the default.
mod zstd {
    use super::common::*;
    use karaka_session::SessionId;
    use karaka_session_jsonl::{Compression, JsonlPersistence};
    use karaka_session_persistence::{
        CreateOptions, PersistenceError, SessionAccess, SessionPersistence,
    };
    use serde_json::json;

    fn backend(root: &TempRoot) -> JsonlPersistence {
        root.backend_in(Compression::Zstd)
    }

    /// A torn last frame whose append never resolved, holding no complete
    /// record, as `zstd.spec.ts`'s `corruptTail` makes it.
    fn corrupt_tail(root: &TempRoot, cwd: Option<&str>, name: &str) {
        let line = format!(
            "{}\n",
            json!({ "type": "assistant/chunk", "seq": 8, "time": 9,
                    "data": { "turn": 2, "step": 1, "chunk": { "type": "text-delta", "index": 0, "text": noise(300_000) } } })
        );
        append_bytes(
            &root.log_in(cwd, name, Compression::Zstd),
            &torn_frame(&line, |decoded| !decoded.contains('\n')),
        );
    }

    include!("contract/cases.rs");
}

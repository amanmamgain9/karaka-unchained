//! Where a session's files live under the root: the project directory, the
//! session's own directory, and the generation filename. (dsh:
//! `session-persistence-jsonl/src/format.ts`, and `filename.ts` in
//! `session-format`.)
//!
//! ```text
//! <root>/
//!   --<normalized-cwd>--/     a readable project directory, or _no-cwd/
//!     <encoded-id>/           the session's own directory
//!       session.v4.jsonl      the current generation, raw lines
//!       session.lock          the write lock
//! ```
//!
//! A session id is an unchecked string, so it is escaped before it touches
//! a path: one segment, no traversal, and no two ids alike.

use std::path::{Path, PathBuf};

/// The lock file in a session's directory. (dsh: `LEASE_FILENAME`.)
pub const LOCK_FILENAME: &str = "session.lock";

/// The project directory of a session with no cwd.
const NO_CWD: &str = "_no-cwd";

/// What a compressed generation adds to its name. The compressed encoding
/// isn't ported yet; this backend writes raw lines, and refuses a root that
/// holds the other encoding, as dsh does.
const ZSTD_SUFFIX: &str = ".zstd";

/// Whether `unit` (one UTF-16 code unit) may appear as itself.
fn safe(unit: u16) -> bool {
    matches!(unit, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x2E | 0x5F | 0x2D)
}

fn escape(out: &mut String, unit: u16) {
    out.push_str(&format!("~{unit:04X}"));
}

/// Escapes `raw` into one safe path segment, injectively. (dsh:
/// `encodeSegment`.)
///
/// Letters, digits, `.`, `_` and `-` stay; every other UTF-16 code unit,
/// `~` included, becomes `~XXXX`. A whole `.` or `..` is escaped too, so
/// the segment can't climb out of its directory. Working on UTF-16 code
/// units gives the same segment dsh gives for the same id.
///
/// # Errors
///
/// When `raw` is empty.
pub fn encode_segment(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("cannot encode an empty path segment".into());
    }
    match raw {
        "." => return Ok("~002E".into()),
        ".." => return Ok("~002E~002E".into()),
        _ => {}
    }
    let mut out = String::new();
    for unit in raw.encode_utf16() {
        if safe(unit) {
            out.push(char::from(unit as u8));
        } else {
            escape(&mut out, unit);
        }
    }
    Ok(out)
}

/// The readable directory name for a project path. (dsh: `projectKey`.)
///
/// Separators (`/`, `\`, `:`) become one `-`, and other unsafe code units
/// use the same `~XXXX` escape. Unlike [`encode_segment`] this is lossy on
/// purpose, so the name stays readable: paths that normalize alike share a
/// project directory, and their sessions are still told apart by id. The
/// name is cut to fit a filesystem's limit of 255 bytes.
///
/// # Errors
///
/// When `cwd` is empty.
pub fn project_key(cwd: &str) -> Result<String, String> {
    if cwd.is_empty() {
        return Err("cannot encode an empty project path".into());
    }
    let mut readable = String::new();
    let mut separator_run = false;
    for unit in cwd.encode_utf16() {
        if matches!(unit, 0x2F | 0x5C | 0x3A) {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else {
            if safe(unit) {
                readable.push(char::from(unit as u8));
            } else {
                escape(&mut readable, unit);
            }
            separator_run = false;
        }
    }
    let slug = readable.trim_start_matches('-');
    let slug = if slug.is_empty() { "root" } else { slug };
    // Every character is ASCII, so cutting at a byte is cutting at a char.
    Ok(format!("--{}--", &slug[..slug.len().min(251)]))
}

/// The project directory under `root` for `cwd`. (dsh: `projectDir`.)
pub fn project_dir(root: &Path, cwd: Option<&str>) -> Result<PathBuf, String> {
    match cwd {
        None => Ok(root.join(NO_CWD)),
        Some(cwd) => Ok(root.join(project_key(cwd)?)),
    }
}

/// The directory a session owns. (dsh: `sessionDir`.)
pub fn session_dir(root: &Path, cwd: Option<&str>, id: &str) -> Result<PathBuf, String> {
    Ok(project_dir(root, cwd)?.join(encode_segment(id)?))
}

/// The raw log's name for one format generation: `session.jsonl` for v0,
/// `session.vN.jsonl` after. (dsh: `sessionFormatLogFilename`.)
pub fn generation_filename(version: u64) -> String {
    if version == 0 {
        "session.jsonl".into()
    } else {
        format!("session.v{version}.jsonl")
    }
}

/// The path of a session's log in one generation. (dsh:
/// `generationLogPath`.)
pub fn generation_path(
    root: &Path,
    cwd: Option<&str>,
    id: &str,
    version: u64,
) -> Result<PathBuf, String> {
    Ok(session_dir(root, cwd, id)?.join(generation_filename(version)))
}

/// Which physical encoding a generation file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Raw lines, which this backend reads and writes.
    Raw,
    /// Zstandard frames, which it doesn't yet.
    Zstd,
}

/// The format version a canonical generation filename names, and its
/// encoding; `None` for any other name, such as a temporary file, `.v0`,
/// a leading zero or capitals. (dsh: `parseGenerationLogFilename`.)
pub(crate) fn parse_generation_filename(name: &str) -> Option<(u64, Encoding)> {
    let (raw, encoding) = match name.strip_suffix(ZSTD_SUFFIX) {
        Some(raw) => (raw, Encoding::Zstd),
        None => (name, Encoding::Raw),
    };
    if raw == "session.jsonl" {
        return Some((0, encoding));
    }
    let digits = raw.strip_prefix("session.v")?.strip_suffix(".jsonl")?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && !digits.starts_with('0');
    let version = digits.parse::<u64>().ok()?;
    (canonical && version <= karaka_session_persistence::MAX_SAFE_INTEGER)
        .then_some((version, encoding))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_neutralize_traversal_separators_and_absolute_paths() {
        assert_eq!(encode_segment("..").unwrap(), "~002E~002E");
        assert_eq!(encode_segment(".").unwrap(), "~002E");
        assert_eq!(encode_segment("a/b").unwrap(), "a~002Fb");
        assert_eq!(
            encode_segment("/etc/passwd").unwrap(),
            "~002Fetc~002Fpasswd"
        );
        assert_eq!(encode_segment("a\u{0}b").unwrap(), "a~0000b");
        assert_eq!(encode_segment("plain-ID_1.2").unwrap(), "plain-ID_1.2");
        assert_eq!(encode_segment("a~b").unwrap(), "a~007Eb");
    }

    #[test]
    fn segments_are_injective() {
        // A literal "~002F" can't collide with the escape of "/".
        assert_ne!(
            encode_segment("~002F").unwrap(),
            encode_segment("/").unwrap()
        );
        // Characters outside the BMP escape as their two UTF-16 units, as
        // in dsh.
        assert_eq!(encode_segment("😀").unwrap(), "~D83D~DE00");
        assert!(encode_segment("").unwrap_err().contains("empty"));
    }

    #[test]
    fn project_keys_are_readable_and_bounded() {
        assert_eq!(
            project_key("/Users/someone/work/harness").unwrap(),
            "--Users-someone-work-harness--"
        );
        assert_eq!(
            project_key("/a/b-c").unwrap(),
            project_key("/a-b/c").unwrap()
        );
        assert_eq!(project_key("C:\\work\\agent").unwrap(), "--C-work-agent--");
        assert_eq!(
            project_key("/开发/~agent").unwrap(),
            "--~5F00~53D1-~007Eagent--"
        );
        assert_eq!(project_key("/").unwrap(), "--root--");
        assert_eq!(
            project_key(&format!("/{}", "x".repeat(1000)))
                .unwrap()
                .len(),
            255
        );
        assert!(project_key("").unwrap_err().contains("empty project path"));
    }

    #[test]
    fn only_canonical_generation_names_parse() {
        assert_eq!(generation_filename(0), "session.jsonl");
        assert_eq!(generation_filename(4), "session.v4.jsonl");
        assert_eq!(
            parse_generation_filename("session.jsonl"),
            Some((0, Encoding::Raw))
        );
        assert_eq!(
            parse_generation_filename("session.v1.jsonl"),
            Some((1, Encoding::Raw))
        );
        assert_eq!(
            parse_generation_filename("session.v27.jsonl.zstd"),
            Some((27, Encoding::Zstd))
        );
        assert_eq!(
            parse_generation_filename("session.v9007199254740992.jsonl"),
            None
        );
        for name in [
            "session.v0.jsonl",
            "session.v01.jsonl",
            "session.V1.jsonl",
            "session.v1.backup.jsonl",
            "session.migration.deadbeef.tmp.jsonl",
            "session.lock",
        ] {
            assert_eq!(parse_generation_filename(name), None, "{name}");
        }
    }
}

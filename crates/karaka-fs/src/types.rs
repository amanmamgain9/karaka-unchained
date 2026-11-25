//! The seam's vocabulary: targets, versions, metadata, and the shapes of
//! writes and edits. (dsh: `fs/src/types.ts`.)

use std::fmt;

/// A target's stable identity, for stale guards and lookups. A backend
/// makes it; a consumer never parses it or takes it for a local path. The
/// local backend's is a realpath. (dsh: `FsTargetKey`.)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FsTargetKey(String);

impl FsTargetKey {
    /// For backends: a consumer gets keys from `resolve`. No check is made.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FsTargetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A file's freshness token, which guarded writes and edits check. Opaque:
/// a consumer records and compares it, never reads it. (dsh: `FsVersion`.)
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FsVersion(String);

impl FsVersion {
    /// For backends: a consumer gets versions from `stat` and from write and
    /// edit outcomes. No check is made.
    pub fn new(version: impl Into<String>) -> Self {
        Self(version.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One authoritative observation of a target. Present carries the version a
/// guarded replacement checks; absent allows only a guarded create, never
/// an edit. (dsh: `FsObservation`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsObservation {
    Present(FsVersion),
    Absent,
}

/// A path a backend resolved to a stable identity: what `resolve` returns,
/// and what every other operation takes. (dsh: `FsTarget`.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FsTarget {
    pub target_key: FsTargetKey,
    /// The path to show the model or a person. A local absolute path, a
    /// workspace-relative one, or a URI, as the backend chooses.
    pub display_path: String,
}

/// What a target is. (dsh: `FsInfo.type`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsType {
    File,
    Directory,
    Other,
}

impl FsType {
    /// dsh's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Other => "other",
        }
    }
}

/// What a path entry is, a symbolic link not followed. (dsh:
/// `FsPathInfo.type`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsPathType {
    File,
    Directory,
    Symlink,
    Other,
}

impl FsPathType {
    /// dsh's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

/// A target's metadata, never its content: what `stat` returns for a target
/// that exists. (dsh: `FsInfo`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsInfo {
    pub version: FsVersion,
    pub kind: FsType,
    /// A regular file's size in bytes, when the backend can tell.
    pub size: Option<u64>,
}

/// A path's metadata, its last component not followed if it is a symbolic
/// link, so a consumer can refuse a link before resolving it. (dsh:
/// `FsPathInfo`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsPathInfo {
    pub version: FsVersion,
    pub kind: FsPathType,
    pub size: Option<u64>,
}

/// One direct child of a listed directory: metadata and a resolved target,
/// never content. (dsh: `FsDirEntry`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsDirEntry {
    /// The child's name in the directory.
    pub name: String,
    pub kind: FsType,
    pub target: FsTarget,
    /// When the backend can read it cheaply.
    pub version: Option<FsVersion>,
    /// A regular file's size, when the backend can tell.
    pub size: Option<u64>,
}

/// A byte window: `length` bytes from `offset`. (dsh: `readByteRange`'s
/// `range`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

/// A guard on a write. No guard at all, `None`, is an unconditional create
/// or overwrite. (dsh: `FsWriteIntent`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsWriteIntent {
    /// Refuse a target that exists, with `FS_NOT_OBSERVED`.
    CreateIfAbsent,
    /// Refuse a target that is gone or at another version, with
    /// `FS_STALE_VERSION`.
    ReplaceIfVersion(FsVersion),
}

/// Whether a write made the file or replaced it. (dsh:
/// `FsWriteOutcome.operation`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsWriteOperation {
    Create,
    Update,
}

/// What a whole-file write did. (dsh: `FsWriteOutcome`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsWriteOutcome {
    pub operation: FsWriteOperation,
    /// The file's version after the write.
    pub version: FsVersion,
    /// The content before, with CRLF made LF, as a diff's basis; `None` for
    /// a create, or when the backend declined one, such as for a binary or
    /// oversized file.
    pub before: Option<String>,
    /// The content after, with CRLF made LF, to share `before`'s basis.
    pub after: String,
}

/// A literal search and replace. (dsh: `FsEditRequest`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEditRequest {
    /// The text to replace, matched exactly once line endings are
    /// normalized. Must not be empty.
    pub old_string: String,
    /// The replacement; empty deletes the match.
    pub new_string: String,
    /// Replace every match rather than requiring exactly one.
    pub replace_all: bool,
}

/// What an edit did. (dsh: `FsEditOutcome`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEditOutcome {
    /// The file's version after the edit.
    pub version: FsVersion,
    /// The content before, with CRLF made LF.
    pub before: String,
    /// The content after, with CRLF made LF.
    pub after: String,
}

//! The local backend's mechanics, apart from the port: resolution, probes,
//! listings, UTF-8 text reads, raw byte reads, the atomic write, and literal
//! edits. (dsh: `fs-local/src/fsio.ts`.)
//!
//! A write is staged in an owner-only file, in a private directory beside
//! the target, synced, and published with a rename, or with a hard link
//! when it must not replace anything. The staging is removed whatever
//! happens, including when the write's future is dropped.
//!
//! Where dsh lets a raw Node error through, such as a failed rename, here
//! it is an [`FsError`] of `FS_IO_ERROR`, or `FS_PERMISSION_DENIED`, with
//! the `io::Error` as its source.

use std::fs::{Metadata, Permissions};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::stream;
use karaka_fs::{
    AbortSignal, ByteRange, FsError, FsErrorCode, FsPathType, FsTargetKey, FsType, FsVersion,
    TextStream,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::paths::{
    basename, dirname, has_parent_segment, is_absolute, normalize_absolute, process_cwd, resolve,
};

/// How much of a file is searched for a NUL byte, the sign of a binary
/// file. (dsh: `BINARY_SAMPLE_BYTES`.)
const BINARY_SAMPLE_BYTES: usize = 8192;

/// One read's size: Node's stream chunk, and dsh's diff-basis read.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// A resolved local path. (dsh: `LocalTarget`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalTarget {
    /// The absolute path, symbolic links unresolved, shown to callers.
    pub display_path: String,
    /// The realpath: the stable key, and the path I/O goes through.
    pub target_key: FsTargetKey,
}

/// A path's metadata, symbolic links followed. (dsh: `PathInfo`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathInfo {
    pub version: FsVersion,
    /// The permission bits, `mode & 0o777`.
    pub mode: u32,
    pub kind: FsType,
    pub size: u64,
}

/// A path's metadata, its last component not followed. (dsh:
/// `PathLinkInfo`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLinkInfo {
    pub version: FsVersion,
    pub mode: u32,
    pub kind: FsPathType,
    pub size: u64,
}

/// One listed child. (dsh: `LocalDirEntry`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDirEntry {
    pub name: String,
    pub kind: FsType,
    pub target: LocalTarget,
    pub version: Option<FsVersion>,
    pub size: Option<u64>,
}

/// A staged write's paths, as [`Internals::inspect_temp`] sees them.
#[derive(Clone, Debug)]
pub struct StagedPaths {
    pub staging_dir: PathBuf,
    pub temp_path: PathBuf,
}

/// A naming hook: the target path in, a name out.
pub type NameHook = Arc<dyn Fn(&str) -> String + Send + Sync>;
/// A step on paths that may fail, as an `io::Error`.
pub type PathHook<T> = Arc<dyn Fn(&Path) -> io::Result<T> + Send + Sync>;
/// A publication: the temp file, then the target.
pub type LinkHook = Arc<dyn Fn(&Path, &Path) -> io::Result<()> + Send + Sync>;
/// A look between two steps.
pub type InspectHook<T> = Arc<dyn Fn(&T) + Send + Sync>;

/// Test hooks into a write's and a read's steps, to pin names, fail a step,
/// or act between two. Each is `None` by default. (dsh: `FsIoInternals`,
/// without its Windows hooks.)
#[derive(Clone, Default)]
pub struct Internals {
    /// The staging directory's name, beside the target.
    pub temp_dir_name: Option<NameHook>,
    /// The temp file's name, in the staging directory.
    pub temp_name: Option<NameHook>,
    /// Publishes a guarded create, in place of `hard_link(temp, target)`.
    pub link_file: Option<LinkHook>,
    /// Reads the target after a guarded publication failed, in place of
    /// `symlink_metadata`.
    pub inspect_publication_target: Option<PathHook<Metadata>>,
    /// Removes the staging directory, in place of `remove_dir_all`.
    pub remove_staging_dir: Option<PathHook<()>>,
    /// Runs once the temp file is written and synced, before it gets its
    /// final mode and is published.
    pub inspect_temp: Option<InspectHook<StagedPaths>>,
    /// Runs after a raw read's size check, before its content is read.
    pub inspect_read_bytes_after_stat: Option<InspectHook<LocalTarget>>,
}

impl std::fmt::Debug for Internals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Internals").finish_non_exhaustive()
    }
}

pub(crate) fn check(signal: Option<&AbortSignal>, verb: &str) -> Result<(), FsError> {
    match signal {
        Some(signal) if signal.is_aborted() => Err(FsError::aborted(verb)),
        _ => Ok(()),
    }
}

/// An error dsh lets through raw, as an [`FsError`].
pub(crate) fn raw(verb: &str, display_path: &str, error: io::Error) -> FsError {
    let code = if error.kind() == io::ErrorKind::PermissionDenied {
        FsErrorCode::PermissionDenied
    } else {
        FsErrorCode::IoError
    };
    FsError::with_source(
        code,
        format!("cannot {verb} \"{display_path}\": {error}"),
        error,
    )
}

/// Absent: no such entry, or a parent segment that is a file. (dsh:
/// `ENOENT` and `ENOTDIR`.)
fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// The version token: device, inode, size, and the modification and change
/// times in nanoseconds. (dsh: `versionOf`, from `stat`'s bigint fields.)
fn version_of(metadata: &Metadata) -> FsVersion {
    let nanos = |seconds: i64, nanos: i64| i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    FsVersion::new(format!(
        "{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        nanos(metadata.mtime(), metadata.mtime_nsec()),
        nanos(metadata.ctime(), metadata.ctime_nsec())
    ))
}

fn kind_of(metadata: &Metadata) -> FsType {
    if metadata.is_file() {
        FsType::File
    } else if metadata.is_dir() {
        FsType::Directory
    } else {
        FsType::Other
    }
}

/// The absolute path shown for `path` from `cwd`. A relative `cwd` is taken
/// from the process's directory. A path with a `..` segment is kept as
/// spelled, so the kernel resolves it physically, through symbolic links;
/// any other is normalized. (dsh: `localDisplayPath`.)
pub fn local_display_path(cwd: &str, path: &str) -> String {
    let absolute_cwd = if is_absolute(cwd) {
        cwd.to_owned()
    } else {
        format!("{}/{cwd}", process_cwd())
    };
    let spelled = if is_absolute(path) {
        path.to_owned()
    } else {
        format!("{absolute_cwd}/{path}")
    };
    if has_parent_segment(&spelled) {
        spelled
    } else {
        resolve(cwd, path)
    }
}

/// A resolution's failure: one of the seam's, or a raw one, which a
/// listing words its own way.
pub(crate) enum Failure {
    Fs(FsError),
    Io(io::Error),
}

impl From<FsError> for Failure {
    fn from(error: FsError) -> Self {
        Self::Fs(error)
    }
}

fn utf8(path: PathBuf) -> Result<String, Failure> {
    path.into_os_string().into_string().map_err(|_| {
        Failure::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "the path is not valid UTF-8",
        ))
    })
}

/// Resolves `path` from `cwd` to its display path and realpath. A missing
/// target keys on its nearest existing ancestor's realpath, with the
/// missing part appended, so the key is the same before and after it is
/// created. (dsh: `resolveLocalTarget`.)
pub async fn resolve_local_target(cwd: &str, path: &str) -> Result<LocalTarget, FsError> {
    resolve_target(cwd, path)
        .await
        .map_err(|failure| match failure {
            Failure::Fs(error) => error,
            Failure::Io(error) => raw("resolve", &local_display_path(cwd, path), error),
        })
}

pub(crate) async fn resolve_target(cwd: &str, path: &str) -> Result<LocalTarget, Failure> {
    if path.trim().is_empty() {
        return Err(FsError::new(
            FsErrorCode::NotFound,
            "file_path must be a non-empty string",
        )
        .into());
    }
    let display_path = local_display_path(cwd, path);
    match tokio::fs::canonicalize(&display_path).await {
        Ok(real) => {
            return Ok(LocalTarget {
                target_key: FsTargetKey::new(utf8(real)?),
                display_path,
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
            return Err(FsError::new(
                FsErrorCode::NotFound,
                format!(
                    "cannot resolve \"{display_path}\": a parent path segment is not a directory"
                ),
            )
            .into());
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(Failure::Io(error)),
        Err(_) => {}
    }
    let mut missing = vec![basename(&display_path).to_owned()];
    let mut ancestor = dirname(&display_path);
    loop {
        match tokio::fs::canonicalize(&ancestor).await {
            Ok(real) => {
                if missing.iter().any(|segment| segment == "..") {
                    return Err(FsError::new(
                        FsErrorCode::NotFound,
                        format!(
                            "cannot resolve \"{display_path}\": parent traversal crosses a \
                             missing directory"
                        ),
                    )
                    .into());
                }
                let real = utf8(real)?;
                let key = normalize_absolute(&format!("{real}/{}", missing.join("/")));
                return Ok(LocalTarget {
                    target_key: FsTargetKey::new(key),
                    display_path,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = dirname(&ancestor);
                if parent == ancestor {
                    return Ok(LocalTarget {
                        target_key: FsTargetKey::new(display_path.clone()),
                        display_path,
                    });
                }
                missing.insert(0, basename(&ancestor).to_owned());
                ancestor = parent;
            }
            Err(error) => return Err(Failure::Io(error)),
        }
    }
}

/// `path`'s metadata, symbolic links followed, or `None` when it, or a
/// parent segment, doesn't exist. (dsh: `probe`.)
pub async fn probe(path: &str) -> io::Result<Option<PathInfo>> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(Some(PathInfo {
            version: version_of(&metadata),
            mode: metadata.mode() & 0o777,
            kind: kind_of(&metadata),
            size: metadata.size(),
        })),
        Err(error) if is_absent(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// `path`'s metadata, its last component not followed. (dsh:
/// `probeNoFollow`.)
pub async fn probe_no_follow(path: &str) -> io::Result<Option<PathLinkInfo>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(Some(PathLinkInfo {
            version: version_of(&metadata),
            mode: metadata.mode() & 0o777,
            kind: if metadata.file_type().is_symlink() {
                FsPathType::Symlink
            } else {
                match kind_of(&metadata) {
                    FsType::File => FsPathType::File,
                    FsType::Directory => FsPathType::Directory,
                    FsType::Other => FsPathType::Other,
                }
            },
            size: metadata.size(),
        })),
        Err(error) if is_absent(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// A listing's failure, worded as dsh's. (dsh: `listingIoError`.)
fn listing_error(display_path: &str, failure: Failure) -> FsError {
    let error = match failure {
        Failure::Fs(error) => return error,
        Failure::Io(error) => error,
    };
    if is_absent(&error) {
        FsError::with_source(
            FsErrorCode::NotFound,
            format!("cannot list \"{display_path}\": not found"),
            error,
        )
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        FsError::with_source(
            FsErrorCode::PermissionDenied,
            format!("cannot list \"{display_path}\": permission denied"),
            error,
        )
    } else {
        FsError::with_source(
            FsErrorCode::IoError,
            format!("cannot list \"{display_path}\": {error}"),
            error,
        )
    }
}

/// A directory's direct children, in name order, each with a target
/// resolved from the listed directory's own realpath, and its metadata when
/// it still exists. Never reads content. (dsh: `listDirectory`.)
pub async fn list_directory(
    target: &LocalTarget,
    signal: Option<&AbortSignal>,
) -> Result<Vec<LocalDirEntry>, FsError> {
    check(signal, "list")?;
    let display = &target.display_path;
    let key = target.target_key.as_str();
    let info = probe(key)
        .await
        .map_err(|error| listing_error(display, Failure::Io(error)))?;
    match info {
        None => {
            return Err(FsError::new(
                FsErrorCode::NotFound,
                format!("cannot list \"{display}\": not found"),
            ));
        }
        Some(info) if info.kind != FsType::Directory => {
            return Err(FsError::new(
                FsErrorCode::NotDirectory,
                format!("cannot list \"{display}\": not a directory"),
            ));
        }
        Some(_) => {}
    }
    let mut names = Vec::new();
    let mut entries = tokio::fs::read_dir(key)
        .await
        .map_err(|error| listing_error(display, Failure::Io(error)))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| listing_error(display, Failure::Io(error)))?
    {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    check(signal, "list")?;
    names.sort();

    let mut listed = Vec::with_capacity(names.len());
    for name in names {
        check(signal, "list")?;
        let child = async {
            let identity = resolve_target(key, &name).await?;
            let info = probe(identity.target_key.as_str())
                .await
                .map_err(Failure::Io)?;
            Ok::<_, Failure>(LocalDirEntry {
                kind: info.as_ref().map_or(FsType::Other, |info| info.kind),
                version: info.as_ref().map(|info| info.version.clone()),
                size: info
                    .as_ref()
                    .filter(|info| info.kind == FsType::File)
                    .map(|info| info.size),
                target: LocalTarget {
                    display_path: local_display_path(display, &name),
                    target_key: identity.target_key,
                },
                name: name.clone(),
            })
        };
        let entry = child
            .await
            .map_err(|failure| listing_error(&local_display_path(display, &name), failure))?;
        listed.push(entry);
        check(signal, "list")?;
    }
    Ok(listed)
}

/// UTF-8 text, a leading byte-order mark dropped, as Node's fatal
/// `TextDecoder` does.
fn decode(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    Some(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_owned(),
        None => text,
    })
}

fn not_text(verb: &str, display_path: &str) -> FsError {
    FsError::new(
        FsErrorCode::NotText,
        format!("cannot {verb} \"{display_path}\": invalid UTF-8 text"),
    )
}

fn binary(verb: &str, display_path: &str) -> FsError {
    FsError::new(
        FsErrorCode::NotText,
        format!("cannot {verb} \"{display_path}\": binary file"),
    )
}

/// The target's metadata, refusing a missing or non-regular file. (dsh:
/// `statRegularFile`.)
async fn stat_regular_file(
    target: &LocalTarget,
    signal: Option<&AbortSignal>,
) -> Result<Metadata, FsError> {
    check(signal, "read")?;
    let display = &target.display_path;
    let metadata = match tokio::fs::metadata(target.target_key.as_str()).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(FsError::new(
                FsErrorCode::NotFound,
                format!("cannot read \"{display}\": not found"),
            ));
        }
        Err(error) => return Err(raw("read", display, error)),
    };
    if !metadata.is_file() {
        return Err(FsError::new(
            FsErrorCode::NotRegularFile,
            format!("cannot read \"{display}\": not a regular file"),
        ));
    }
    Ok(metadata)
}

/// The whole regular file as UTF-8 text, refusing a NUL in its first 8 KiB
/// and invalid UTF-8. (dsh: `readWholeText`.)
pub async fn read_whole_text(
    target: &LocalTarget,
    signal: Option<&AbortSignal>,
) -> Result<String, FsError> {
    stat_regular_file(target, signal).await?;
    let display = &target.display_path;
    let bytes = tokio::fs::read(target.target_key.as_str())
        .await
        .map_err(|error| raw("read", display, error))?;
    check(signal, "read")?;
    if bytes[..bytes.len().min(BINARY_SAMPLE_BYTES)].contains(&0) {
        return Err(binary("read", display));
    }
    decode(bytes).ok_or_else(|| not_text("read", display))
}

/// The whole regular file's bytes, at most `max_bytes`: the stat refuses a
/// larger file before any content is read, and the read takes one byte past
/// the cap, so a file grown since is refused too. (dsh: `readWholeBytes`.)
pub async fn read_whole_bytes(
    target: &LocalTarget,
    signal: Option<&AbortSignal>,
    max_bytes: u64,
    internals: &Internals,
) -> Result<Vec<u8>, FsError> {
    let metadata = stat_regular_file(target, signal).await?;
    let display = &target.display_path;
    if metadata.len() > max_bytes {
        return Err(FsError::new(
            FsErrorCode::TooLarge,
            format!(
                "cannot read \"{display}\": {} bytes exceeds the {max_bytes}-byte limit",
                metadata.len()
            ),
        ));
    }
    if let Some(hook) = &internals.inspect_read_bytes_after_stat {
        hook(target);
    }
    let file = tokio::fs::File::open(target.target_key.as_str())
        .await
        .map_err(|error| raw("read", display, error))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| raw("read", display, error))?;
    check(signal, "read")?;
    if bytes.len() as u64 > max_bytes {
        return Err(FsError::new(
            FsErrorCode::TooLarge,
            format!("cannot read \"{display}\": content exceeds the {max_bytes}-byte limit"),
        ));
    }
    Ok(bytes)
}

/// The bytes of `range`, never more: shorter where the file ends inside
/// it, empty at or past the end. (dsh: `readByteWindow`.)
pub async fn read_byte_window(
    target: &LocalTarget,
    range: ByteRange,
    signal: Option<&AbortSignal>,
) -> Result<Vec<u8>, FsError> {
    stat_regular_file(target, signal).await?;
    if range.length == 0 {
        return Ok(Vec::new());
    }
    let display = &target.display_path;
    let mut file = tokio::fs::File::open(target.target_key.as_str())
        .await
        .map_err(|error| raw("read", display, error))?;
    file.seek(io::SeekFrom::Start(range.offset))
        .await
        .map_err(|error| raw("read", display, error))?;
    let mut bytes = Vec::new();
    file.take(range.length)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| raw("read", display, error))?;
    check(signal, "read")?;
    Ok(bytes)
}

/// Decodes UTF-8 across chunk boundaries, fatally, as Node's
/// `TextDecoder('utf-8', { fatal: true })` with `stream: true`.
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
    started: bool,
}

impl Utf8Decoder {
    fn feed(&mut self, chunk: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            // A sequence cut off by the chunk's end waits for the next one.
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return None,
        };
        let rest = self.pending.split_off(valid);
        let text = String::from_utf8(std::mem::replace(&mut self.pending, rest)).ok()?;
        Some(self.without_bom(text))
    }

    fn finish(&mut self) -> Option<String> {
        self.pending.is_empty().then(String::new)
    }

    fn without_bom(&mut self, text: String) -> String {
        if self.started || text.is_empty() {
            return text;
        }
        self.started = true;
        match text.strip_prefix('\u{feff}') {
            Some(rest) => rest.to_owned(),
            None => text,
        }
    }
}

enum Streaming {
    Start(LocalTarget, Option<AbortSignal>),
    Reading(Reading),
    Done,
}

struct Reading {
    file: tokio::fs::File,
    decoder: Utf8Decoder,
    sampled: usize,
    display_path: String,
    signal: Option<AbortSignal>,
}

/// The whole regular file as decoded text chunks, with
/// [`read_whole_text`]'s checks, never all of it in memory. Its refusals
/// come as the stream's first item, as dsh's generator throws on its first
/// step. (dsh: `streamWholeText`.)
pub fn stream_whole_text(target: LocalTarget, signal: Option<AbortSignal>) -> TextStream {
    Box::pin(stream::unfold(
        Streaming::Start(target, signal),
        |state| async move {
            let reading = match state {
                Streaming::Done => return None,
                Streaming::Reading(reading) => reading,
                Streaming::Start(target, signal) => {
                    if let Err(error) = stat_regular_file(&target, signal.as_ref()).await {
                        return Some((Err(error), Streaming::Done));
                    }
                    let display_path = target.display_path;
                    match tokio::fs::File::open(target.target_key.as_str()).await {
                        Ok(file) => Reading {
                            file,
                            decoder: Utf8Decoder::default(),
                            sampled: 0,
                            display_path,
                            signal,
                        },
                        Err(error) => {
                            return Some((Err(raw("read", &display_path, error)), Streaming::Done));
                        }
                    }
                }
            };
            next_chunk(reading).await
        },
    ))
}

async fn next_chunk(mut reading: Reading) -> Option<(Result<String, FsError>, Streaming)> {
    if let Err(error) = check(reading.signal.as_ref(), "read") {
        return Some((Err(error), Streaming::Done));
    }
    let mut buffer = vec![0; READ_CHUNK_BYTES];
    let read = match reading.file.read(&mut buffer).await {
        Ok(read) => read,
        Err(error) => {
            return Some((
                Err(raw("read", &reading.display_path, error)),
                Streaming::Done,
            ));
        }
    };
    if read == 0 {
        let rest = reading
            .decoder
            .finish()
            .ok_or_else(|| not_text("read", &reading.display_path));
        return Some((rest, Streaming::Done));
    }
    let chunk = &buffer[..read];
    if reading.sampled < BINARY_SAMPLE_BYTES {
        let sample = &chunk[..chunk.len().min(BINARY_SAMPLE_BYTES - reading.sampled)];
        if sample.contains(&0) {
            return Some((Err(binary("read", &reading.display_path)), Streaming::Done));
        }
        reading.sampled += sample.len();
    }
    match reading.decoder.feed(chunk) {
        Some(text) => Some((Ok(text), Streaming::Reading(reading))),
        None => Some((
            Err(not_text("read", &reading.display_path)),
            Streaming::Done,
        )),
    }
}

/// A unique name part for a staging directory. (dsh: `randomUUID()`.)
fn unique() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    let first = hasher.finish();
    hasher.write_u64(first);
    format!("{first:016x}{:016x}", hasher.finish())
}

/// Removes a staging directory the write never got to clean up, such as
/// when its future is dropped.
struct Staging {
    dir: PathBuf,
    armed: bool,
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Atomically writes `content` to `path`, through an owner-only temp file
/// (0o600) in a private staging directory (0o700) beside it, synced before
/// it is published. Missing parent directories are made. `mode` is the
/// existing file's, to keep; a new file stays owner-only. The signal is
/// checked before publication. (dsh: `writeFileAtomic`.)
///
/// With `create_if_absent`, the display path to name in errors, the temp
/// file is published with a hard link, which never replaces anything: a
/// file a competitor made meanwhile is kept, and the write refused with
/// `FS_NOT_OBSERVED`.
pub async fn write_file_atomic(
    path: &str,
    content: &str,
    mode: Option<u32>,
    signal: Option<&AbortSignal>,
    internals: &Internals,
    create_if_absent: Option<&str>,
) -> Result<(), FsError> {
    check(signal, "write")?;
    let directory = dirname(path);
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| raw("write", path, error))?;
    check(signal, "write")?;

    let base = basename(path).to_owned();
    let staging_name = match &internals.temp_dir_name {
        Some(name) => name(path),
        None => format!(".{base}.{}.{}.tmpdir", std::process::id(), unique()),
    };
    let staging_dir = Path::new(&directory).join(staging_name);
    let temp_name = match &internals.temp_name {
        Some(name) => name(path),
        None => format!("{base}.tmp"),
    };
    let temp_path = staging_dir.join(temp_name);

    let mut builder = tokio::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(&staging_dir)
        .await
        .map_err(|error| raw("write", path, error))?;
    let mut staging = Staging {
        dir: staging_dir.clone(),
        armed: true,
    };

    let paths = StagedPaths {
        staging_dir: staging_dir.clone(),
        temp_path,
    };
    let published = stage_and_publish(
        path,
        content,
        mode,
        signal,
        internals,
        create_if_absent,
        &paths,
    )
    .await;
    let removed = match &internals.remove_staging_dir {
        Some(remove) => remove(&staging_dir),
        None => tokio::fs::remove_dir_all(&staging_dir).await,
    };
    staging.armed = false;
    match (published, removed) {
        // Published: leftover staging can't turn that into a failure.
        (Ok(()), _) => Ok(()),
        (Err(failure), Ok(())) => Err(failure),
        (Err(failure), Err(cleanup)) => Err(FsError::with_source(
            FsErrorCode::NotFound,
            format!("write failed ({failure}) and temp cleanup failed ({cleanup})"),
            failure,
        )),
    }
}

async fn stage_and_publish(
    path: &str,
    content: &str,
    mode: Option<u32>,
    signal: Option<&AbortSignal>,
    internals: &Internals,
    create_if_absent: Option<&str>,
    paths: &StagedPaths,
) -> Result<(), FsError> {
    let failed = |error| raw("write", path, error);
    tokio::fs::set_permissions(&paths.staging_dir, Permissions::from_mode(0o700))
        .await
        .map_err(failed)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&paths.temp_path)
        .await
        .map_err(failed)?;
    file.set_permissions(Permissions::from_mode(0o600))
        .await
        .map_err(failed)?;
    file.write_all(content.as_bytes()).await.map_err(failed)?;
    file.flush().await.map_err(failed)?;
    file.sync_all().await.map_err(failed)?;
    if let Some(inspect) = &internals.inspect_temp {
        inspect(paths);
    }
    if let Some(mode) = mode {
        file.set_permissions(Permissions::from_mode(mode))
            .await
            .map_err(failed)?;
    }
    drop(file);

    check(signal, "write")?;
    match create_if_absent {
        Some(display_path) => {
            let linked = match &internals.link_file {
                Some(link) => link(&paths.temp_path, Path::new(path)),
                None => tokio::fs::hard_link(&paths.temp_path, path).await,
            };
            if let Err(error) = linked {
                return Err(guarded_create_failure(error, path, display_path, internals).await);
            }
        }
        None => tokio::fs::rename(&paths.temp_path, path)
            .await
            .map_err(failed)?,
    }
    Ok(())
}

/// Why a guarded create's publication failed. The link's error varies by
/// platform, so the target is looked at: something there is a competitor,
/// not a missing feature. (dsh: `throwGuardedCreateFailure`.)
async fn guarded_create_failure(
    error: io::Error,
    path: &str,
    display_path: &str,
    internals: &Internals,
) -> FsError {
    let inspected = match &internals.inspect_publication_target {
        Some(inspect) => inspect(Path::new(path)),
        None => tokio::fs::symlink_metadata(path).await,
    };
    let existing = match inspected {
        Ok(metadata) => Some(metadata),
        Err(inspection) if is_absent(&inspection) => None,
        Err(inspection) => {
            return FsError::with_source(
                FsErrorCode::IoError,
                format!("cannot write \"{display_path}\": {inspection}"),
                inspection,
            );
        }
    };
    let observed =
        || format!("cannot overwrite existing \"{display_path}\" without reading it first");
    match existing {
        Some(metadata) if !metadata.is_file() => FsError::with_source(
            FsErrorCode::NotRegularFile,
            format!("cannot write \"{display_path}\": not a regular file"),
            error,
        ),
        Some(_) => FsError::with_source(FsErrorCode::NotObserved, observed(), error),
        None if error.kind() == io::ErrorKind::AlreadyExists => {
            FsError::with_source(FsErrorCode::NotObserved, observed(), error)
        }
        None => FsError::with_source(
            FsErrorCode::IoError,
            format!("cannot write \"{display_path}\": {error}"),
            error,
        ),
    }
}

/// A file's line endings, read before they are made LF. (dsh:
/// `LineEndings`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEndings {
    Lf,
    Crlf,
}

/// CRLF made LF; a lone CR is left. (dsh: `normalizeLineEndings`.)
pub fn normalize_line_endings(content: &str) -> String {
    content.replace("\r\n", "\n")
}

/// CRLF when more of the first 4096 UTF-16 units' line breaks are CRLF
/// than LF. (dsh: `detectLineEndings`, whose `slice` counts UTF-16 units.)
fn detect_line_endings(raw: &str) -> LineEndings {
    let mut units = 0;
    let end = raw
        .char_indices()
        .find(|(_, character)| {
            let past = units >= 4096;
            units += character.len_utf16();
            past
        })
        .map_or(raw.len(), |(index, _)| index);
    let sample = &raw[..end];
    let crlf = sample.matches("\r\n").count();
    let lf = sample.matches('\n').count() - crlf;
    if crlf > lf {
        LineEndings::Crlf
    } else {
        LineEndings::Lf
    }
}

/// LF-normalized content in `line_endings`, never doubling a CR. (dsh:
/// `restoreLineEndings`.)
pub fn restore_line_endings(content: &str, line_endings: LineEndings) -> String {
    match line_endings {
        LineEndings::Lf => content.to_owned(),
        LineEndings::Crlf => normalize_line_endings(content).replace('\n', "\r\n"),
    }
}

/// A file read for editing. (dsh: `readForEdit`'s result.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForEdit {
    /// LF-normalized.
    pub content: String,
    /// The style to write back in.
    pub line_endings: LineEndings,
}

/// The file's text, LF-normalized, and its line endings, refusing a NUL
/// anywhere and invalid UTF-8. (dsh: `readForEdit`.)
pub async fn read_for_edit(
    path: &str,
    display_path: &str,
    signal: Option<&AbortSignal>,
) -> Result<ForEdit, FsError> {
    check(signal, "edit")?;
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| raw("edit", display_path, error))?;
    check(signal, "edit")?;
    if bytes.contains(&0) {
        return Err(binary("edit", display_path));
    }
    let raw_text = decode(bytes).ok_or_else(|| not_text("edit", display_path))?;
    Ok(ForEdit {
        content: normalize_line_endings(&raw_text),
        line_endings: detect_line_endings(&raw_text),
    })
}

/// The file's text, LF-normalized, as a write's diff basis, or `None` when
/// there is none to be had: not a regular file, at or over `max_bytes` by
/// the opened file's own size, changed in size while read, binary, invalid
/// UTF-8, or gone. Only an abort fails it. (dsh: `readTextForDiff`.)
pub async fn read_text_for_diff(
    path: &str,
    max_bytes: u64,
    signal: Option<&AbortSignal>,
) -> Result<Option<String>, FsError> {
    check(signal, "read")?;
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return Ok(None);
    };
    check(signal, "read")?;
    let Ok(metadata) = file.metadata().await else {
        return Ok(None);
    };
    check(signal, "read")?;
    if !metadata.is_file() || metadata.len() >= max_bytes {
        return Ok(None);
    }
    let Ok(size) = usize::try_from(metadata.len()) else {
        return Ok(None);
    };
    // One byte more catches a file grown since the stat.
    let mut buffer = vec![0; size + 1];
    let mut total = 0;
    while total < buffer.len() {
        check(signal, "read")?;
        let length = (buffer.len() - total).min(READ_CHUNK_BYTES);
        match file.read(&mut buffer[total..total + length]).await {
            Ok(0) => break,
            Ok(read) => total += read,
            Err(_) => return Ok(None),
        }
    }
    drop(file);
    check(signal, "read")?;
    if total != size {
        return Ok(None);
    }
    buffer.truncate(total);
    if buffer.contains(&0) {
        return Ok(None);
    }
    Ok(decode(buffer).map(|text| normalize_line_endings(&text)))
}

/// What [`apply_literal_edit`] made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiteralEdit {
    pub content: String,
    pub replacements: usize,
}

/// Replaces `old_string` with `new_string` in LF-normalized `content`, both
/// normalized the same way. An empty or missing search fails with
/// `FS_EDIT_NOT_FOUND`; several matches, unless `replace_all`, with
/// `FS_AMBIGUOUS_EDIT`. (dsh: `applyLiteralEdit`.)
pub fn apply_literal_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    display_path: &str,
) -> Result<LiteralEdit, FsError> {
    let old = normalize_line_endings(old_string);
    if old.is_empty() {
        return Err(FsError::new(
            FsErrorCode::EditNotFound,
            "old_string must be a non-empty string",
        ));
    }
    let new = normalize_line_endings(new_string);
    let replacements = content.matches(old.as_str()).count();
    if replacements == 0 {
        return Err(FsError::new(
            FsErrorCode::EditNotFound,
            format!("old_string was not found in \"{display_path}\""),
        ));
    }
    if !replace_all && replacements > 1 {
        return Err(FsError::new(
            FsErrorCode::AmbiguousEdit,
            format!(
                "old_string matched {replacements} times in \"{display_path}\"; provide a more \
                 specific old_string or set replace_all to true"
            ),
        ));
    }
    Ok(LiteralEdit {
        content: content.replace(old.as_str(), &new),
        replacements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_are_counted_in_the_first_4096_units() {
        assert_eq!(detect_line_endings("a\r\nb\r\nc\n"), LineEndings::Crlf);
        assert_eq!(detect_line_endings("a\r\nb\nc\n"), LineEndings::Lf);
        let late = format!("{}\r\n\r\n", "x".repeat(4096));
        assert_eq!(detect_line_endings(&late), LineEndings::Lf);
    }

    #[test]
    fn a_split_character_waits_for_its_next_chunk() {
        let mut decoder = Utf8Decoder::default();
        let e = "é".as_bytes();
        assert_eq!(decoder.feed(&e[..1]).as_deref(), Some(""));
        assert_eq!(decoder.feed(&e[1..]).as_deref(), Some("é"));
        assert_eq!(decoder.finish().as_deref(), Some(""));
        let mut cut = Utf8Decoder::default();
        cut.feed(&e[..1]);
        assert_eq!(cut.finish(), None);
        assert_eq!(Utf8Decoder::default().feed(&[0xff]), None);
    }
}

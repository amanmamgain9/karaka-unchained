//! The filesystem seam's contract, through a minimal in-memory backend, and
//! its error and abort types. (dsh: `fs/tests/service.spec.ts`.)
//!
//! dsh's registration, duplicate-service and disposal cases are Cordis's;
//! here the kernel refuses a second provider of a port, which
//! `karaka-fs-local`'s plugin test shows.

use std::collections::HashMap;
use std::error::Error;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_core::Stream;
use karaka_fs::{
    AbortController, AbortSignal, ByteRange, FileSystem, FsDirEntry, FsEditOutcome, FsEditRequest,
    FsError, FsErrorCode, FsFuture, FsInfo, FsPathInfo, FsPathType, FsTarget, FsTargetKey, FsType,
    FsVersion, FsWriteIntent, FsWriteOperation, FsWriteOutcome, TextStream,
};

/// A minimal in-memory backend. (dsh: `FakeFileSystem`.)
#[derive(Default)]
struct FakeFileSystem {
    files: Mutex<HashMap<String, String>>,
}

impl FakeFileSystem {
    fn set(&self, key: &str, content: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(key.to_owned(), content.to_owned());
    }

    fn get(&self, target: &FsTarget) -> Option<String> {
        self.files
            .lock()
            .unwrap()
            .get(target.target_key.as_str())
            .cloned()
    }

    fn text(&self, target: &FsTarget) -> Result<String, FsError> {
        self.get(target).ok_or_else(|| {
            FsError::new(
                FsErrorCode::NotFound,
                format!("not found: {}", target.display_path),
            )
        })
    }
}

impl FileSystem for FakeFileSystem {
    fn resolve<'a>(
        &'a self,
        path: &'a str,
        _cwd: Option<&'a str>,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsTarget> {
        Box::pin(async move {
            Ok(FsTarget {
                target_key: FsTargetKey::new(path),
                display_path: path.to_owned(),
            })
        })
    }

    fn process_path(&self, target: &FsTarget) -> String {
        target.target_key.as_str().to_owned()
    }

    fn file_url(&self, target: &FsTarget) -> String {
        format!("file:///{}", target.target_key.as_str())
    }

    fn contains(&self, parent: &FsTarget, child: &FsTarget) -> bool {
        child.target_key == parent.target_key
            || child
                .target_key
                .as_str()
                .starts_with(&format!("{}/", parent.target_key.as_str()))
    }

    fn stat<'a>(
        &'a self,
        target: &'a FsTarget,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsInfo>> {
        Box::pin(async move {
            Ok(self.get(target).map(|content| FsInfo {
                version: FsVersion::new("v1"),
                kind: FsType::File,
                size: Some(content.len() as u64),
            }))
        })
    }

    fn lstat<'a>(
        &'a self,
        path: &'a str,
        _cwd: Option<&'a str>,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Option<FsPathInfo>> {
        Box::pin(async move {
            Ok(self
                .files
                .lock()
                .unwrap()
                .get(path)
                .map(|content| FsPathInfo {
                    version: FsVersion::new("v1"),
                    kind: FsPathType::File,
                    size: Some(content.len() as u64),
                }))
        })
    }

    fn read_text<'a>(
        &'a self,
        target: &'a FsTarget,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, String> {
        Box::pin(async move { self.text(target) })
    }

    fn stream_text<'a>(
        &'a self,
        target: &'a FsTarget,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, TextStream> {
        Box::pin(async move {
            let content = self.text(target)?;
            let stream: TextStream = Box::pin(Once(Some(Ok(content))));
            Ok(stream)
        })
    }

    fn read_bytes<'a>(
        &'a self,
        target: &'a FsTarget,
        _signal: Option<&'a AbortSignal>,
        max_bytes: u64,
    ) -> FsFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let bytes = self.text(target)?.into_bytes();
            if bytes.len() as u64 > max_bytes {
                return Err(FsError::new(
                    FsErrorCode::TooLarge,
                    format!("too large: {}", target.display_path),
                ));
            }
            Ok(bytes)
        })
    }

    fn read_byte_range<'a>(
        &'a self,
        target: &'a FsTarget,
        range: ByteRange,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let bytes = self.text(target)?.into_bytes();
            let start = (range.offset as usize).min(bytes.len());
            let end = (start + range.length as usize).min(bytes.len());
            Ok(bytes[start..end].to_vec())
        })
    }

    fn list_dir<'a>(
        &'a self,
        target: &'a FsTarget,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, Vec<FsDirEntry>> {
        Box::pin(async move {
            if target.target_key.as_str() != "skills" {
                return Err(FsError::new(
                    FsErrorCode::NotDirectory,
                    format!("not a directory: {}", target.display_path),
                ));
            }
            Ok(vec![FsDirEntry {
                name: "alpha.md".into(),
                kind: FsType::File,
                target: FsTarget {
                    target_key: FsTargetKey::new("skills/alpha.md"),
                    display_path: "skills/alpha.md".into(),
                },
                version: Some(FsVersion::new("v1")),
                size: Some(2),
            }])
        })
    }

    fn write_text<'a>(
        &'a self,
        target: &'a FsTarget,
        content: &'a str,
        _expected: Option<&'a FsWriteIntent>,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsWriteOutcome> {
        Box::pin(async move {
            let before = self.get(target);
            self.set(target.target_key.as_str(), content);
            Ok(FsWriteOutcome {
                operation: if before.is_some() {
                    FsWriteOperation::Update
                } else {
                    FsWriteOperation::Create
                },
                version: FsVersion::new("v2"),
                before,
                after: content.to_owned(),
            })
        })
    }

    fn edit_text<'a>(
        &'a self,
        target: &'a FsTarget,
        edit: &'a FsEditRequest,
        _expected: Option<&'a FsVersion>,
        _signal: Option<&'a AbortSignal>,
    ) -> FsFuture<'a, FsEditOutcome> {
        Box::pin(async move {
            let before = self.get(target).unwrap_or_default();
            let after = before.replace(&edit.old_string, &edit.new_string);
            self.set(target.target_key.as_str(), &after);
            Ok(FsEditOutcome {
                version: FsVersion::new("v3"),
                before,
                after,
            })
        })
    }
}

/// A stream of one item.
struct Once(Option<Result<String, FsError>>);

impl Stream for Once {
    type Item = Result<String, FsError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(self.0.take())
    }
}

async fn collect(mut stream: TextStream) -> Result<String, FsError> {
    let mut out = String::new();
    while let Some(chunk) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        out.push_str(&chunk?);
    }
    Ok(out)
}

async fn fake() -> (FakeFileSystem, FsTarget) {
    let fs = FakeFileSystem::default();
    let target = fs.resolve("a.txt", None, None).await.unwrap();
    (fs, target)
}

// "registers as ctx.fs and serves the primitives"
#[tokio::test]
async fn a_backend_serves_the_primitives_and_maps_no_host_path_by_default() {
    let (fs, target) = fake().await;
    let fs: Arc<dyn FileSystem> = Arc::new(fs);
    assert_eq!(fs.process_path_from_host_path("/host/file"), None);
    fs.write_text(&target, "hi", None, None).await.unwrap();
    assert_eq!(
        fs.stat(&target, None).await.unwrap().map(|info| info.kind),
        Some(FsType::File)
    );
    assert_eq!(fs.read_text(&target, None).await.unwrap(), "hi");
}

// "streamText yields the same text readText returns"
#[tokio::test]
async fn a_stream_yields_the_text_a_read_returns() {
    let (fs, target) = fake().await;
    fs.set("a.txt", "one\ntwo");
    let streamed = collect(fs.stream_text(&target, None).await.unwrap())
        .await
        .unwrap();
    assert_eq!(streamed, fs.read_text(&target, None).await.unwrap());
}

// "readByteRange returns the window, shorter at the end and empty past it"
#[tokio::test]
async fn a_byte_range_is_the_window_shorter_at_the_end_and_empty_past_it() {
    let fs = FakeFileSystem::default();
    fs.set("a.bin", "hello");
    let target = fs.resolve("a.bin", None, None).await.unwrap();
    let range = |offset, length| ByteRange { offset, length };
    assert_eq!(
        fs.read_byte_range(&target, range(1, 3), None)
            .await
            .unwrap(),
        b"ell"
    );
    assert_eq!(
        fs.read_byte_range(&target, range(3, 10), None)
            .await
            .unwrap(),
        b"lo"
    );
    assert!(
        fs.read_byte_range(&target, range(9, 2), None)
            .await
            .unwrap()
            .is_empty()
    );
}

// "readBytes returns raw content and enforces the byte cap with
// FS_TOO_LARGE"
#[tokio::test]
async fn raw_bytes_are_capped() {
    let fs = FakeFileSystem::default();
    fs.set("a.bin", "hi");
    let target = fs.resolve("a.bin", None, None).await.unwrap();
    assert_eq!(fs.read_bytes(&target, None, 2).await.unwrap(), b"hi");
    assert_eq!(
        fs.read_bytes(&target, None, 1).await.unwrap_err().code(),
        FsErrorCode::TooLarge
    );
}

// "rejects watching when a provider does not implement observation"
#[tokio::test]
async fn a_backend_without_watching_refuses_it() {
    let (fs, target) = fake().await;
    let controller = AbortController::new();
    let error = fs
        .watch(&target, Arc::new(|_| {}), &controller.signal())
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::IoError);
    assert_eq!(
        error.to_string(),
        "Filesystem watching is not supported by this provider."
    );
    // An aborted start is an abort, before the refusal.
    let error = fs
        .watch(&target, Arc::new(|_| {}), &AbortSignal::aborted())
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::Aborted);
}

// "listDir returns child entry targets without reading file content"
#[tokio::test]
async fn a_listing_gives_child_targets() {
    let fs = FakeFileSystem::default();
    let skills = fs.resolve("skills", None, None).await.unwrap();
    let entries = fs.list_dir(&skills, None).await.unwrap();
    assert_eq!(
        entries,
        [FsDirEntry {
            name: "alpha.md".into(),
            kind: FsType::File,
            target: FsTarget {
                target_key: FsTargetKey::new("skills/alpha.md"),
                display_path: "skills/alpha.md".into(),
            },
            version: Some(FsVersion::new("v1")),
            size: Some(2),
        }]
    );
}

// "stat returns undefined for an absent target"
#[tokio::test]
async fn an_absent_target_has_no_metadata() {
    let fs = FakeFileSystem::default();
    let missing = fs.resolve("missing.txt", None, None).await.unwrap();
    assert_eq!(fs.stat(&missing, None).await.unwrap(), None);
}

// "lstat returns path metadata before resolving a target"
#[tokio::test]
async fn lstat_reads_a_path_without_a_target() {
    let fs = FakeFileSystem::default();
    fs.set("a.txt", "hi");
    assert_eq!(
        fs.lstat("a.txt", None, None).await.unwrap(),
        Some(FsPathInfo {
            version: FsVersion::new("v1"),
            kind: FsPathType::File,
            size: Some(2),
        })
    );
    assert_eq!(fs.lstat("missing.txt", None, None).await.unwrap(), None);
}

// "FsTargetKey and FsVersion brand a string"
#[test]
fn keys_and_versions_are_their_strings() {
    assert_eq!(FsTargetKey::new("k").as_str(), "k");
    assert_eq!(FsVersion::new("v").as_str(), "v");
}

// "carries a stable code and HarnessError name"
#[test]
fn an_error_carries_its_code() {
    let error = FsError::new(FsErrorCode::NotFound, "nope");
    assert_eq!(error.code(), FsErrorCode::NotFound);
    assert_eq!(error.code().as_str(), "FS_NOT_FOUND");
    assert_eq!(error.to_string(), "nope");
}

// "chains an underlying cause through ErrorOptions"
#[test]
fn an_error_chains_its_cause() {
    let root = io::Error::from(io::ErrorKind::PermissionDenied);
    let error = FsError::with_source(FsErrorCode::Aborted, "cannot read", root);
    assert_eq!(error.code(), FsErrorCode::Aborted);
    let source = error.source().expect("a cause");
    assert_eq!(
        source.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::PermissionDenied)
    );
}

#[test]
fn every_code_is_dshs() {
    let codes = [
        (FsErrorCode::NotFound, "FS_NOT_FOUND"),
        (FsErrorCode::NotDirectory, "FS_NOT_DIRECTORY"),
        (FsErrorCode::NotText, "FS_NOT_TEXT"),
        (FsErrorCode::NotRegularFile, "FS_NOT_REGULAR_FILE"),
        (FsErrorCode::TooLarge, "FS_TOO_LARGE"),
        (FsErrorCode::PermissionDenied, "FS_PERMISSION_DENIED"),
        (FsErrorCode::SandboxDenied, "FS_SANDBOX_DENIED"),
        (FsErrorCode::IoError, "FS_IO_ERROR"),
        (FsErrorCode::StaleVersion, "FS_STALE_VERSION"),
        (FsErrorCode::NotObserved, "FS_NOT_OBSERVED"),
        (FsErrorCode::AmbiguousEdit, "FS_AMBIGUOUS_EDIT"),
        (FsErrorCode::EditNotFound, "FS_EDIT_NOT_FOUND"),
        (FsErrorCode::Aborted, "FS_ABORTED"),
    ];
    for (code, name) in codes {
        assert_eq!(code.as_str(), name);
        assert_eq!(code.to_string(), name);
    }
}

#[tokio::test]
async fn an_abort_is_seen_by_every_signal_once() {
    let controller = AbortController::new();
    let signal = controller.signal();
    let copy = signal.clone();
    assert!(!signal.is_aborted());
    let waiting = tokio::spawn(async move { copy.wait().await });
    assert!(controller.abort());
    assert!(!controller.abort(), "only the first abort counts");
    waiting.await.unwrap();
    assert!(signal.is_aborted());
    assert!(AbortSignal::aborted().is_aborted());
    assert!(!AbortSignal::never().is_aborted());
    // A dropped controller never aborts.
    let dropped = AbortController::new();
    let orphan = dropped.signal();
    drop(dropped);
    assert!(!orphan.is_aborted());
}

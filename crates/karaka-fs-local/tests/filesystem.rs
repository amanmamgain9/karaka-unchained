//! The local backend through the port: resolution, metadata, reads, guarded
//! writes and edits, races, symlink identity, and the plugin. (dsh:
//! `fs-local/tests/filesystem.spec.ts`.)
//!
//! Not ported: the Windows drive-relative case; the metadata-cancellation
//! case, which mocks `node:fs/promises` to hold a probe open (here an abort
//! is checked before and after each probe, as dsh does); and dsh's V8
//! allocation ceiling on the diff limit, which Rust has no counterpart of.
//! The per-target lock release is a unit test in `local.rs`.

mod common;

use std::sync::Arc;

use common::*;
use futures_util::StreamExt;
use karaka_fs::{
    AbortController, AbortSignal, ByteRange, FileSystem, FsEditRequest, FsErrorCode, FsPathType,
    FsTarget, FsType, FsVersion, FsWriteIntent, FsWriteOperation,
};
use karaka_fs_local::fsio::Internals;
use karaka_fs_local::{LocalFileSystem, LocalFsConfig, LocalFsPlugin};
use karaka_kernel::Runtime;

fn local(dir: &TempDir) -> LocalFileSystem {
    LocalFileSystem::new(LocalFsConfig {
        cwd: dir.str(),
        ..LocalFsConfig::default()
    })
    .unwrap()
}

fn limited(dir: &TempDir, diff_basis_max_bytes: u64) -> LocalFileSystem {
    LocalFileSystem::new(LocalFsConfig {
        cwd: dir.str(),
        diff_basis_max_bytes,
    })
    .unwrap()
}

async fn target(fs: &LocalFileSystem, path: &str) -> FsTarget {
    fs.resolve(path, None, None).await.unwrap()
}

async fn version_of(fs: &LocalFileSystem, target: &FsTarget) -> FsVersion {
    fs.stat(target, None)
        .await
        .unwrap()
        .expect("the target exists")
        .version
}

fn edit(old: &str, new: &str, replace_all: bool) -> FsEditRequest {
    FsEditRequest {
        old_string: old.into(),
        new_string: new.into(),
        replace_all,
    }
}

// "registers LocalFileSystem as ctx.fs with a default cwd"
#[test]
fn the_default_config_is_the_process_cwd_and_ten_mebibytes() {
    let config = LocalFsConfig::default();
    assert_eq!(
        config.cwd,
        std::env::current_dir().unwrap().to_str().unwrap()
    );
    assert_eq!(config.diff_basis_max_bytes, 10 * 1024 * 1024);
}

// "rejects non-positive ... diff-basis limits"
#[test]
fn a_zero_diff_limit_is_refused() {
    let error = LocalFileSystem::new(LocalFsConfig {
        diff_basis_max_bytes: 0,
        ..LocalFsConfig::default()
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "fs-local: diff_basis_max_bytes must be a positive integer"
    );
}

// "maps only absolute host paths into its process path namespace"
#[test]
fn only_absolute_host_paths_map_into_the_process_world() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    assert_eq!(
        fs.process_path_from_host_path(&dir.at("image.png")),
        Some(dir.at("image.png"))
    );
    assert_eq!(
        fs.process_path_from_host_path("/a/./b/../image.png"),
        Some("/a/image.png".into())
    );
    assert_eq!(fs.process_path_from_host_path("image.png"), None);
}

// "resolves a relative path against opts.cwd, not config.cwd"
#[tokio::test]
async fn a_relative_path_resolves_against_the_cwd_asked_for() {
    let dir = TempDir::new("fs");
    let other = TempDir::new("fs-other");
    let fs = local(&dir);
    write(other.join("x.txt"), "in other");
    let via_other = fs.resolve("x.txt", Some(&other.str()), None).await.unwrap();
    assert_eq!(fs.read_text(&via_other, None).await.unwrap(), "in other");
    let via_config = target(&fs, "x.txt").await;
    assert_eq!(
        fs.read_text(&via_config, None).await.unwrap_err().code(),
        FsErrorCode::NotFound
    );
}

// "ignores opts.cwd for an ABSOLUTE path"
#[tokio::test]
async fn an_absolute_path_ignores_the_cwd() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("abs.txt"), "absolute");
    let target = fs
        .resolve(&dir.at("abs.txt"), Some("/nonexistent-base"), None)
        .await
        .unwrap();
    assert_eq!(fs.read_text(&target, None).await.unwrap(), "absolute");
}

// "honors a pre-aborted signal", and one aborted while in flight
#[tokio::test]
async fn an_aborted_resolve_is_aborted() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let error = fs
        .resolve("a.txt", None, Some(&AbortSignal::aborted()))
        .await
        .unwrap_err();
    assert_eq!(
        (error.code(), error.to_string()),
        (FsErrorCode::Aborted, "resolve aborted".into())
    );
    let controller = AbortController::new();
    let signal = controller.signal();
    let pending = fs.resolve("a.txt", None, Some(&signal));
    controller.abort();
    assert_eq!(pending.await.unwrap_err().code(), FsErrorCode::Aborted);
}

// "projects process paths, file URLs, and canonical containment"
#[tokio::test]
async fn process_paths_urls_and_containment() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    std::fs::create_dir(dir.join("nested")).unwrap();
    write(dir.join("nested/file.txt"), "text");
    let root = target(&fs, ".").await;
    let child = target(&fs, "nested/file.txt").await;
    let outside = target(&fs, "..").await;
    assert_eq!(fs.process_path(&child), real(dir.join("nested/file.txt")));
    assert_eq!(
        fs.file_url(&child),
        format!("file://{}", real(dir.join("nested/file.txt")))
    );
    assert!(fs.contains(&root, &root));
    assert!(fs.contains(&root, &child));
    assert!(!fs.contains(&root, &outside));
}

// pathToFileURL's escaping.
#[tokio::test]
async fn a_file_url_escapes_as_nodes_does() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let target = target(&fs, "a b#c?d%e\u{e9}.txt").await;
    assert_eq!(
        fs.file_url(&target),
        format!("file://{}/a%20b%23c%3Fd%25e%C3%A9.txt", dir.real())
    );
}

// stat: "returns file metadata, directory type, and undefined for absent"
#[tokio::test]
async fn stat_gives_metadata_or_none() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "hello");
    let info = fs
        .stat(&target(&fs, "a.txt").await, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((info.kind, info.size), (FsType::File, Some(5)));
    assert_eq!(
        fs.stat(&target(&fs, ".").await, None)
            .await
            .unwrap()
            .map(|info| info.kind),
        Some(FsType::Directory)
    );
    assert_eq!(
        fs.stat(&target(&fs, "missing.txt").await, None)
            .await
            .unwrap(),
        None
    );
    let error = fs
        .stat(&target(&fs, "a.txt").await, Some(&AbortSignal::aborted()))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "stat aborted");
}

// "changes version after a same-size rewrite even when mtime is restored"
#[tokio::test]
async fn a_same_size_rewrite_changes_the_version() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let path = dir.join("same-size.txt");
    write(&path, "first");
    let target = target(&fs, &dir.at("same-size.txt")).await;
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let before = version_of(&fs, &target).await;
    fs.write_text(&target, "other", None, None).await.unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 5);
    assert_ne!(version_of(&fs, &target).await, before);
}

// lstat: the link not followed, opts.cwd, an aborted signal, a blank path
#[tokio::test]
async fn lstat_sees_the_link_itself() {
    let dir = TempDir::new("fs");
    let other = TempDir::new("fs-other");
    let fs = local(&dir);
    write(dir.join("real.txt"), "hello");
    symlink(dir.join("real.txt"), dir.join("link.txt"));
    let kind = |info: Option<karaka_fs::FsPathInfo>| info.map(|info| info.kind);
    assert_eq!(
        kind(fs.lstat("real.txt", None, None).await.unwrap()),
        Some(FsPathType::File)
    );
    assert_eq!(
        kind(fs.lstat("link.txt", None, None).await.unwrap()),
        Some(FsPathType::Symlink)
    );
    assert_eq!(fs.lstat("missing.txt", None, None).await.unwrap(), None);

    write(other.join("x.txt"), "in other");
    assert_eq!(
        kind(fs.lstat("x.txt", Some(&other.str()), None).await.unwrap()),
        Some(FsPathType::File)
    );
    let aborted = fs
        .lstat("x.txt", Some(&other.str()), Some(&AbortSignal::aborted()))
        .await
        .unwrap_err();
    assert_eq!(aborted.to_string(), "lstat aborted");
    let blank = fs.lstat("   ", None, None).await.unwrap_err();
    assert_eq!(blank.code(), FsErrorCode::NotFound);
}

// readText / streamText
#[tokio::test]
async fn text_reads_and_streams_and_their_refusals() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "one\ntwo\nthree");
    let a = target(&fs, "a.txt").await;
    assert_eq!(fs.read_text(&a, None).await.unwrap(), "one\ntwo\nthree");
    let mut stream = fs.stream_text(&a, None).await.unwrap();
    let mut streamed = String::new();
    while let Some(chunk) = stream.next().await {
        streamed.push_str(&chunk.unwrap());
    }
    assert_eq!(streamed, "one\ntwo\nthree");

    let code = |result: Result<String, karaka_fs::FsError>| result.unwrap_err().code();
    assert_eq!(
        code(fs.read_text(&target(&fs, "nope").await, None).await),
        FsErrorCode::NotFound
    );
    assert_eq!(
        code(fs.read_text(&target(&fs, ".").await, None).await),
        FsErrorCode::NotRegularFile
    );
    write(dir.join("bin"), [0x68, 0x00, 0x69]);
    assert_eq!(
        code(fs.read_text(&target(&fs, "bin").await, None).await),
        FsErrorCode::NotText
    );
    write(dir.join("bad"), [0x68, 0xff, 0x69]);
    assert_eq!(
        code(fs.read_text(&target(&fs, "bad").await, None).await),
        FsErrorCode::NotText
    );
    // A stream's refusal comes with its first chunk, as dsh's generator's.
    let mut missing = fs
        .stream_text(&target(&fs, "nope").await, None)
        .await
        .unwrap();
    assert_eq!(
        missing.next().await.unwrap().unwrap_err().code(),
        FsErrorCode::NotFound
    );
}

// readBytes
#[tokio::test]
async fn raw_reads_are_capped_and_undecoded() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let raw = [0x68, 0x00, 0x69, 0xff];
    write(dir.join("a.bin"), raw);
    let a = target(&fs, "a.bin").await;
    assert_eq!(fs.read_bytes(&a, None, 4).await.unwrap(), raw);
    write(dir.join("four.bin"), [1; 4]);
    let four = target(&fs, "four.bin").await;
    assert_eq!(fs.read_bytes(&four, None, 4).await.unwrap().len(), 4);
    let too_large = fs.read_bytes(&four, None, 3).await.unwrap_err();
    assert_eq!(too_large.code(), FsErrorCode::TooLarge);
    assert_eq!(
        too_large.to_string(),
        format!(
            "cannot read \"{}\": 4 bytes exceeds the 3-byte limit",
            dir.at("four.bin")
        )
    );

    let code = |result: Result<Vec<u8>, karaka_fs::FsError>| result.unwrap_err().code();
    assert_eq!(
        code(fs.read_bytes(&target(&fs, "nope").await, None, 1024).await),
        FsErrorCode::NotFound
    );
    assert_eq!(
        code(fs.read_bytes(&target(&fs, ".").await, None, 1024).await),
        FsErrorCode::NotRegularFile
    );
    let controller = AbortController::new();
    assert_eq!(
        fs.read_bytes(&a, Some(&controller.signal()), 1024)
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        code(fs.read_bytes(&a, Some(&AbortSignal::aborted()), 1024).await),
        FsErrorCode::Aborted
    );
}

// "bounds content I/O when a file grows after stat preflight"
#[tokio::test]
async fn a_file_that_grows_after_its_stat_is_still_capped() {
    let dir = TempDir::new("fs");
    write(dir.join("a.bin"), [1; 4]);
    let path = dir.join("a.bin");
    let fs = local(&dir).with_internals(Internals {
        inspect_read_bytes_after_stat: Some(Arc::new(move |_| write(&path, vec![2; 1024 * 1024]))),
        ..Internals::default()
    });
    let a = target(&fs, "a.bin").await;
    let error = fs.read_bytes(&a, None, 4).await.unwrap_err();
    assert_eq!(error.code(), FsErrorCode::TooLarge);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot read \"{}\": content exceeds the 4-byte limit",
            dir.at("a.bin")
        )
    );
}

// readByteRange
#[tokio::test]
async fn a_byte_window_is_read_alone() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let ramp: Vec<u8> = (0..=255).collect();
    write(dir.join("ramp.bin"), &ramp);
    let ramp_target = target(&fs, "ramp.bin").await;
    let window = |offset, length| ByteRange { offset, length };
    let controller = AbortController::new();
    assert_eq!(
        fs.read_byte_range(&ramp_target, window(100, 4), Some(&controller.signal()))
            .await
            .unwrap(),
        [100, 101, 102, 103]
    );
    assert_eq!(
        fs.read_byte_range(&ramp_target, window(253, 10), None)
            .await
            .unwrap(),
        [253, 254, 255]
    );
    for (offset, length) in [(256, 10), (1000, 10), (0, 0)] {
        assert!(
            fs.read_byte_range(&ramp_target, window(offset, length), None)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let raw = [0x00, 0xff, 0xfe, 0x41];
    write(dir.join("a.bin"), raw);
    let a = target(&fs, "a.bin").await;
    assert_eq!(
        fs.read_byte_range(&a, window(0, 4), None).await.unwrap(),
        raw
    );
    let code = |result: Result<Vec<u8>, karaka_fs::FsError>| result.unwrap_err().code();
    assert_eq!(
        code(
            fs.read_byte_range(&target(&fs, "nope").await, window(0, 1), None)
                .await
        ),
        FsErrorCode::NotFound
    );
    assert_eq!(
        code(
            fs.read_byte_range(&target(&fs, ".").await, window(0, 1), None)
                .await
        ),
        FsErrorCode::NotRegularFile
    );
    assert_eq!(
        code(
            fs.read_byte_range(&a, window(0, 1), Some(&AbortSignal::aborted()))
                .await
        ),
        FsErrorCode::Aborted
    );
}

// listDir
#[tokio::test]
async fn a_listing_through_the_port() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    std::fs::create_dir_all(dir.join("skills/dir-skill")).unwrap();
    write(dir.join("skills/zeta.md"), "zeta");
    write(dir.join("skills/alpha.md"), "alpha");
    symlink(
        dir.join("skills/missing-target"),
        dir.join("skills/broken-link"),
    );
    let entries = fs
        .list_dir(&target(&fs, "skills").await, None)
        .await
        .unwrap();
    let names: Vec<(&str, FsType, &str)> = entries
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                entry.kind,
                entry.target.display_path.as_str(),
            )
        })
        .collect();
    let at = |name: &str| dir.at(&format!("skills/{name}"));
    assert_eq!(
        names,
        [
            ("alpha.md", FsType::File, at("alpha.md").as_str()),
            ("broken-link", FsType::Other, at("broken-link").as_str()),
            ("dir-skill", FsType::Directory, at("dir-skill").as_str()),
            ("zeta.md", FsType::File, at("zeta.md").as_str()),
        ]
    );
    for entry in entries.iter().filter(|entry| entry.version.is_some()) {
        assert_eq!(
            entry.target.target_key.as_str(),
            real(&entry.target.display_path)
        );
    }
    assert_eq!(entries[0].size, Some(5));
    assert_eq!(entries[1].version, None);
    assert_eq!(entries[2].size, None);

    let code = |result: Result<_, karaka_fs::FsError>| result.unwrap_err().code();
    assert_eq!(
        code(fs.list_dir(&target(&fs, "missing").await, None).await),
        FsErrorCode::NotFound
    );
    assert_eq!(
        code(
            fs.list_dir(&target(&fs, "skills/zeta.md").await, None)
                .await
        ),
        FsErrorCode::NotDirectory
    );
    assert_eq!(
        code(
            fs.list_dir(&target(&fs, "skills").await, Some(&AbortSignal::aborted()))
                .await
        ),
        FsErrorCode::Aborted
    );
}

// writeText: "createIfAbsent creates a new file", and "rejects an existing
// file as FS_NOT_OBSERVED"
#[tokio::test]
async fn a_guarded_create_makes_a_file_and_refuses_an_existing_one() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let outcome = fs
        .write_text(
            &target(&fs, "new.txt").await,
            "fresh",
            Some(&FsWriteIntent::CreateIfAbsent),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.operation, FsWriteOperation::Create);
    assert_eq!(read(dir.join("new.txt")), "fresh");

    write(dir.join("a.txt"), "old");
    let error = fs
        .write_text(
            &target(&fs, "a.txt").await,
            "new",
            Some(&FsWriteIntent::CreateIfAbsent),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotObserved);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot overwrite existing \"{}\" without reading it first",
            dir.at("a.txt")
        )
    );
    assert_eq!(read(dir.join("a.txt")), "old");
}

// "createIfAbsent preserves a competitor created after the initial probe"
#[tokio::test]
async fn a_guarded_create_keeps_a_competitors_file() {
    let dir = TempDir::new("fs");
    let path = dir.join("a.txt");
    let fs = local(&dir).with_internals(Internals {
        inspect_temp: Some(Arc::new(move |_| write(&path, "competitor"))),
        ..Internals::default()
    });
    let error = fs
        .write_text(
            &target(&fs, "a.txt").await,
            "ours",
            Some(&FsWriteIntent::CreateIfAbsent),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotObserved);
    assert_eq!(read(dir.join("a.txt")), "competitor");
}

// "reports a createIfAbsent race with the unresolved display path"
#[tokio::test]
async fn a_create_race_names_the_unresolved_path() {
    let dir = TempDir::new("fs");
    std::fs::create_dir(dir.join("real-workspace")).unwrap();
    symlink(dir.join("real-workspace"), dir.join("linked-workspace"));
    let competitor = dir.join("real-workspace/a.txt");
    let fs = local(&dir).with_internals(Internals {
        inspect_temp: Some(Arc::new(move |_| write(&competitor, "competitor"))),
        ..Internals::default()
    });
    let error = fs
        .write_text(
            &target(&fs, "linked-workspace/a.txt").await,
            "ours",
            Some(&FsWriteIntent::CreateIfAbsent),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotObserved);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot overwrite existing \"{}\" without reading it first",
            dir.at("linked-workspace/a.txt")
        )
    );
    assert_eq!(read(dir.join("real-workspace/a.txt")), "competitor");
}

// "createIfAbsent rejects a competing directory as not a regular file"
#[tokio::test]
async fn a_guarded_create_refuses_a_competing_directory() {
    let dir = TempDir::new("fs");
    let path = dir.join("a.txt");
    let fs = local(&dir).with_internals(Internals {
        inspect_temp: Some(Arc::new(move |_| std::fs::create_dir(&path).unwrap())),
        ..Internals::default()
    });
    let error = fs
        .write_text(
            &target(&fs, "a.txt").await,
            "ours",
            Some(&FsWriteIntent::CreateIfAbsent),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotRegularFile);
    assert!(dir.join("a.txt").is_dir());
}

// "createIfAbsent rejects and preserves a dangling symbolic link"
#[tokio::test]
async fn a_guarded_create_keeps_a_dangling_link() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    symlink(dir.join("missing-target"), dir.join("dangling"));
    let error = fs
        .write_text(
            &target(&fs, "dangling").await,
            "ours",
            Some(&FsWriteIntent::CreateIfAbsent),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotRegularFile);
    assert!(!dir.join("missing-target").exists());
    assert!(dir.join("dangling").is_symlink());
}

// replaceIfVersion: a match, a stale version, a deleted target, and the
// version it returns
#[tokio::test]
async fn a_versioned_replace_checks_the_version() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "old");
    let a = target(&fs, "a.txt").await;
    let version = version_of(&fs, &a).await;
    let outcome = fs
        .write_text(
            &a,
            "a much longer replacement body",
            Some(&FsWriteIntent::ReplaceIfVersion(version.clone())),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.operation, FsWriteOperation::Update);
    assert_ne!(outcome.version, version);
    assert_eq!(outcome.version, version_of(&fs, &a).await);
    assert_eq!(read(dir.join("a.txt")), "a much longer replacement body");

    let stale = version_of(&fs, &a).await;
    write(dir.join("a.txt"), "changed-externally");
    let error = fs
        .write_text(
            &a,
            "v2",
            Some(&FsWriteIntent::ReplaceIfVersion(stale.clone())),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::StaleVersion);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot write \"{}\": file changed since it was read",
            dir.at("a.txt")
        )
    );

    let current = version_of(&fs, &a).await;
    std::fs::remove_file(dir.join("a.txt")).unwrap();
    let error = fs
        .write_text(
            &a,
            "v2",
            Some(&FsWriteIntent::ReplaceIfVersion(current)),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::StaleVersion);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot write \"{}\": file no longer exists",
            dir.at("a.txt")
        )
    );
    assert!(!dir.join("a.txt").exists());
}

// "rejects writing onto a directory", with and without a guard
#[tokio::test]
async fn a_directory_is_never_written() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let root = target(&fs, ".").await;
    for intent in [None, Some(&FsWriteIntent::CreateIfAbsent)] {
        let error = fs.write_text(&root, "x", intent, None).await.unwrap_err();
        assert_eq!(error.code(), FsErrorCode::NotRegularFile);
    }
}

// The unguarded provider: create, overwrite, and the before and after
// bases
#[tokio::test]
async fn an_unguarded_write_creates_or_overwrites() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let created = fs
        .write_text(&target(&fs, "new.txt").await, "fresh", None, None)
        .await
        .unwrap();
    assert_eq!(created.operation, FsWriteOperation::Create);
    assert_eq!((created.before, created.after.as_str()), (None, "fresh"));

    write(dir.join("a.txt"), "old body");
    let updated = fs
        .write_text(&target(&fs, "a.txt").await, "new body", None, None)
        .await
        .unwrap();
    assert_eq!(updated.operation, FsWriteOperation::Update);
    assert_eq!(updated.before.as_deref(), Some("old body"));
    assert_eq!(updated.after, "new body");
    assert_eq!(read(dir.join("a.txt")), "new body");

    write(dir.join("crlf.txt"), "a\r\nb\r\nc\r\n");
    let crlf = fs
        .write_text(
            &target(&fs, "crlf.txt").await,
            "a\r\nB\r\nc\r\n",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(crlf.before.as_deref(), Some("a\nb\nc\n"));
    assert_eq!(crlf.after, "a\nB\nc\n");
    assert_eq!(read(dir.join("crlf.txt")), "a\r\nB\r\nc\r\n");

    for (name, bytes) in [
        ("bin", &[0x00, 0x01, 0x02][..]),
        ("bad", &[0x68, 0xff, 0x69]),
    ] {
        write(dir.join(name), bytes);
        let outcome = fs
            .write_text(&target(&fs, name).await, "now text", None, None)
            .await
            .unwrap();
        assert_eq!(outcome.operation, FsWriteOperation::Update);
        assert_eq!(outcome.before, None);
        assert_eq!(outcome.after, "now text");
    }
}

// The diff basis's bound: the prior file at it, the new content at it, by
// bytes, and both below it
#[tokio::test]
async fn the_diff_basis_is_declined_at_its_bound_on_either_side() {
    let dir = TempDir::new("fs");
    let fs = limited(&dir, 8);
    let cases = [
        ("big.txt", "12345678", "tiny", None),
        ("grow.txt", "tiny", "12345678", None),
        ("cjk.txt", "tiny", "你好吗", None),
        ("small.txt", "1234567", "new", Some("1234567")),
    ];
    for (name, before, after, basis) in cases {
        write(dir.join(name), before);
        let outcome = fs
            .write_text(&target(&fs, name).await, after, None, None)
            .await
            .unwrap();
        assert_eq!(outcome.operation, FsWriteOperation::Update, "{name}");
        assert_eq!(outcome.before.as_deref(), basis, "{name}");
        assert_eq!(outcome.after, after, "{name}");
    }
}

// "honors a pre-aborted signal without creating the file"
#[tokio::test]
async fn an_aborted_write_creates_nothing() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    let error = fs
        .write_text(
            &target(&fs, "aborted.txt").await,
            "x",
            None,
            Some(&AbortSignal::aborted()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::Aborted);
    assert!(!dir.join("aborted.txt").exists());
}

// "two concurrent guarded writes: one updates, the other is rejected as
// stale", and "concurrent write vs edit at the same version"
#[tokio::test]
async fn concurrent_guarded_mutations_let_one_win() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "base");
    let a = target(&fs, "a.txt").await;
    let version = version_of(&fs, &a).await;
    let intent = FsWriteIntent::ReplaceIfVersion(version.clone());
    let (one, two) = tokio::join!(
        fs.write_text(&a, "one", Some(&intent), None),
        fs.write_text(&a, "two", Some(&intent), None),
    );
    let mut results = [one.map(|_| ()), two.map(|_| ())];
    results.sort_by_key(Result::is_err);
    assert!(results[0].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().code(),
        FsErrorCode::StaleVersion
    );

    write(dir.join("b.txt"), "base");
    let b = target(&fs, "b.txt").await;
    let version = version_of(&fs, &b).await;
    let intent = FsWriteIntent::ReplaceIfVersion(version.clone());
    let change = edit("base", "edited", false);
    let (written, edited) = tokio::join!(
        fs.write_text(&b, "written", Some(&intent), None),
        fs.edit_text(&b, &change, Some(&version), None),
    );
    let mut results = [written.map(|_| ()), edited.map(|_| ())];
    results.sort_by_key(Result::is_err);
    assert!(results[0].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().code(),
        FsErrorCode::StaleVersion
    );
}

// editText: at the matching version, unguarded, and the CRLF basis
#[tokio::test]
async fn a_literal_edit_keeps_the_files_line_endings() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "hello world");
    let a = target(&fs, "a.txt").await;
    let version = version_of(&fs, &a).await;
    let outcome = fs
        .edit_text(&a, &edit("world", "there", false), Some(&version), None)
        .await
        .unwrap();
    assert_eq!(outcome.after, "hello there");
    assert_eq!(read(dir.join("a.txt")), "hello there");
    // With no guard, the current content is edited.
    fs.edit_text(&a, &edit("there", "again", false), None, None)
        .await
        .unwrap();
    assert_eq!(read(dir.join("a.txt")), "hello again");

    write(dir.join("crlf.txt"), "a\r\nOLD\r\nb\r\n");
    let outcome = fs
        .edit_text(
            &target(&fs, "crlf.txt").await,
            &edit("OLD", "NEW", false),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.before, "a\nOLD\nb\n");
    assert_eq!(outcome.after, "a\nNEW\nb\n");
    assert_eq!(read(dir.join("crlf.txt")), "a\r\nNEW\r\nb\r\n");
}

// "checks the stale version BEFORE literal matching", a missing target
// with and without a guard, a deleted one, a directory
#[tokio::test]
async fn an_edits_freshness_is_checked_before_its_match() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "hello world");
    let a = target(&fs, "a.txt").await;
    let stale = version_of(&fs, &a).await;
    write(dir.join("a.txt"), "goodbye");
    let error = fs
        .edit_text(&a, &edit("world", "there", false), Some(&stale), None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::StaleVersion);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot edit \"{}\": file changed since it was read",
            dir.at("a.txt")
        )
    );

    let missing = fs
        .edit_text(
            &target(&fs, "missing.txt").await,
            &edit("a", "b", false),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code(), FsErrorCode::StaleVersion);

    let version = version_of(&fs, &a).await;
    std::fs::remove_file(dir.join("a.txt")).unwrap();
    let deleted = fs
        .edit_text(&a, &edit("goodbye", "bye", false), Some(&version), None)
        .await
        .unwrap_err();
    assert_eq!(deleted.code(), FsErrorCode::StaleVersion);

    let directory = fs
        .edit_text(
            &target(&fs, ".").await,
            &edit("a", "b", false),
            Some(&FsVersion::new("v")),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(directory.code(), FsErrorCode::NotRegularFile);
    assert_eq!(
        directory.to_string(),
        format!("cannot edit \"{}\": not a regular file", dir.str())
    );
}

// Literal-match codes, replaceAll, invalid UTF-8, an abort, and a
// follow-up edit on the returned version
#[tokio::test]
async fn an_edits_match_rules_and_follow_up() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "a a a");
    let a = target(&fs, "a.txt").await;
    let version = version_of(&fs, &a).await;
    let code = |result: Result<_, karaka_fs::FsError>| result.unwrap_err().code();
    assert_eq!(
        code(
            fs.edit_text(&a, &edit("z", "X", false), Some(&version), None)
                .await
        ),
        FsErrorCode::EditNotFound
    );
    assert_eq!(
        code(
            fs.edit_text(&a, &edit("absent", "X", false), None, None)
                .await
        ),
        FsErrorCode::EditNotFound
    );
    assert_eq!(
        code(
            fs.edit_text(&a, &edit("a", "X", false), Some(&version), None)
                .await
        ),
        FsErrorCode::AmbiguousEdit
    );
    let all = fs
        .edit_text(&a, &edit("a", "b", true), Some(&version), None)
        .await
        .unwrap();
    assert_eq!(all.after, "b b b");
    assert_eq!(read(dir.join("a.txt")), "b b b");

    let bytes = [0x68, 0xff, 0x69];
    write(dir.join("bad.txt"), bytes);
    let bad = target(&fs, "bad.txt").await;
    let version = version_of(&fs, &bad).await;
    assert_eq!(
        code(
            fs.edit_text(&bad, &edit("h", "H", false), Some(&version), None)
                .await
        ),
        FsErrorCode::NotText
    );
    assert_eq!(std::fs::read(dir.join("bad.txt")).unwrap(), bytes);

    write(dir.join("keep.txt"), "keep");
    let keep = target(&fs, "keep.txt").await;
    assert_eq!(
        code(
            fs.edit_text(
                &keep,
                &edit("keep", "x", false),
                None,
                Some(&AbortSignal::aborted())
            )
            .await
        ),
        FsErrorCode::Aborted
    );
    assert_eq!(read(dir.join("keep.txt")), "keep");

    write(dir.join("two.txt"), "one two");
    let two = target(&fs, "two.txt").await;
    let version = version_of(&fs, &two).await;
    let first = fs
        .edit_text(&two, &edit("one", "ONE", false), Some(&version), None)
        .await
        .unwrap();
    let second = fs
        .edit_text(&two, &edit("two", "TWO", false), Some(&first.version), None)
        .await
        .unwrap();
    assert_eq!(second.after, "ONE TWO");
    assert_eq!(read(dir.join("two.txt")), "ONE TWO");
}

// "two concurrent edits: one wins, the other is rejected as stale"
#[tokio::test]
async fn concurrent_edits_let_one_win() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("a.txt"), "base");
    let a = target(&fs, "a.txt").await;
    let version = version_of(&fs, &a).await;
    let (one, two) = (edit("base", "one", false), edit("base", "two", false));
    let (first, second) = tokio::join!(
        fs.edit_text(&a, &one, Some(&version), None),
        fs.edit_text(&a, &two, Some(&version), None),
    );
    let mut results = [first.map(|_| ()), second.map(|_| ())];
    results.sort_by_key(Result::is_err);
    assert!(results[0].is_ok());
    assert_eq!(
        results[1].as_ref().unwrap_err().code(),
        FsErrorCode::StaleVersion
    );
}

// symlink targetKey identity: one version, the real target written, and a
// stale change seen through both paths
#[tokio::test]
async fn a_symlink_and_its_file_are_one_target() {
    let dir = TempDir::new("fs");
    let fs = local(&dir);
    write(dir.join("real.txt"), "hello");
    symlink(dir.join("real.txt"), dir.join("link.txt"));
    let via_real = target(&fs, "real.txt").await;
    let via_link = target(&fs, "link.txt").await;
    assert_eq!(via_link.target_key, via_real.target_key);
    let version = version_of(&fs, &via_real).await;
    fs.edit_text(
        &via_link,
        &edit("hello", "bye", false),
        Some(&version),
        None,
    )
    .await
    .unwrap();
    assert_eq!(read(dir.join("real.txt")), "bye");
    assert!(dir.join("link.txt").is_symlink());

    let stale = version_of(&fs, &via_real).await;
    write(dir.join("real.txt"), "changed");
    let error = fs
        .edit_text(&via_link, &edit("bye", "x", false), Some(&stale), None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::StaleVersion);
}

// Not in dsh, whose writes can't be dropped: a write whose future is
// dropped mid-way leaves no staging behind.
#[tokio::test]
async fn a_dropped_write_leaves_no_staging() {
    let dir = TempDir::new("fs");
    let fs = local(&dir).with_internals(Internals {
        inspect_temp: Some(Arc::new(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50))
        })),
        ..Internals::default()
    });
    let a = target(&fs, "a.txt").await;
    let write = fs.write_text(&a, "never", None, None);
    let _ = tokio::time::timeout(std::time::Duration::from_millis(1), write).await;
    assert!(residue(dir.path()).is_empty(), "{:?}", residue(dir.path()));
}

// The plugin provides the port; a second backend is refused by the kernel,
// as dsh refuses a second `fs` service.
#[tokio::test]
async fn the_plugin_provides_the_port_once() {
    let dir = TempDir::new("fs");
    write(dir.join("a.txt"), "hi");
    let config = LocalFsConfig {
        cwd: dir.str(),
        ..LocalFsConfig::default()
    };
    let runtime = Runtime::start(vec![Box::new(LocalFsPlugin::new(config.clone()))])
        .await
        .unwrap();
    let fs = runtime.get::<dyn FileSystem>();
    let a = fs.resolve("a.txt", None, None).await.unwrap();
    assert_eq!(fs.read_text(&a, None).await.unwrap(), "hi");

    let twice = Runtime::start(vec![
        Box::new(LocalFsPlugin::new(config.clone())),
        Box::new(LocalFsPlugin::new(config)),
    ])
    .await;
    assert!(twice.is_err());

    let refused = Runtime::start(vec![Box::new(LocalFsPlugin::new(LocalFsConfig {
        diff_basis_max_bytes: 0,
        ..LocalFsConfig::default()
    }))])
    .await;
    assert!(refused.is_err());
}

//! The local backend's raw mechanics: resolution, probes, listing, text
//! reads and their refusals, the atomic write's staging, literal edits and
//! line endings. (dsh: `fs-local/tests/fsio.spec.ts`.)
//!
//! Not ported: the Windows cases, which Karaka doesn't target; and the
//! cases that replace `node:fs/promises` with a mock to stop a read between
//! its descriptor's stat and its chunks, which Rust can't do to `std::fs`.

mod common;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use futures_util::StreamExt;
use karaka_fs::{AbortController, AbortSignal, FsErrorCode, FsPathType, FsTargetKey, FsType};
use karaka_fs_local::fsio::{
    Internals, LineEndings, LocalTarget, apply_literal_edit, list_directory, probe,
    probe_no_follow, read_for_edit, read_text_for_diff, read_whole_text, resolve_local_target,
    restore_line_endings, stream_whole_text, write_file_atomic,
};

fn local(path: impl AsRef<Path>) -> LocalTarget {
    let path = path.as_ref().to_str().unwrap().to_owned();
    LocalTarget {
        target_key: FsTargetKey::new(path.clone()),
        display_path: path,
    }
}

async fn collect(target: LocalTarget, signal: Option<AbortSignal>) -> Result<String, FsErrorCode> {
    let mut stream = stream_whole_text(target, signal);
    let mut out = String::new();
    while let Some(chunk) = stream.next().await {
        out.push_str(&chunk.map_err(|error| error.code())?);
    }
    Ok(out)
}

fn source_kind(error: &karaka_fs::FsError) -> Option<io::ErrorKind> {
    std::error::Error::source(error)?
        .downcast_ref::<io::Error>()
        .map(io::Error::kind)
}

// resolveLocalTarget: "resolves a relative path from cwd and realpaths it"
#[tokio::test]
async fn a_relative_path_resolves_from_cwd_to_its_realpath() {
    let dir = TempDir::new("fsio");
    write(dir.join("a.txt"), "hi");
    let target = resolve_local_target(&dir.str(), "a.txt").await.unwrap();
    assert_eq!(target.display_path, dir.at("a.txt"));
    assert_eq!(target.target_key.as_str(), real(dir.join("a.txt")));
}

// "uses the realpathed parent + basename when the file does not exist"
#[tokio::test]
async fn a_missing_file_keys_on_its_real_parent() {
    let dir = TempDir::new("fsio");
    let target = resolve_local_target(&dir.str(), "missing.txt")
        .await
        .unwrap();
    assert_eq!(
        target.target_key.as_str(),
        format!("{}/missing.txt", dir.real())
    );
}

// "two paths to the same file via a symlink share one targetKey"
#[tokio::test]
async fn a_symlink_shares_its_files_key() {
    let dir = TempDir::new("fsio");
    write(dir.join("real.txt"), "hi");
    symlink(dir.join("real.txt"), dir.join("link.txt"));
    let via_real = resolve_local_target(&dir.str(), "real.txt").await.unwrap();
    let via_link = resolve_local_target(&dir.str(), "link.txt").await.unwrap();
    assert_eq!(via_link.target_key, via_real.target_key);
    assert_eq!(via_link.display_path, dir.at("link.txt"));
}

// "realpaths the nearest existing ancestor when intermediate dirs are
// missing"
#[tokio::test]
async fn missing_directories_key_on_the_nearest_real_ancestor() {
    let dir = TempDir::new("fsio");
    let target = resolve_local_target(&dir.str(), "no-such-dir/child.txt")
        .await
        .unwrap();
    assert_eq!(
        target.target_key.as_str(),
        format!("{}/no-such-dir/child.txt", dir.real())
    );
}

// "keeps the key stable across create when an ancestor is a symlink"
#[tokio::test]
async fn a_key_through_a_symlinked_ancestor_survives_creation() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir(dir.join("real-root")).unwrap();
    symlink(dir.join("real-root"), dir.join("link-root"));
    let before = resolve_local_target(&dir.at("link-root"), "sub/file.txt")
        .await
        .unwrap();
    std::fs::create_dir_all(dir.join("real-root/sub")).unwrap();
    write(dir.join("real-root/sub/file.txt"), "hi");
    let after = resolve_local_target(&dir.at("link-root"), "sub/file.txt")
        .await
        .unwrap();
    assert_eq!(before.target_key, after.target_key);
}

// "rejects a blank path"
#[tokio::test]
async fn a_blank_path_is_refused() {
    let dir = TempDir::new("fsio");
    let error = resolve_local_target(&dir.str(), "   ").await.unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotFound);
    assert_eq!(error.to_string(), "file_path must be a non-empty string");
}

// "anchors a relative cwd before retaining parent traversal in a display
// path"
#[tokio::test]
async fn a_relative_cwd_is_anchored_and_parent_traversal_kept() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir(dir.join("nested")).unwrap();
    let process_cwd = std::env::current_dir().unwrap();
    let cwd = relative(&process_cwd, &std::fs::canonicalize(dir.path()).unwrap());
    let target = resolve_local_target(&cwd, "nested/../created.txt")
        .await
        .unwrap();
    assert!(target.display_path.starts_with('/'));
    assert!(target.display_path.ends_with("nested/../created.txt"));
    assert_eq!(
        target.target_key.as_str(),
        format!("{}/created.txt", dir.real())
    );
    let again = resolve_local_target(&cwd, &target.display_path)
        .await
        .unwrap();
    assert_eq!(again.target_key, target.target_key);
}

/// `to` relative to `from`, both absolute, as Node's `relative` gives it.
fn relative(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|part| part.as_os_str().to_str().unwrap().to_owned()),
    );
    parts.join("/")
}

// "rejects parent traversal through a missing directory hierarchy"
#[tokio::test]
async fn parent_traversal_through_a_missing_directory_is_refused() {
    let dir = TempDir::new("fsio");
    let error = resolve_local_target(&dir.str(), "missing/deeper/../created.txt")
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotFound);
    assert!(
        error
            .to_string()
            .ends_with("parent traversal crosses a missing directory"),
        "{error}"
    );
}

// "resolves parent traversal after a symlink in the provider filesystem"
#[tokio::test]
async fn parent_traversal_after_a_symlink_is_physical() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir_all(dir.join("physical/nested")).unwrap();
    std::fs::create_dir(dir.join("lexical")).unwrap();
    symlink(dir.join("physical/nested"), dir.join("lexical/link"));
    let cwd = format!("{}/..", dir.at("lexical/link"));
    let before = resolve_local_target(&cwd, "created.txt").await.unwrap();
    assert_eq!(
        before.target_key.as_str(),
        format!("{}/created.txt", real(dir.join("physical")))
    );
    write(dir.join("physical/created.txt"), "physical");
    let after = resolve_local_target(&dir.str(), "lexical/link/../created.txt")
        .await
        .unwrap();
    assert_eq!(after.target_key, before.target_key);
    let error = resolve_local_target(&dir.str(), "missing/../created.txt")
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotFound);
}

// "rejects a path whose ancestor is a file with a structured FsError
// (ENOTDIR)"
#[tokio::test]
async fn a_path_under_a_file_is_not_found() {
    let dir = TempDir::new("fsio");
    write(dir.join("afile"), "i am a file");
    let error = resolve_local_target(&dir.str(), "afile/child.txt")
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotFound);
    assert_eq!(
        error.to_string(),
        format!(
            "cannot resolve \"{}\": a parent path segment is not a directory",
            dir.at("afile/child.txt")
        )
    );
}

// probe: "returns null for a missing path and metadata for a file", "reports
// a directory", and the ENOTDIR case
#[tokio::test]
async fn a_probe_reads_metadata_or_finds_nothing() {
    let dir = TempDir::new("fsio");
    assert_eq!(probe(&dir.at("nope")).await.unwrap(), None);
    write(dir.join("a.txt"), "hi");
    let info = probe(&dir.at("a.txt")).await.unwrap().unwrap();
    assert_eq!((info.kind, info.size), (FsType::File, 2));
    assert_eq!(info.version.as_str().split(':').count(), 5);
    std::fs::create_dir(dir.join("sub")).unwrap();
    assert_eq!(
        probe(&dir.at("sub")).await.unwrap().map(|info| info.kind),
        Some(FsType::Directory)
    );
    assert_eq!(probe(&dir.at("a.txt/child.txt")).await.unwrap(), None);
}

// "reports a socket/special file as type "other""
#[tokio::test]
async fn a_socket_is_other() {
    let dir = TempDir::new("fsio");
    let Ok(_listener) = std::os::unix::net::UnixListener::bind(dir.join("sock")) else {
        // A sandbox may forbid sockets; that is the environment, not us.
        return;
    };
    assert_eq!(
        probe(&dir.at("sock")).await.unwrap().map(|info| info.kind),
        Some(FsType::Other)
    );
}

// probeNoFollow: "reports symlinks without following them", and missing
#[tokio::test]
async fn a_no_follow_probe_sees_the_link() {
    let dir = TempDir::new("fsio");
    write(dir.join("real.txt"), "hi");
    symlink(dir.join("real.txt"), dir.join("link.txt"));
    assert_eq!(
        probe_no_follow(&dir.at("real.txt"))
            .await
            .unwrap()
            .map(|info| info.kind),
        Some(FsPathType::File)
    );
    let link = probe_no_follow(&dir.at("link.txt")).await.unwrap().unwrap();
    assert_eq!(link.kind, FsPathType::Symlink);
    assert!(link.size > 0);
    assert_eq!(probe_no_follow(&dir.at("missing")).await.unwrap(), None);
    write(dir.join("afile"), "i am a file");
    assert_eq!(
        probe_no_follow(&dir.at("afile/child.txt")).await.unwrap(),
        None
    );
}

// listDirectory: "preserves physical parent traversal in displayed child
// paths"
#[tokio::test]
async fn listed_children_keep_physical_parent_traversal() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir(dir.join("lexical")).unwrap();
    std::fs::create_dir_all(dir.join("physical/nested")).unwrap();
    symlink(dir.join("physical/nested"), dir.join("lexical/link"));
    write(dir.join("lexical/same.txt"), "lexical file");
    write(dir.join("physical/same.txt"), "physical file");
    let parent = resolve_local_target(&dir.str(), "lexical/link/..")
        .await
        .unwrap();
    let entries = list_directory(&parent, None).await.unwrap();
    let child = entries
        .iter()
        .find(|entry| entry.name == "same.txt")
        .unwrap();
    assert_eq!(read(&child.target.display_path), "physical file");
    let again = resolve_local_target(&dir.str(), &child.target.display_path)
        .await
        .unwrap();
    assert_eq!(again.target_key, child.target.target_key);
}

// "lists direct children in stable order without reading content"
#[tokio::test]
async fn a_listing_is_in_name_order_with_cheap_metadata() {
    let dir = TempDir::new("fsio");
    let root = dir.join("skills");
    std::fs::create_dir_all(root.join("dir-skill")).unwrap();
    write(root.join("zeta.md"), "zeta");
    write(root.join("alpha.md"), "alpha");
    symlink(root.join("missing-target"), root.join("broken-link"));
    let entries = list_directory(&local(&root), None).await.unwrap();
    let kinds: Vec<(&str, FsType)> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("alpha.md", FsType::File),
            ("broken-link", FsType::Other),
            ("dir-skill", FsType::Directory),
            ("zeta.md", FsType::File),
        ]
    );
    let alpha = &entries[0];
    assert_eq!(alpha.size, Some(5));
    assert!(alpha.version.is_some());
    assert_eq!(entries[1].version, None);
    assert_eq!(entries[2].size, None);
}

// "derives child target keys from the listed parent identity"
#[tokio::test]
async fn child_keys_come_from_the_listed_parents_identity() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir(dir.join("real-one")).unwrap();
    std::fs::create_dir(dir.join("real-two")).unwrap();
    write(dir.join("real-one/same.txt"), "one");
    write(dir.join("real-two/same.txt"), "different two");
    symlink(dir.join("real-one"), dir.join("link"));
    let target = resolve_local_target(&dir.str(), "link").await.unwrap();
    std::fs::remove_file(dir.join("link")).unwrap();
    symlink(dir.join("real-two"), dir.join("link"));
    let entries = list_directory(&target, None).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "same.txt");
    assert_eq!(entries[0].target.display_path, dir.at("link/same.txt"));
    assert_eq!(
        entries[0].target.target_key.as_str(),
        real(dir.join("real-one/same.txt"))
    );
    assert_eq!(entries[0].size, Some(3));
}

// "rejects missing, non-directory, and aborted listing requests"
#[tokio::test]
async fn a_listing_refuses_missing_files_and_aborts() {
    let dir = TempDir::new("fsio");
    let missing = list_directory(&local(dir.join("missing")), None)
        .await
        .unwrap_err();
    assert_eq!(missing.code(), FsErrorCode::NotFound);
    assert_eq!(
        missing.to_string(),
        format!("cannot list \"{}\": not found", dir.at("missing"))
    );
    write(dir.join("a.txt"), "hi");
    let file = list_directory(&local(dir.join("a.txt")), None)
        .await
        .unwrap_err();
    assert_eq!(file.code(), FsErrorCode::NotDirectory);
    let aborted = list_directory(&local(dir.path()), Some(&AbortSignal::aborted()))
        .await
        .unwrap_err();
    assert_eq!(aborted.code(), FsErrorCode::Aborted);
    assert_eq!(aborted.to_string(), "list aborted");
}

// "translates directory permission failures into FS_PERMISSION_DENIED"
#[tokio::test]
async fn an_unreadable_directory_is_permission_denied() {
    let dir = TempDir::new("fsio");
    let root = dir.join("restricted");
    std::fs::create_dir(&root).unwrap();
    chmod(&root, 0o000);
    let result = list_directory(&local(&root), None).await;
    chmod(&root, 0o700);
    // Root may still list a mode-000 directory.
    if let Err(error) = result {
        assert_eq!(error.code(), FsErrorCode::PermissionDenied);
    }
}

// "translates preflight metadata IO failures into FS_IO_ERROR", and "child
// resolution failures"
#[tokio::test]
async fn a_symlink_loop_is_an_io_error() {
    let dir = TempDir::new("fsio");
    symlink(dir.join("loop"), dir.join("loop"));
    let error = list_directory(&local(dir.join("loop")), None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::IoError);

    std::fs::create_dir(dir.join("listed")).unwrap();
    symlink(dir.join("listed/loop"), dir.join("listed/loop"));
    let error = list_directory(&local(dir.join("listed")), None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::IoError);
}

// "translates child permission failures into FS_PERMISSION_DENIED"
#[tokio::test]
async fn a_child_behind_an_unreadable_directory_is_permission_denied() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir(dir.join("listed")).unwrap();
    std::fs::create_dir_all(dir.join("protected/secret")).unwrap();
    symlink(dir.join("protected/secret"), dir.join("listed/secret-link"));
    chmod(dir.join("protected"), 0o000);
    let result = list_directory(&local(dir.join("listed")), None).await;
    chmod(dir.join("protected"), 0o700);
    if let Err(error) = result {
        assert_eq!(error.code(), FsErrorCode::PermissionDenied);
    }
}

// readWholeText: small file, missing and directory, binary and invalid
// UTF-8, aborts and a live signal
#[tokio::test]
async fn a_whole_read_gives_text_or_refuses() {
    let dir = TempDir::new("fsio");
    write(dir.join("a.txt"), "one\ntwo\nthree");
    assert_eq!(
        read_whole_text(&local(dir.join("a.txt")), None)
            .await
            .unwrap(),
        "one\ntwo\nthree"
    );
    let controller = AbortController::new();
    assert_eq!(
        read_whole_text(&local(dir.join("a.txt")), Some(&controller.signal()))
            .await
            .unwrap(),
        "one\ntwo\nthree"
    );
    let missing = read_whole_text(&local(dir.join("nope")), None)
        .await
        .unwrap_err();
    assert_eq!(
        missing.to_string(),
        format!("cannot read \"{}\": not found", dir.at("nope"))
    );
    let directory = read_whole_text(&local(dir.path()), None).await.unwrap_err();
    assert_eq!(directory.code(), FsErrorCode::NotRegularFile);

    write(dir.join("bin"), [0x68, 0x00, 0x69]);
    let binary = read_whole_text(&local(dir.join("bin")), None)
        .await
        .unwrap_err();
    assert_eq!(binary.code(), FsErrorCode::NotText);
    assert_eq!(
        binary.to_string(),
        format!("cannot read \"{}\": binary file", dir.at("bin"))
    );
    write(dir.join("bad"), [0x68, 0xff, 0x69]);
    let invalid = read_whole_text(&local(dir.join("bad")), None)
        .await
        .unwrap_err();
    assert_eq!(
        invalid.to_string(),
        format!("cannot read \"{}\": invalid UTF-8 text", dir.at("bad"))
    );

    // dsh's "mid-read" abort lands before the read starts, as here.
    let controller = AbortController::new();
    let (target, signal) = (local(dir.join("a.txt")), controller.signal());
    let pending = read_whole_text(&target, Some(&signal));
    controller.abort();
    let aborted = pending.await.unwrap_err();
    assert_eq!(
        (aborted.code(), aborted.to_string()),
        (FsErrorCode::Aborted, "read aborted".into())
    );
}

// Node's fatal `TextDecoder` drops a leading byte-order mark, so dsh's
// reads do.
#[tokio::test]
async fn a_leading_byte_order_mark_is_dropped_as_dshs_decoder_does() {
    let dir = TempDir::new("fsio");
    write(dir.join("bom.txt"), b"\xef\xbb\xbfhello");
    assert_eq!(
        read_whole_text(&local(dir.join("bom.txt")), None)
            .await
            .unwrap(),
        "hello"
    );
    assert_eq!(
        collect(local(dir.join("bom.txt")), None).await.unwrap(),
        "hello"
    );
    assert_eq!(
        read_for_edit(&dir.at("bom.txt"), &dir.at("bom.txt"), None)
            .await
            .unwrap()
            .content,
        "hello"
    );
}

// readTextForDiff: the limit, the opened file's own size, deletion, binary
// and invalid UTF-8, and a pre-aborted signal
#[tokio::test]
async fn a_diff_basis_is_best_effort_and_bounded() {
    let dir = TempDir::new("fsio");
    write(dir.join("basis.txt"), "a\r\nb");
    assert_eq!(
        read_text_for_diff(&dir.at("basis.txt"), 5, None)
            .await
            .unwrap()
            .as_deref(),
        Some("a\nb")
    );
    assert_eq!(
        read_text_for_diff(&dir.at("basis.txt"), 4, None)
            .await
            .unwrap(),
        None
    );
    write(dir.join("replaced.txt"), "123456789");
    assert_eq!(
        read_text_for_diff(&dir.at("replaced.txt"), 8, None)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        read_text_for_diff(&dir.at("deleted.txt"), 32, None)
            .await
            .unwrap(),
        None
    );
    write(dir.join("bin"), [0x68, 0x00, 0x69]);
    write(dir.join("bad"), [0x68, 0xff, 0x69]);
    assert_eq!(
        read_text_for_diff(&dir.at("bin"), 8, None).await.unwrap(),
        None
    );
    assert_eq!(
        read_text_for_diff(&dir.at("bad"), 8, None).await.unwrap(),
        None
    );
    let aborted = read_text_for_diff(&dir.at("basis.txt"), 8, Some(&AbortSignal::aborted()))
        .await
        .unwrap_err();
    assert_eq!(aborted.code(), FsErrorCode::Aborted);
}

// streamWholeText: whole and multi-chunk files, its refusals, and aborts
#[tokio::test]
async fn a_streamed_read_gives_the_same_text_and_refusals() {
    let dir = TempDir::new("fsio");
    write(dir.join("a.txt"), "one\ntwo\nthree");
    assert_eq!(
        collect(local(dir.join("a.txt")), None).await.unwrap(),
        "one\ntwo\nthree"
    );
    let content: Vec<String> = (0..50)
        .map(|line| format!("line {line}: {}", "x".repeat(3000)))
        .collect();
    let content = content.join("\n");
    write(dir.join("big.txt"), &content);
    assert_eq!(
        collect(local(dir.join("big.txt")), None).await.unwrap(),
        content
    );
    // A multi-byte character split across chunks still decodes.
    let split = format!("{}é{}", "x".repeat(64 * 1024 - 1), "y".repeat(10));
    write(dir.join("split.txt"), &split);
    assert_eq!(
        collect(local(dir.join("split.txt")), None).await.unwrap(),
        split
    );

    assert_eq!(
        collect(local(dir.join("nope")), None).await,
        Err(FsErrorCode::NotFound)
    );
    assert_eq!(
        collect(local(dir.path()), None).await,
        Err(FsErrorCode::NotRegularFile)
    );
    write(dir.join("bin"), [0x68, 0x00, 0x69]);
    assert_eq!(
        collect(local(dir.join("bin")), None).await,
        Err(FsErrorCode::NotText)
    );
    write(dir.join("bad"), [0x68, 0xff, 0x69]);
    assert_eq!(
        collect(local(dir.join("bad")), None).await,
        Err(FsErrorCode::NotText)
    );
    assert_eq!(
        collect(local(dir.join("a.txt")), Some(AbortSignal::aborted())).await,
        Err(FsErrorCode::Aborted)
    );
    let controller = AbortController::new();
    assert_eq!(
        collect(local(dir.join("a.txt")), Some(controller.signal())).await,
        Ok("one\ntwo\nthree".into())
    );
}

// "translates a mid-stream abort into FS_ABORTED"
#[tokio::test]
async fn an_abort_mid_stream_ends_it() {
    let dir = TempDir::new("fsio");
    write(dir.join("big.txt"), "x".repeat(256 * 1024));
    let controller = AbortController::new();
    let mut stream = stream_whole_text(local(dir.join("big.txt")), Some(controller.signal()));
    let first = stream.next().await.unwrap();
    assert!(first.is_ok());
    controller.abort();
    let next = stream.next().await.unwrap().unwrap_err();
    assert_eq!(next.code(), FsErrorCode::Aborted);
}

// writeFileAtomic: "writes through a private staging dir and owner-only
// temp file"
#[tokio::test]
async fn a_write_is_staged_privately_and_keeps_the_mode() {
    let dir = TempDir::new("fsio");
    write(dir.join("a.txt"), "old");
    chmod(dir.join("a.txt"), 0o640);
    let inspected = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&inspected);
    let internals = Internals {
        inspect_temp: Some(Arc::new(move |paths| {
            seen.store(true, Ordering::SeqCst);
            assert!(paths.staging_dir.is_dir());
            assert!(paths.temp_path.is_file());
            assert_eq!(mode(&paths.staging_dir), 0o700);
            assert_eq!(mode(&paths.temp_path), 0o600);
            assert_eq!(std::fs::read_to_string(&paths.temp_path).unwrap(), "hello");
        })),
        ..Internals::default()
    };
    write_file_atomic(
        &dir.at("a.txt"),
        "hello",
        Some(0o640),
        None,
        &internals,
        None,
    )
    .await
    .unwrap();
    assert!(inspected.load(Ordering::SeqCst));
    assert_eq!(read(dir.join("a.txt")), "hello");
    assert_eq!(mode(dir.join("a.txt")), 0o640);
    assert!(residue(dir.path()).is_empty());
}

// "maps a non-collision guarded-create publication failure and cleans
// staging"
#[tokio::test]
async fn a_failed_guarded_publication_is_an_io_error() {
    let dir = TempDir::new("fsio");
    let file = dir.at("a.txt");
    let internals = Internals {
        link_file: Some(Arc::new(|_, _| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })),
        ..Internals::default()
    };
    let error = write_file_atomic(&file, "ours", None, None, &internals, Some(&file))
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::IoError);
    assert_eq!(source_kind(&error), Some(io::ErrorKind::PermissionDenied));
    assert!(!dir.join("a.txt").exists());
    assert!(residue(dir.path()).is_empty());
}

// "maps a guarded-create target-inspection failure and cleans staging"
#[tokio::test]
async fn a_failed_inspection_after_a_guarded_publication_is_an_io_error() {
    let dir = TempDir::new("fsio");
    let file = dir.at("a.txt");
    let internals = Internals {
        link_file: Some(Arc::new(|_, _| Err(io::Error::other("link failed")))),
        inspect_publication_target: Some(Arc::new(|_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })),
        ..Internals::default()
    };
    let error = write_file_atomic(&file, "ours", None, None, &internals, Some(&file))
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::IoError);
    assert_eq!(source_kind(&error), Some(io::ErrorKind::PermissionDenied));
    assert!(!dir.join("a.txt").exists());
    assert!(residue(dir.path()).is_empty());
}

// "rejects a guarded-create collision that vanishes before inspection"
#[tokio::test]
async fn a_collision_that_vanished_is_still_not_observed() {
    let dir = TempDir::new("fsio");
    let file = dir.at("a.txt");
    let internals = Internals {
        link_file: Some(Arc::new(|_, _| {
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        })),
        ..Internals::default()
    };
    let error = write_file_atomic(&file, "ours", None, None, &internals, Some(&file))
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotObserved);
    assert_eq!(
        error.to_string(),
        format!("cannot overwrite existing \"{file}\" without reading it first")
    );
    assert_eq!(source_kind(&error), Some(io::ErrorKind::AlreadyExists));
    assert!(residue(dir.path()).is_empty());
}

// "uses the display path and target type when guarded publication finds a
// competitor"
#[tokio::test]
async fn a_competitor_is_named_by_the_display_path_and_its_type() {
    let dir = TempDir::new("fsio");
    let file = dir.at("a.txt");
    let display = dir.at("linked-workspace/a.txt");
    let competitor = dir.join("a.txt");
    let internals = Internals {
        inspect_temp: Some(Arc::new(move |_| write(&competitor, "competitor"))),
        ..Internals::default()
    };
    let error = write_file_atomic(&file, "ours", None, None, &internals, Some(&display))
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotObserved);
    assert_eq!(
        error.to_string(),
        format!("cannot overwrite existing \"{display}\" without reading it first")
    );
    assert_eq!(read(dir.join("a.txt")), "competitor");

    std::fs::remove_file(dir.join("a.txt")).unwrap();
    let directory = dir.join("a.txt");
    let internals = Internals {
        inspect_temp: Some(Arc::new(move |_| std::fs::create_dir(&directory).unwrap())),
        ..Internals::default()
    };
    let error = write_file_atomic(&file, "ours", None, None, &internals, Some(&display))
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::NotRegularFile);
    assert_eq!(
        error.to_string(),
        format!("cannot write \"{display}\": not a regular file")
    );
    assert!(dir.join("a.txt").is_dir());
}

// "does not turn post-commit staging cleanup failure into a failed guarded
// write"
#[tokio::test]
async fn a_failed_cleanup_after_publication_doesnt_fail_the_write() {
    let dir = TempDir::new("fsio");
    let file = dir.at("a.txt");
    let internals = Internals {
        remove_staging_dir: Some(Arc::new(|_| Err(io::Error::other("cleanup failed")))),
        ..Internals::default()
    };
    write_file_atomic(&file, "ours", None, None, &internals, Some(&file))
        .await
        .unwrap();
    assert_eq!(read(dir.join("a.txt")), "ours");
}

// "creates new files owner-only by default", "creates parent directories
// as needed", "passes a live (non-aborted) signal through the write"
#[tokio::test]
async fn a_new_file_is_owner_only_and_its_parents_are_made() {
    let dir = TempDir::new("fsio");
    let controller = AbortController::new();
    write_file_atomic(
        &dir.at("nested/deep/a.txt"),
        "hi",
        None,
        Some(&controller.signal()),
        &Internals::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(read(dir.join("nested/deep/a.txt")), "hi");
    assert_eq!(mode(dir.join("nested/deep/a.txt")), 0o600);
}

// "opens staging paths exclusively — a pre-existing path is never
// clobbered"
#[tokio::test]
async fn a_staging_directory_is_never_reused() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir(dir.join(".fixed-temp.tmpdir")).unwrap();
    write(dir.join(".fixed-temp.tmpdir/PRECIOUS"), "keep");
    let internals = Internals {
        temp_dir_name: Some(Arc::new(|_| ".fixed-temp.tmpdir".into())),
        ..Internals::default()
    };
    let error = write_file_atomic(&dir.at("a.txt"), "hello", None, None, &internals, None)
        .await
        .unwrap_err();
    assert_eq!(source_kind(&error), Some(io::ErrorKind::AlreadyExists));
    assert_eq!(read(dir.join(".fixed-temp.tmpdir/PRECIOUS")), "keep");
    assert!(!dir.join("a.txt").exists());
}

// "aborts before writing when the signal is already aborted"
#[tokio::test]
async fn an_aborted_write_writes_nothing() {
    let dir = TempDir::new("fsio");
    let error = write_file_atomic(
        &dir.at("a.txt"),
        "hi",
        None,
        Some(&AbortSignal::aborted()),
        &Internals::default(),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        (error.code(), error.to_string()),
        (FsErrorCode::Aborted, "write aborted".into())
    );
    assert!(!dir.join("a.txt").exists());
}

// An abort while the temp file is staged stops before publication, and the
// staging is cleaned up. (dsh checks the signal before publishing.)
#[tokio::test]
async fn an_abort_while_staging_publishes_nothing() {
    let dir = TempDir::new("fsio");
    let controller = Arc::new(AbortController::new());
    let aborting = Arc::clone(&controller);
    let internals = Internals {
        inspect_temp: Some(Arc::new(move |_| {
            aborting.abort();
        })),
        ..Internals::default()
    };
    let error = write_file_atomic(
        &dir.at("a.txt"),
        "hi",
        None,
        Some(&controller.signal()),
        &internals,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::Aborted);
    assert!(!dir.join("a.txt").exists());
    assert!(residue(dir.path()).is_empty());
}

// "cleans up the temp file when the final rename fails"
#[tokio::test]
async fn a_failed_rename_leaves_no_staging() {
    let dir = TempDir::new("fsio");
    std::fs::create_dir(dir.join("occupied")).unwrap();
    std::fs::create_dir(dir.join("occupied/inside")).unwrap();
    assert!(
        write_file_atomic(
            &dir.at("occupied"),
            "hi",
            None,
            None,
            &Internals::default(),
            None
        )
        .await
        .is_err()
    );
    assert!(residue(dir.path()).is_empty());
}

// applyLiteralEdit: a unique match, none, an empty search, several, all,
// and line endings
#[test]
fn a_literal_edit_needs_one_match_unless_it_replaces_all() {
    let edit = apply_literal_edit("a b c", "b", "X", false, "f").unwrap();
    assert_eq!((edit.content.as_str(), edit.replacements), ("a X c", 1));
    let none = apply_literal_edit("a b c", "z", "X", false, "f").unwrap_err();
    assert_eq!(none.code(), FsErrorCode::EditNotFound);
    assert_eq!(none.to_string(), "old_string was not found in \"f\"");
    let empty = apply_literal_edit("a b c", "", "X", false, "f").unwrap_err();
    assert_eq!(
        (empty.code(), empty.to_string()),
        (
            FsErrorCode::EditNotFound,
            "old_string must be a non-empty string".into()
        )
    );
    let ambiguous = apply_literal_edit("a a a", "a", "X", false, "f").unwrap_err();
    assert_eq!(ambiguous.code(), FsErrorCode::AmbiguousEdit);
    assert_eq!(
        ambiguous.to_string(),
        "old_string matched 3 times in \"f\"; provide a more specific old_string or set \
         replace_all to true"
    );
    let all = apply_literal_edit("a a a", "a", "X", true, "f").unwrap();
    assert_eq!((all.content.as_str(), all.replacements), ("X X X", 3));
    assert_eq!(
        apply_literal_edit("one\ntwo", "one\r\ntwo", "x", false, "f")
            .unwrap()
            .replacements,
        1
    );
}

// readForEdit + restoreLineEndings
#[tokio::test]
async fn an_edit_matches_on_lf_and_writes_back_crlf() {
    let dir = TempDir::new("fsio");
    write(dir.join("crlf.txt"), "one\r\ntwo\r\n");
    let original = read_for_edit(&dir.at("crlf.txt"), &dir.at("crlf.txt"), None)
        .await
        .unwrap();
    assert_eq!(original.line_endings, LineEndings::Crlf);
    let edited = apply_literal_edit(&original.content, "two", "TWO", false, "f").unwrap();
    assert_eq!(
        restore_line_endings(&edited.content, original.line_endings),
        "one\r\nTWO\r\n"
    );

    write(dir.join("bin"), [0x00, 0x01]);
    let binary = read_for_edit(&dir.at("bin"), &dir.at("bin"), None)
        .await
        .unwrap_err();
    assert_eq!(
        binary.to_string(),
        format!("cannot edit \"{}\": binary file", dir.at("bin"))
    );
    write(dir.join("bad"), [0x68, 0xff, 0x69]);
    let invalid = read_for_edit(&dir.at("bad"), &dir.at("bad"), None)
        .await
        .unwrap_err();
    assert_eq!(invalid.code(), FsErrorCode::NotText);

    write(dir.join("a.txt"), "one\ntwo");
    let controller = AbortController::new();
    assert_eq!(
        read_for_edit(&dir.at("a.txt"), "a", Some(&controller.signal()))
            .await
            .unwrap()
            .content,
        "one\ntwo"
    );
    let (path, signal) = (dir.at("a.txt"), controller.signal());
    let pending = read_for_edit(&path, "a", Some(&signal));
    controller.abort();
    assert_eq!(pending.await.unwrap_err().to_string(), "edit aborted");
}

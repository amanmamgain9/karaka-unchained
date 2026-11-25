//! Watching a file or a directory's direct entries. (dsh:
//! `fs-local/tests/watch.spec.ts`.)
//!
//! dsh's cases drive a mocked chokidar watcher by hand; their filtering is
//! ported the same way, with synthetic events, as unit tests in `local.rs`.
//! These cases watch the real filesystem: each makes real changes in a
//! temporary directory and waits, with a deadline, for what it should hear.
//! A system with no inotify instances left can't watch at all; there they
//! say so and skip, as dsh's socket case skips where sockets are refused.
//! dsh's "metadata lookup cancelled mid-way" case needs a mocked `stat`;
//! the abort checks around it are covered by the already-aborted case.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use karaka_fs::{AbortController, AbortSignal, FileSystem, FsErrorCode, FsTarget};
use karaka_fs_local::{LocalFileSystem, LocalFsConfig};
use tokio::sync::mpsc;

/// What a watch heard: `None` for a change, the error's code for a failure.
type Heard = mpsc::UnboundedReceiver<Option<FsErrorCode>>;

/// A watch of `target`, or `None` where the system has no inotify
/// instances left to watch with.
async fn watch(fs: &LocalFileSystem, target: &FsTarget) -> Option<(karaka_fs::FsWatch, Heard)> {
    let (sender, heard) = mpsc::unbounded_channel();
    let controller = AbortController::new();
    let started = fs
        .watch(
            target,
            Arc::new(move |error| {
                let _ = sender.send(error.map(|error| error.code()));
            }),
            &controller.signal(),
        )
        .await;
    match started {
        Ok(watch) => Some((watch, heard)),
        Err(error) if exhausted(&error) => {
            eprintln!("skipped: {error}");
            None
        }
        Err(error) => panic!("the watch didn't start: {error}"),
    }
}

/// The system's inotify instances are all taken (`EMFILE`).
fn exhausted(error: &karaka_fs::FsError) -> bool {
    error.to_string().contains("Too many open files")
}

async fn hears(heard: &mut Heard) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), heard.recv()).await,
        Ok(Some(None))
    )
}

async fn hears_nothing(heard: &mut Heard) -> bool {
    tokio::time::sleep(Duration::from_millis(300)).await;
    heard.try_recv().is_err()
}

fn setup() -> (TempDir, LocalFileSystem) {
    let dir = TempDir::new("watch");
    let fs = LocalFileSystem::new(LocalFsConfig {
        cwd: dir.str(),
        ..LocalFsConfig::default()
    })
    .unwrap();
    (dir, fs)
}

// "waits for the parent watch of a file target and awaits closure"
#[tokio::test]
async fn a_file_is_watched_through_its_parent_and_closes() {
    let (dir, fs) = setup();
    write(dir.join("file.txt"), "one");
    let target = fs.resolve("file.txt", None, None).await.unwrap();
    let Some((watch, mut heard)) = watch(&fs, &target).await else {
        return;
    };
    std::fs::create_dir(dir.join("sibling-dir")).unwrap();
    write(dir.join("unrelated.txt"), "x");
    assert!(hears_nothing(&mut heard).await, "a sibling isn't heard");
    write(dir.join("file.txt"), "two");
    assert!(hears(&mut heard).await);
    watch.close().await;
    // Drain what the write itself raised, then nothing more.
    while heard.try_recv().is_ok() {}
    write(dir.join("file.txt"), "three");
    assert!(hears_nothing(&mut heard).await, "a closed watch is silent");
}

// "waits for the parent watch of a missing target"
#[tokio::test]
async fn a_missing_file_is_heard_when_it_appears() {
    let (dir, fs) = setup();
    let target = fs.resolve("later.txt", None, None).await.unwrap();
    let Some((watch, mut heard)) = watch(&fs, &target).await else {
        return;
    };
    write(dir.join("other.txt"), "x");
    assert!(hears_nothing(&mut heard).await);
    write(dir.join("later.txt"), "now");
    assert!(hears(&mut heard).await);
    watch.close().await;
}

// "watches a directory itself without filtering its direct entries"
#[tokio::test]
async fn a_directorys_entries_are_all_heard() {
    let (dir, fs) = setup();
    std::fs::create_dir(dir.join("dir")).unwrap();
    let target = fs.resolve("dir", None, None).await.unwrap();
    let Some((watch, mut heard)) = watch(&fs, &target).await else {
        return;
    };
    write(dir.join("dir/child.txt"), "x");
    assert!(hears(&mut heard).await);
    watch.close().await;
}

// "rejects an already-cancelled initialization without acquiring a
// watcher"
#[tokio::test]
async fn an_aborted_start_watches_nothing() {
    let (dir, fs) = setup();
    write(dir.join("file.txt"), "one");
    let target = fs.resolve("file.txt", None, None).await.unwrap();
    let error = fs
        .watch(
            &target,
            Arc::new(|_| panic!("nothing is heard")),
            &AbortSignal::aborted(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.code(), error.to_string()),
        (FsErrorCode::Aborted, "watch aborted".into())
    );
    write(dir.join("file.txt"), "two");
    tokio::time::sleep(Duration::from_millis(200)).await;
}

// "reports initialization errors and closes before rejecting": a parent
// that doesn't exist can't be watched, and the failure is heard too.
#[tokio::test]
async fn a_watch_that_cant_start_is_heard_and_refused() {
    let (_dir, fs) = setup();
    let target = fs
        .resolve("missing-dir/file.txt", None, None)
        .await
        .unwrap();
    let (sender, mut heard) = mpsc::unbounded_channel();
    let error = fs
        .watch(
            &target,
            Arc::new(move |error| {
                let _ = sender.send(error.map(|error| error.code()));
            }),
            &AbortController::new().signal(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), FsErrorCode::IoError);
    assert!(error.to_string().starts_with("cannot watch \""), "{error}");
    assert_eq!(heard.try_recv(), Ok(Some(FsErrorCode::IoError)));
}

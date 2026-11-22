//! The write lock: one writer across backend instances and processes.
//! (dsh: `lease.spec.ts` and `lease.two-process.e2e.ts`.)
//!
//! Two backends over one root in this process stand for two processes: a
//! `flock` is held per open file, so the second instance's lock attempt
//! meets the first's just as another process would. One test also runs a
//! real second process, this test binary started again as the holder.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use common::*;
use karaka_session_jsonl::LOCK_FILENAME;
use karaka_session_persistence::{
    CreateOptions, PersistenceError, SessionAccess, SessionPersistence,
};

/// Where the child process's session lives.
const HOLD_ROOT: &str = "KARAKA_JSONL_TEST_HOLD_ROOT";

fn lock(root: &TempRoot, name: &str) -> std::path::PathBuf {
    root.log(None, name).with_file_name(LOCK_FILENAME)
}

#[tokio::test]
async fn a_second_instance_is_excluded_while_the_holder_is_live() {
    let root = TempRoot::new("excluded");
    let first = root.backend();
    let second = root.backend();
    let holder = first
        .create(meta("excluded", None), CreateOptions::default())
        .await
        .unwrap();
    holder.append(second_turn(0)).await.unwrap();

    // The stored duplicate is an existence fact; the write open, an
    // ownership one.
    let refusal = second
        .create(meta("excluded", None), CreateOptions::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyExists(_)));
    let refusal = second
        .open(&id("excluded"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyOwned(_)));

    // Unwritten creates hold no lock and leave no file, so a rival's
    // create succeeds; the collision shows at the loser's first write,
    // where the winner already holds the lock.
    let winner = first
        .create(meta("excluded-pending", None), CreateOptions::default())
        .await
        .unwrap();
    let loser = second
        .create(meta("excluded-pending", None), CreateOptions::default())
        .await
        .unwrap();
    winner.append(second_turn(0)).await.unwrap();
    let refusal = loser.append(second_turn(0)).await.unwrap_err();
    assert!(matches!(refusal, PersistenceError::AlreadyOwned(_)));
    loser.close().await.unwrap();
    winner.close().await.unwrap();

    // Reads never touch the lock.
    assert_eq!(seqs(&read_all(&second, "excluded").await.unwrap()), [0, 1]);

    holder.close().await.unwrap();
    // The lock file stays; the kernel lock is gone.
    assert!(lock(&root, "excluded").is_file());
    let reopened = second
        .open(&id("excluded"), SessionAccess::Write)
        .await
        .unwrap();
    reopened.append(second_turn(2)).await.unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn removing_the_lock_file_forfeits_a_stuck_holder() {
    let root = TempRoot::new("wedged");
    let first = root.backend();
    let second = root.backend();
    let wedged = first
        .create(meta("wedged", None), CreateOptions::default())
        .await
        .unwrap();
    wedged.append(second_turn(0)).await.unwrap();
    // The escape hatch for a live but stuck holder: a successor locks the
    // fresh inode.
    std::fs::remove_file(lock(&root, "wedged")).unwrap();
    let successor = second
        .open(&id("wedged"), SessionAccess::Write)
        .await
        .unwrap();
    successor.append(second_turn(2)).await.unwrap();
    successor.close().await.unwrap();
    wedged.close().await.unwrap();
}

#[tokio::test]
async fn an_absent_or_unwritten_session_leaves_no_lock() {
    let root = TempRoot::new("no-residue");
    let persistence = root.backend();
    let refusal = persistence
        .open(&id("absent"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::NotFound(_)));
    assert!(root.files().is_empty());

    // An absent id beside a stored one is still not found.
    write_log(&persistence, meta("present-sibling", None), second_turn(0)).await;
    let refusal = persistence
        .open(&id("absent-sibling"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::NotFound(_)));

    let handle = persistence
        .create(meta("erased", None), CreateOptions::default())
        .await
        .unwrap();
    assert!(!lock(&root, "erased").parent().unwrap().exists());
    handle.close().await.unwrap();
    assert!(!lock(&root, "erased").parent().unwrap().exists());
    assert!(persistence.stat(&id("erased")).await.unwrap().is_none());
}

#[tokio::test]
async fn the_first_write_takes_the_lock_and_keeps_it() {
    let root = TempRoot::new("lazy-lock");
    let first = root.backend();
    let second = root.backend();
    let creator = first
        .create(meta("lazy-lock", None), CreateOptions::default())
        .await
        .unwrap();
    creator.append(second_turn(0)).await.unwrap();
    assert!(lock(&root, "lazy-lock").is_file());
    let refusal = second
        .open(&id("lazy-lock"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyOwned(_)));
    // A later append uses the lock it holds.
    creator.append(second_turn(2)).await.unwrap();
    creator.close().await.unwrap();
    second
        .open(&id("lazy-lock"), SessionAccess::Write)
        .await
        .unwrap()
        .close()
        .await
        .unwrap();

    // A flushed empty session takes the lock with its header.
    let creator = first
        .create(meta("flush-lock", None), CreateOptions::default())
        .await
        .unwrap();
    creator.flush().await.unwrap();
    let refusal = second
        .open(&id("flush-lock"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyOwned(_)));
    creator.close().await.unwrap();
}

#[tokio::test]
async fn distinct_sessions_lock_independently() {
    let root = TempRoot::new("distinct");
    let first = root.backend();
    let second = root.backend();
    let a = first
        .create(meta("a", None), CreateOptions::default())
        .await
        .unwrap();
    let b = second
        .create(meta("b", None), CreateOptions::default())
        .await
        .unwrap();
    a.append(second_turn(0)).await.unwrap();
    b.append(second_turn(0)).await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();
}

/// Run by [`another_process_is_excluded_until_it_dies`] in a child process:
/// takes the write lock of the session `held` and keeps it until killed.
#[test]
#[ignore = "run only as the holder child of another test"]
fn lease_holder_child() {
    let Ok(root) = std::env::var(HOLD_ROOT) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let persistence =
            karaka_session_jsonl::JsonlPersistence::new(karaka_session_jsonl::JsonlConfig {
                root: root.into(),
            })
            .unwrap();
        let handle = persistence
            .create(meta("held", None), CreateOptions::default())
            .await
            .unwrap();
        handle.append(second_turn(0)).await.unwrap();
        println!("held");
        // Never closes: the parent kills this process, as a crash would.
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        std::process::abort();
    });
}

#[tokio::test]
async fn another_process_is_excluded_until_it_dies() {
    let root = TempRoot::new("two-process");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_holder_child", "--ignored", "--nocapture"])
        .env(HOLD_ROOT, root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = lines.next().expect("the child holds the lock").unwrap();
        if line == "held" {
            break;
        }
    }

    let persistence = root.backend();
    let refusal = persistence
        .open(&id("held"), SessionAccess::Write)
        .await
        .err()
        .unwrap();
    assert!(matches!(refusal, PersistenceError::AlreadyOwned(_)));
    // Reading needs no lock.
    assert_eq!(seqs(&read_all(&persistence, "held").await.unwrap()), [0, 1]);

    // A crashed holder's lock dies with it: the session is writable at once.
    child.kill().unwrap();
    child.wait().unwrap();
    let writer = persistence
        .open(&id("held"), SessionAccess::Write)
        .await
        .unwrap();
    writer.append(second_turn(2)).await.unwrap();
    writer.close().await.unwrap();
}

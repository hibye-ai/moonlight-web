//! Streamer shutdown supervisor regressions (MW-03).
//!
//! These execute a real child process. The defect under test: `Stop` was awaited on
//! a possibly-full IPC queue and the child was reaped only *after* that await and
//! after `session.close()`, so a wedged child was never killed and the per-host
//! stream lock stayed held.
use super::*;
use std::process::Stdio;
use std::time::Instant;
use tokio::process::{Child, Command};

/// A child that ignores graceful shutdown entirely (it just sleeps).
async fn stubborn_child() -> Child {
    Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sleep child")
}

/// MW-03: a child that does not exit on its own must be killed, and the whole
/// shutdown must respect a single overall deadline.
#[tokio::test]
async fn unresponsive_child_is_killed_within_the_deadline() {
    let mut child = stubborn_child().await;
    let pid = child.id().expect("child pid");

    let started = Instant::now();
    let outcome = reap_streamer_child(&mut child, Duration::from_millis(100)).await;
    let elapsed = started.elapsed();

    assert_eq!(
        outcome,
        ReapOutcome::Killed,
        "an unresponsive streamer must be killed, not waited on forever"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown must finish near the deadline, took {elapsed:?}"
    );
    assert!(
        child.try_wait().expect("try_wait").is_some(),
        "the child must be reaped before shutdown returns"
    );
    assert!(
        !process_exists(pid),
        "no streamer process may outlive the shutdown supervisor"
    );
}

/// MW-03: a child that exits promptly is reported as a clean shutdown and is not
/// needlessly killed.
#[tokio::test]
async fn cooperative_child_exits_cleanly() {
    let mut child = Command::new("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn true child");

    let outcome = reap_streamer_child(&mut child, Duration::from_secs(5)).await;

    assert_eq!(outcome, ReapOutcome::Exited);
}

/// MW-03: even when the IPC control queue is saturated (the child never reads its
/// stdin), Stop must not block shutdown. The gate has to fall through to the kill.
#[tokio::test]
async fn saturated_ipc_queue_does_not_block_shutdown() {
    let (mut child, stdin, stdout) = spawn_with_pipes().await;

    let (mut ipc_sender, _ipc_receiver) = create_child_ipc::<ServerIpcMessage, StreamerIpcMessage>(
        tracing::Span::none(),
        stdin,
        stdout,
        None,
    )
    .await;

    // Fill the bounded queue without anybody draining it to the child.
    let mut queued = 0;
    while ipc_sender.try_send(ServerIpcMessage::Stop) {
        queued += 1;
        assert!(queued < 1_000, "the IPC queue must be bounded");
    }
    assert!(queued > 0, "the queue should accept at least one message");

    // The queue is now full: a blocking send would hang here.
    let started = Instant::now();
    let sent = try_send_stop(&mut ipc_sender);
    let elapsed = started.elapsed();

    assert!(
        !sent,
        "a full queue must report the message as not delivered"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "the non-blocking Stop must return immediately, took {elapsed:?}"
    );

    // Shutdown still completes even though Stop could not be delivered.
    let outcome = reap_streamer_child(&mut child, Duration::from_millis(100)).await;
    assert_eq!(outcome, ReapOutcome::Killed);
}

async fn spawn_with_pipes() -> (
    Child,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
) {
    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn child");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    (child, stdin, stdout)
}

#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_exists(_pid: u32) -> bool {
    // Not observable portably here; the `child.try_wait` assertion still applies.
    false
}

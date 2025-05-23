// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::assert_eq;
use crate::test::new_isolate;
use anyhow::*;
use ffx_executor::FfxExecutor;
use fuchsia_async::MonotonicDuration;
use nix::sys::signal;
use nix::unistd::Pid;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration as StdDuration;

pub(crate) async fn test_echo() -> Result<()> {
    let isolate = new_isolate("daemon-echo").await?;
    isolate.start_daemon().await?;
    let out = isolate.exec_ffx(&["daemon", "echo"]).await?;

    let want = "SUCCESS: received \"Ffx\"\n";
    assert_eq!(out.stdout, want);

    Ok(())
}

pub(crate) async fn test_isolate_cleanup() -> Result<()> {
    let isolate = std::panic::AssertUnwindSafe(new_isolate("isolate-cleanup").await?);
    let isolate_dir = PathBuf::from(isolate.dir());
    let mut daemon = isolate.start_daemon().await?;

    assert!(isolate_dir.exists());

    // Move the isolate into the closure, then panic.
    let result = std::panic::catch_unwind(move || {
        let _isolate = isolate;
        panic!()
    });

    result.expect_err("Closure somehow did not panic");

    // The isolate directory is cleaned up on panic.
    assert!(!isolate_dir.exists());

    // This also triggers the daemon to shutdown.
    while let None = daemon.try_wait().expect("failed to wait for daemon exit") {
        sleep(StdDuration::from_secs(1));
    }

    Ok(())
}

pub(crate) async fn test_config_flag() -> Result<()> {
    let isolate = new_isolate("daemon-config-flag").await?;
    let mut daemon = isolate.start_daemon().await?;

    assert_eq!(None, daemon.try_wait()?, "Daemon exited quickly after starting");

    // This should not terminate the daemon just started, as it won't
    // share an overnet socket with it.
    let mut ascendd_path2 = isolate.ascendd_path().clone();
    ascendd_path2.set_extension("2");
    let _out = isolate
        .exec_ffx(&[
            "--config",
            &format!("overnet.socket={}", ascendd_path2.to_string_lossy()),
            "daemon",
            "stop",
            "-t",
            "1000",
        ])
        .await?;

    assert_eq!(
        None,
        daemon.try_wait()?,
        "Daemon didn't stay up after the stop message was sent to the other socket."
    );

    // TODO(): on the mac, we may need to explicitly tell the daemon to exit,
    // because the socket-watcher doesn't seem to always work.  We don't want
    // to use "ffx daemon stop" in general, however, since the new daemon may
    // not yet have created the ascendd socket. (This usually happens only with
    // targets, so since we don't do host tests on the mac with targets, this
    // should be a non-issue on mac builders, at least for now.)
    if cfg!(target_os = "macos") {
        fuchsia_async::Timer::new(MonotonicDuration::from_millis(500)).await;
        let _ = isolate.exec_ffx(&["daemon", "stop", "-t", "3000"]).await?;
    }

    // Because we created the daemon, it won't go away (i.e. in cleanup_isolate())
    // until we wait() for it. So instead we drop the isolate here, then wait.
    drop(isolate);
    fuchsia_async::unblock(move || daemon.wait()).await?;
    Ok(())
}

pub(crate) async fn test_stop() -> Result<()> {
    let isolate = new_isolate("daemon-stop").await?;
    let out = isolate.exec_ffx(&["daemon", "stop", "-t", "3000"]).await?;
    let want = "No daemon was running.\n";
    assert_eq!(out.stdout, want);

    let daemon = isolate.start_daemon().await?;

    // Reap the daemon in another thread so the daemon exits.
    // Otherwise the process will still exist but in a zombie state.
    let _ = std::thread::spawn(move || { daemon }.wait());

    let out = isolate.exec_ffx(&["daemon", "stop", "-t", "3000"]).await?;
    let want = "Stopped daemon.\n";
    assert_eq!(out.stdout, want);

    Ok(())
}

pub(crate) async fn test_no_autostart() -> Result<()> {
    let isolate = new_isolate("daemon-no-autostart").await?;
    let out = isolate.exec_ffx(&["daemon", "echo"]).await?;
    assert!(!out.status.success());
    let want = "FFX Daemon was told not to autostart and no existing Daemon instance was found";
    assert!(
        out.stderr.contains(want),
        "stderr does not have '{}'. stderr reads: '{}'",
        want,
        out.stderr.trim()
    );

    let mut daemon = isolate.start_daemon().await?;

    assert_eq!(None, daemon.try_wait()?, "Daemon exited quickly after starting");

    let out = isolate.exec_ffx(&["daemon", "echo"]).await?;
    // Don't assert here -- it if fails, we still want to kill the daemon
    let echo_succeeded = out.status.success();

    // Want to kill the daemon here, rather than in cleanup_isolate(), because since we
    // forked it explicitly, we want to wait()
    let _ = daemon.kill();
    daemon.wait().expect("Daemon wasn't running");

    assert!(echo_succeeded);

    Ok(())
}

pub(crate) async fn test_cleanup_on_signal() -> Result<()> {
    let isolate = new_isolate("daemon-cleanup-on-signal").await?;
    let mut daemon = isolate.start_daemon().await?;
    let socket_out = isolate.exec_ffx(&["--machine", "json", "daemon", "socket"]).await?.stdout;
    let socket_details: serde_json::Value = serde_json::from_str(&socket_out)?;
    let path: std::path::PathBuf = socket_details
        .get("socket")
        .expect("socket should exist")
        .get("path")
        .expect("socket.path should exist")
        .as_str()
        .expect("socket.path should be a string")
        .into();
    assert!(path.exists());
    let pid = Pid::from_raw(daemon.id() as i32);
    signal::kill(pid, Some(signal::Signal::SIGTERM))?;
    fuchsia_async::unblock(move || daemon.wait()).await?;
    assert!(!path.exists());
    // We don't need to return the isolate, since we've already killed the daemon
    Ok(())
}

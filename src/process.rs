// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;

/// Run a captured subprocess while sharing the caller's process-lifetime SIGINT stream.
///
/// On cancellation, kill and reap the child before returning so it cannot keep mutating shared
/// repository state after boro starts unwinding its guards.
pub async fn cancellable_output(
    mut command: tokio::process::Command,
    label: &str,
    interrupt: &mut tokio::signal::unix::Signal,
) -> Result<std::process::Output> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().with_context(|| format!("run {label}"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await?;
        std::io::Result::Ok(buf)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).await?;
        std::io::Result::Ok(buf)
    });

    let status = tokio::select! {
        status = child.wait() => status.with_context(|| format!("wait for {label}"))?,
        _ = interrupt.recv() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(anyhow::anyhow!("operation cancelled"));
        }
    };
    let stdout = stdout_task
        .await
        .with_context(|| format!("join stdout reader for {label}"))??;
    let stderr = stderr_task
        .await
        .with_context(|| format!("join stderr reader for {label}"))??;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

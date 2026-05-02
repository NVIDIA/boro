// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::error::Error;
use std::fmt;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

pub const REVIEW_STAGE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct ModelStageTimeout {
    label: String,
    timeout: Duration,
}

impl ModelStageTimeout {
    pub fn new(label: impl Into<String>, timeout: Duration) -> Self {
        Self {
            label: label.into(),
            timeout,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "model stage timed out after {}: {}",
            format_duration(self.timeout),
            self.label
        )
    }
}

impl fmt::Display for ModelStageTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

impl Error for ModelStageTimeout {}

pub fn error(label: impl Into<String>, timeout: Duration) -> anyhow::Error {
    anyhow::Error::new(ModelStageTimeout::new(label, timeout))
}

pub fn is(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ModelStageTimeout>().is_some()
}

pub fn wait_child_poll(child: &Arc<Mutex<Child>>, context: &'static str) -> Result<ExitStatus> {
    loop {
        let status = {
            let mut child = child
                .lock()
                .map_err(|_| anyhow!("subprocess child lock poisoned"))?;
            child.try_wait().context(context)?
        };
        if let Some(status) = status {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 && secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

pub struct ChildTimeoutGuard {
    done_tx: Option<mpsc::Sender<()>>,
    timed_out: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl ChildTimeoutGuard {
    pub fn spawn(child: Arc<Mutex<Child>>, timeout: Duration) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_for_thread = Arc::clone(&timed_out);
        let join = thread::spawn(move || {
            if done_rx.recv_timeout(timeout).is_ok() {
                return;
            }
            timed_out_for_thread.store(true, Ordering::SeqCst);
            let Ok(mut child) = child.lock() else {
                return;
            };
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = child.kill();
        });
        Self {
            done_tx: Some(done_tx),
            timed_out,
            join: Some(join),
        }
    }

    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }
}

impl Drop for ChildTimeoutGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.done_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_error_is_classified() {
        let err = error("stage label", REVIEW_STAGE_TIMEOUT);
        assert!(is(&err));
        let msg = err.to_string();
        assert!(msg.contains("10m"));
        assert!(msg.contains("stage label"));
    }
}

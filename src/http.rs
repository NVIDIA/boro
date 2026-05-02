// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! HTTP client for remote inference: large request bodies and long server think times.
use anyhow::{Context, Result};
use std::time::Duration;

/// Build a client suitable for multi‑MB JSON prompts and slow inference APIs.
///
/// - Long **connect** (5 min) and **overall** (1 h) timeouts for big uploads and slow inference.
/// - **HTTP/1.1 only**: avoids sporadic HTTP/2 stall/timeout behavior seen with some
///   cloud gateways (same class of issues as reqwest #2283).
/// - **TCP keepalive** so idle long responses are less likely to be dropped by middleboxes.
pub fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(300))
        .timeout(Duration::from_secs(3_600))
        .tcp_keepalive(Duration::from_secs(60))
        // Drop idle pooled connections sooner so the next request is less likely to hit a
        // dead socket (common with long gaps between multi-stage calls).
        .pool_idle_timeout(Duration::from_secs(45))
        .pool_max_idle_per_host(4)
        .user_agent(concat!("boro/", env!("CARGO_PKG_VERSION")))
        .http1_only()
        .build()
        .context("build reqwest HTTP client")
}

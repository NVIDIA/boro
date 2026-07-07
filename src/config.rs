// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};

pub const DEFAULT_MAX_INPUT_TOKENS: u32 = 32_768;

/// Which transport boro uses to talk to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// OpenAI-compatible HTTP chat/completions endpoint (default).
    OpenAi,
    /// Shell out to the `claude` CLI in non-interactive mode.
    Claude,
    /// Shell out to the `opencode` CLI (`opencode run`) in non-interactive mode.
    Opencode,
    /// Shell out to the `codex` CLI (`codex exec`) in non-interactive mode.
    Codex,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::OpenAi => "openai",
            Backend::Claude => "claude",
            Backend::Opencode => "opencode",
            Backend::Codex => "codex",
        }
    }

    pub fn is_subprocess(self) -> bool {
        matches!(self, Backend::Claude | Backend::Opencode | Backend::Codex)
    }
}

/// Which codebase the `review` pipeline targets. Selects the embedded prompt
/// corpus (subsystem guides, core patterns, report template) and the reviewer
/// personas. Defaults to [`ReviewTarget::Kernel`]; `build` / `test` only ever
/// operate on the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewTarget {
    /// Linux kernel (prompts synced from Sashiko under `resources/prompts/kernel/`).
    #[default]
    Kernel,
    /// QEMU (boro-authored prompts under `resources/prompts/qemu/`).
    Qemu,
    /// libvirt (boro-authored prompts under `resources/prompts/libvirt/`).
    Libvirt,
    /// virt-manager / virtinst (boro-authored prompts under `resources/prompts/virt-manager/`).
    VirtManager,
}

impl ReviewTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewTarget::Kernel => "kernel",
            ReviewTarget::Qemu => "qemu",
            ReviewTarget::Libvirt => "libvirt",
            ReviewTarget::VirtManager => "virt-manager",
        }
    }
}

/// Best-effort classification of a source tree as a Linux kernel, QEMU,
/// libvirt or virt-manager checkout from unambiguous signature files. Returns
/// `None` when the tree matches none (or, defensively, more than one) — callers
/// should stay silent in that case rather than guess. Used only to warn on a
/// likely `--target` mismatch.
pub fn detect_tree_kind(repo: &std::path::Path) -> Option<ReviewTarget> {
    let has = |rel: &str| repo.join(rel).exists();
    let qemu = has("qapi") && has("qemu-options.hx") && has("include/qemu/osdep.h");
    let kernel = has("Kbuild") && has("mm") && has("kernel/sched") && has("include/linux/kernel.h");
    let libvirt =
        has("include/libvirt/libvirt.h") && has("libvirt.spec.in") && has("src/libvirt.c");
    let virtmanager = has("virtinst") && has("virtManager") && has("virtinst/guest.py");
    match (kernel, qemu, libvirt, virtmanager) {
        (true, false, false, false) => Some(ReviewTarget::Kernel),
        (false, true, false, false) => Some(ReviewTarget::Qemu),
        (false, false, true, false) => Some(ReviewTarget::Libvirt),
        (false, false, false, true) => Some(ReviewTarget::VirtManager),
        _ => None,
    }
}

/// Resolved model description used by every backend.
///
/// For `OpenAi`, `base_url` and `api_key` come from `BORO_URL` / `BORO_KEY` and `model_id` from
/// `BORO_MODEL`. For subprocess backends, `base_url` and `api_key` are unused; `model_id` is
/// optional — when empty, the CLI uses its own default. `prompt_cache` is populated from the
/// `-c` / `--enable-prompt-cache` CLI flag by `main.rs` after construction.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub backend: Backend,
    pub model_id: String,
    pub base_url: String,
    pub api_key: String,
    /// When true, `chat_completion` sends `cache_control: ephemeral` markers on the system and
    /// initial user message blocks (Anthropic-style prompt caching). Default `false` — sends the
    /// classic OpenAI string-content shape, byte-identical to pre-flag boro.
    pub prompt_cache: bool,
    /// Sampling `temperature` to forward in the OpenAI-compat request body when `Some`. `None`
    /// omits the field, letting the provider use its default. Populated from `BORO_TEMPERATURE`
    /// on the main model and `BORO_VALIDATION_TEMPERATURE` on the validation model — the two
    /// are independent (the validation knob does NOT inherit the main value when unset).
    pub temperature: Option<f32>,
    /// Ollama context-window override forwarded as top-level `options.num_ctx` when `Some`.
    /// Populated from `BORO_NUM_CTX` on the main model and `BORO_VALIDATION_NUM_CTX` on the
    /// validation model — the two are independent (the validation knob does NOT inherit the
    /// main value when unset). Ollama's OpenAI-compat layer reads this; most other OpenAI-
    /// compat servers silently ignore unknown top-level fields, but some strict gateways
    /// (e.g. Bedrock-via-litellm) reject them with HTTP 400 — do not set this env var when
    /// targeting such endpoints.
    pub num_ctx: Option<u32>,
    /// Conservative preflight budget for input tokens. `chat_completion` trims oversized
    /// OpenAI-compatible requests before POSTing them so provider context-window errors don't
    /// abort a stage. Populated from `BORO_MAX_INPUT_TOKENS` / `BORO_VALIDATION_MAX_INPUT_TOKENS`;
    /// falls back to `num_ctx` when set, otherwise [`DEFAULT_MAX_INPUT_TOKENS`].
    pub max_input_tokens: Option<u32>,
}

/// Placeholder when `--dry-run` is used (no API calls; env vars not required).
pub fn dry_run_placeholder() -> ResolvedModel {
    ResolvedModel {
        backend: Backend::OpenAi,
        model_id: "(dry-run)".to_string(),
        base_url: String::new(),
        api_key: String::new(),
        prompt_cache: false,
        temperature: None,
        num_ctx: None,
        max_input_tokens: None,
    }
}

/// Strip trailing slashes and optional `/v1`, then append `/v1` for OpenAI-compatible APIs.
fn normalize_boro_base_url(raw: &str) -> String {
    let mut s = raw.trim().trim_end_matches('/').to_string();
    if let Some(without) = s.strip_suffix("/v1") {
        s = without.trim_end_matches('/').to_string();
    }
    format!("{}/v1", s.trim_end_matches('/'))
}

/// Parse an optional `f32` env var. Returns `Ok(None)` for unset or empty-after-trim;
/// `Err` (with the var name in the context) when the value fails to parse.
fn env_parse_f32(key: &str) -> Result<Option<f32>> {
    match std::env::var(key).ok().map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => s
            .parse::<f32>()
            .with_context(|| format!("{key} is set but is not a valid float: {s:?}"))
            .map(Some),
        _ => Ok(None),
    }
}

/// Parse an optional `u32` env var. Returns `Ok(None)` for unset or empty-after-trim;
/// `Err` (with the var name in the context) when the value fails to parse.
fn env_parse_u32(key: &str) -> Result<Option<u32>> {
    match std::env::var(key).ok().map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => s
            .parse::<u32>()
            .with_context(|| format!("{key} is set but is not a valid u32: {s:?}"))
            .map(Some),
        _ => Ok(None),
    }
}

/// Read environment variables appropriate for `backend`.
///
/// - `OpenAi`: requires `BORO_URL` and `BORO_MODEL`; `BORO_KEY` is optional.
/// - `Claude` / `Opencode` / `Codex`: all env vars are optional. `BORO_MODEL`, when set, is
///   forwarded as the CLI's model flag.
///
/// In addition, `BORO_TEMPERATURE` and `BORO_NUM_CTX` are optional sampling knobs read by every
/// backend (subprocess backends ignore them at the call sites in `api.rs`).
pub fn resolve_model_from_env(backend: Backend) -> Result<ResolvedModel> {
    let temperature = env_parse_f32("BORO_TEMPERATURE")?;
    let num_ctx = env_parse_u32("BORO_NUM_CTX")?;
    let max_input_tokens = env_parse_u32("BORO_MAX_INPUT_TOKENS")?
        .or(num_ctx)
        .or(Some(DEFAULT_MAX_INPUT_TOKENS));

    match backend {
        Backend::OpenAi => {
            let base_url = std::env::var("BORO_URL").context(
                "BORO_URL is not set (OpenAI-compatible API base URL, e.g. https://localhost:8000/v1)",
            )?;
            let api_key = std::env::var("BORO_KEY").unwrap_or_default();
            let model_id = std::env::var("BORO_MODEL")
                .context("BORO_MODEL is not set (value for the JSON \"model\" field)")?;

            let base_url = normalize_boro_base_url(&base_url);
            if base_url.is_empty() || base_url == "/v1" {
                anyhow::bail!("BORO_URL is set but empty after trim");
            }
            let api_key = api_key.trim().to_string();
            let model_id = model_id.trim().to_string();
            if model_id.is_empty() {
                anyhow::bail!("BORO_MODEL is set but empty after trim");
            }

            Ok(ResolvedModel {
                backend,
                model_id,
                base_url,
                api_key,
                prompt_cache: false,
                temperature,
                num_ctx,
                max_input_tokens,
            })
        }
        Backend::Claude | Backend::Opencode | Backend::Codex => {
            let model_id = std::env::var("BORO_MODEL")
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            Ok(ResolvedModel {
                backend,
                model_id,
                base_url: String::new(),
                api_key: String::new(),
                prompt_cache: false,
                temperature,
                num_ctx,
                max_input_tokens,
            })
        }
    }
}

/// Resolve the model used for the global review-validation stage.
///
/// `BORO_VALIDATION_MODEL` / `_URL` / `_KEY` fall back to their `BORO_*`
/// counterpart on `main` when unset (because the validation model is
/// usually a same-or-related model on the same endpoint, so inheriting
/// is the helpful default):
/// - `BORO_VALIDATION_MODEL` → `main.model_id`
/// - `BORO_VALIDATION_URL`   → `main.base_url` (OpenAI backend only; normalized)
/// - `BORO_VALIDATION_KEY`   → `main.api_key`  (OpenAI backend only)
///
/// Sampling knobs do **not** inherit — `BORO_VALIDATION_TEMPERATURE` and
/// `BORO_VALIDATION_NUM_CTX` default to `None` when unset. This avoids a
/// nasty footgun where `BORO_NUM_CTX=32768` (set for an Ollama main model)
/// silently bleeds into a non-Ollama validation model and trips strict
/// gateways like Bedrock-via-litellm with HTTP 400 ("options: Extra
/// inputs are not permitted").
///
/// Backend is always inherited from the main run. For subprocess backends,
/// URL/KEY env vars are still read so validation override reporting remains
/// consistent, but boro itself does not require them.
pub fn resolve_validation_from_env(main: &ResolvedModel) -> Result<ResolvedModel> {
    let env_nonempty = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let model_id = env_nonempty("BORO_VALIDATION_MODEL").unwrap_or_else(|| main.model_id.clone());
    let api_key = env_nonempty("BORO_VALIDATION_KEY").unwrap_or_else(|| main.api_key.clone());

    let base_url = match main.backend {
        Backend::OpenAi => match env_nonempty("BORO_VALIDATION_URL") {
            Some(u) => {
                let normalized = normalize_boro_base_url(&u);
                if normalized.is_empty() || normalized == "/v1" {
                    anyhow::bail!("BORO_VALIDATION_URL is set but empty after trim");
                }
                normalized
            }
            None => main.base_url.clone(),
        },
        Backend::Claude | Backend::Opencode | Backend::Codex => {
            env_nonempty("BORO_VALIDATION_URL").unwrap_or_else(|| main.base_url.clone())
        }
    };

    let num_ctx = env_parse_u32("BORO_VALIDATION_NUM_CTX")?;
    let temperature = env_parse_f32("BORO_VALIDATION_TEMPERATURE")?;
    let max_input_tokens = env_parse_u32("BORO_VALIDATION_MAX_INPUT_TOKENS")?
        .or(num_ctx)
        .or(main.max_input_tokens);

    Ok(ResolvedModel {
        backend: main.backend,
        model_id,
        base_url,
        api_key,
        prompt_cache: main.prompt_cache,
        temperature,
        num_ctx,
        max_input_tokens,
    })
}

/// True iff the validation config differs from the main model in any
/// user-visible field — used to label verbose log lines.
pub fn validation_differs(main: &ResolvedModel, validation: &ResolvedModel) -> bool {
    main.model_id != validation.model_id
        || main.base_url != validation.base_url
        || main.api_key != validation.api_key
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn normalize_boro_base_url_accepts_host_root_or_v1_base() {
        for (raw, expected) in [
            ("https://localhost:8000", "https://localhost:8000/v1"),
            ("https://localhost:8000/", "https://localhost:8000/v1"),
            ("https://localhost:8000/v1", "https://localhost:8000/v1"),
            ("https://localhost:8000/v1/", "https://localhost:8000/v1"),
            (
                " https://proxy.example.com/openai/v1/ ",
                "https://proxy.example.com/openai/v1",
            ),
        ] {
            assert_eq!(normalize_boro_base_url(raw), expected);
        }
    }

    fn touch_all(root: &Path, rels: &[&str]) {
        for rel in rels {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            if matches!(*rel, "mm" | "kernel/sched" | "qapi") {
                std::fs::create_dir_all(&p).unwrap();
            } else {
                std::fs::write(&p, b"x").unwrap();
            }
        }
    }

    #[test]
    fn detects_kernel_tree() {
        let tmp = tempfile::tempdir().unwrap();
        touch_all(
            tmp.path(),
            &["Kbuild", "mm", "kernel/sched", "include/linux/kernel.h"],
        );
        assert_eq!(detect_tree_kind(tmp.path()), Some(ReviewTarget::Kernel));
    }

    #[test]
    fn detects_qemu_tree() {
        let tmp = tempfile::tempdir().unwrap();
        touch_all(
            tmp.path(),
            &["qapi", "qemu-options.hx", "include/qemu/osdep.h"],
        );
        assert_eq!(detect_tree_kind(tmp.path()), Some(ReviewTarget::Qemu));
    }

    #[test]
    fn detects_libvirt_tree() {
        let tmp = tempfile::tempdir().unwrap();
        touch_all(
            tmp.path(),
            &[
                "include/libvirt/libvirt.h",
                "libvirt.spec.in",
                "src/libvirt.c",
            ],
        );
        assert_eq!(detect_tree_kind(tmp.path()), Some(ReviewTarget::Libvirt));
    }

    #[test]
    fn detects_virtmanager_tree() {
        let tmp = tempfile::tempdir().unwrap();
        touch_all(
            tmp.path(),
            &["virtinst/guest.py", "virtManager/connection.py"],
        );
        assert_eq!(
            detect_tree_kind(tmp.path()),
            Some(ReviewTarget::VirtManager)
        );
    }

    #[test]
    fn unclassifiable_tree_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        touch_all(tmp.path(), &["README.md", "src/main.rs"]);
        assert_eq!(detect_tree_kind(tmp.path()), None);
    }

    #[test]
    fn ambiguous_tree_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        touch_all(
            tmp.path(),
            &[
                "Kbuild",
                "mm",
                "kernel/sched",
                "include/linux/kernel.h",
                "qapi",
                "qemu-options.hx",
                "include/qemu/osdep.h",
            ],
        );
        assert_eq!(detect_tree_kind(tmp.path()), None);
    }
}

# Boro

## What Boro is for

Boro is an AI-assisted kernel development CLI for developers working inside
their own git tree. It is meant for the stages before a patch series is posted
publicly, or for downstream work that may never follow the normal mailing-list
review path: backports, distro kernel maintenance, security-fix integration,
and other local patch-stack iteration.

The project is inspired by [Sashiko](https://github.com/sashiko-dev/sashiko).
The two tools share the same underlying review prompts, but target different
moments in the patch lifecycle. Sashiko is optimized for reviewing patch series
after they appear on public mailing lists; Boro is optimized for interactive
local review and repair while the developer is still shaping the series or
carrying it downstream.

## TL;DR

Install boro from this checkout:

```bash
cargo install --force --path .
```

Point it at any OpenAI-compatible chat/completions endpoint:

```bash
export BORO_URL=https://api.example.com/v1
export BORO_KEY=<api-key>
export BORO_MODEL=<model-name>
```

Run a review from a Linux kernel git tree:

```bash
cd /path/to/linux
boro review origin/master..HEAD
```

Passing a single commit ID is interpreted as `abc123^..abc123`.

## Demo

https://github.com/user-attachments/assets/d4dc2533-9928-48b0-aca4-dbd9197398cb

## Design and workflow

### Synchronous execution

The whole pipeline runs synchronously in one process. A live multi-row status
shows per-worker state and per-stage token counts as work happens.

### Model roles and token accounting

Boro separates the bulk-of-work model from the strong-validator model.

`BORO_MODEL` drives the main pipeline; `BORO_VALIDATION_MODEL` drives the
per-commit second-opinion pass and the global review-validation stage.

The intended workflow is to keep the broad and specialist passes cheap while
using a stronger model for second-opinion review, validation, and reporting.

For example, run a local model on the broad and specialist stages and point the
validation stages at a stronger remote model. Token usage is broken out per
stage in the run summary so you can see exactly where the budget went.

### Review, build and test

Boro supports optional build and test validation: it can build each commit and,
when requested, boot the resulting kernel under virtme-ng to run an AI-selected
targeted test.

Subcommands:

- `boro review COMMIT_RANGE`: multi-stage agent code review using kernel-focused
  prompts (subsystem notes, patterns, checklists), with an LKML-style narrative
  summary at the end.
- `boro build COMMIT_RANGE` - checks out each commit in its own worktree, builds
  it with `vng -b`, then asks the model to triage the build log.
- `boro test COMMIT_RANGE` - same as `build`, then boots the kernel under
  virtme-ng and runs a model-picked quick test inside the VM (a matching
  kselftest, a userspace probe of the touched code path, or `dmesg` as
  fallback). The model triages the captured output and produces a summary plus
  any findings.
- `boro test --config CONFIG_FOO` - builds `HEAD` with `CONFIG_FOO=y` merged into
  virtme-ng's default config, asks the model to pick a quick test for that
  option from the Kconfig/source context, then boots and runs it. Explicit
  values are accepted, e.g. `CONFIG_FOO=m`, `CONFIG_NR_CPUS=512`, or
  `CONFIG_CMDLINE="console=ttyS0 root=/dev/vda"`.
  Add `--plan` to generate a detailed, non-executed test plan. Plan mode can
  describe multi-step, long-running, or hardware-dependent tests because boro
  does not build or boot the kernel in that mode. When `--plan` is used with a
  multi-commit range, boro generates one integrated test plan for the whole
  range instead of one plan per commit.

The `build` and `test` modes feed real compiler output and real kernel runtime
output back to the model, not just the diff text. That's what justifies running
on your local machine instead of submitting a job somewhere.

### AI-assisted cherry-pick

Boro also supports AI-assisted cherry-picking of commits that do not apply
cleanly, automatically attempting to resolve conflicts.

Subcommands:

- `boro apply COMMIT_ID` - runs `git cherry-pick -x -s COMMIT_ID`. If Git
  reports a failed or empty cherry-pick, boro first checks whether the commit
  subject is already present in path-limited history and skips it when found.
  Otherwise, boro looks for explicit commit references in the target commit
  message; any referenced commits that are not already in `HEAD` are tried
  first with `git cherry-pick -x -s`, then the target commit is retried.

  For remaining diff3 conflicts, `BORO_MODEL` proposes per-conflict resolutions
  and `BORO_VALIDATION_MODEL` gates them. Validator rejections are sent back to
  the main model for another resolution attempt, up to 10 rounds per hunk. If
  every hunk passes validation, boro rewrites the conflicted files, stages them,
  continues the cherry-pick, updates git's `-x` trailer to `(backported from
  commit ...)`, and prints what the agent changed.

  After the post-apply review pass, boro runs a source-only check over newly
  added struct field accesses and flags obvious backport gaps. If that check
  fails, the validation model gets the exact issue plus `edit_file`, amends the
  commit when it can repair the gap, and boro reruns the source-only check.

## Requirements

- For `build` / `test`: install
[virtme-ng](https://github.com/arighi/virtme-ng) (`vng`) and make sure it's
on your `PATH`.
- Optional: `lei` (from the
[`public-inbox`](https://public-inbox.org/) package) on `$PATH` for the
upstream-followup stage. Without `lei`, boro still runs every other stage
and produces a complete review - it just skips the lore.kernel.org
retrieval. The Linux-master `Fixes:` lookup remains available. To disable
the lore portion explicitly even when `lei` is installed, set
`BORO_LORE_ENABLED=0`.

## Build & install

```bash
cargo build --release
# binary: target/release/boro
```

Optionally, install the binary running:

```bash
cargo install --force --path .
```

## Credentials

The cost-aware, multi-model story above is driven by these env vars.
The validator slot is independent of the main model so you can mix
endpoints freely (cheap local main, strong remote validator, or any
other combination).

### Using an API Key

For real API runs, set the following OpenAI-compatible environment variables:

| Variable     | Meaning                        |
|--------------|--------------------------------|
| `BORO_URL`   | OpenAI-compatible API base URL |
| `BORO_KEY`   | API key                        |
| `BORO_MODEL` | Model name                     |

Optionally, the global **review-validation** stage, per-commit
**second-opinion** stage, and `boro apply` conflict validator can be pointed
at a different (often stronger) model:

| Variable                | Meaning                                                    |
|-------------------------|------------------------------------------------------------|
| `BORO_VALIDATION_URL`   | Base URL for the validation model (defaults to `BORO_URL`) |
| `BORO_VALIDATION_KEY`   | API key for the validation model (defaults to `BORO_KEY`)  |
| `BORO_VALIDATION_MODEL` | Name of the validation model (defaults to `BORO_MODEL`)    |

Any unset value falls back to its `BORO_*` counterpart, so setting just
`BORO_VALIDATION_MODEL` is enough when the alternate model lives on the
same endpoint.

### Agent Backend

As an alternative to an API key, you can use the Claude, OpenCode, or Codex
backends via `--backend claude|opencode|codex`.

In this mode, boro relies on the agent CLI for credentials, tooling, and
permissions.
The Codex backend uses `codex exec --json` with approval prompts disabled
(`--ask-for-approval never` and `--dangerously-bypass-approvals-and-sandbox`).

## Use with local Ollama

Pull the qwen3-coder:30b model with Ollama:

```bash
$ ollama pull qwen3-coder:30b
$ ollama serve
```

Review the commits from origin/master to HEAD in a Linux kernel git tree:

```bash
$ cd /path/to/linux
$ BORO_URL=http://127.0.0.1:11434 \
  BORO_MODEL=qwen3-coder:30b \
  boro review origin/master..HEAD
```

## Review pipeline

For each commit in the range, Boro first collects patch metadata and source
context locally, then runs the ordered stages below.

| Step | Stage | Brief description |
| ---: | --- | --- |
| 0 | Identify kernel subsystem | Select subsystem guide prompts for the shared reference bundle |
| 1 | Upstream follow-up | Query lore.kernel.org and summarize relevant follow-ups |
| 2 | Broad concerns | First pass over reference context + patch, collects concerns |
| 3 | Execution flow verification | Control flow, logic, errors, branches, macros, linker/LTO risks |
| 4 | Resource management | Lifetimes, leaks, UAF, refcounts, teardown vs. async handoffs |
| 5 | Locking and concurrency | Sleep/atomic rules, ordering, races, RCU, barriers, IRQ context |
| 6 | Security | Bounds, overflow, privesc, TOCTOU, user/kernel data boundaries |
| 7 | Hardware & portability | Drivers/HW: DMA, IRQ, barriers, endianness |
| 8 | Comment / code consistency | Audit comments touched by the patch against the actual code |
| 9 | Consolidation pass | Merges concerns into findings and applies severity guidance |
| 10 | Second-opinion check | Independent review; findings merge into the main findings |
| 11 | Findings validation | Drops false positives and tightens surviving findings |
| 12 | LKML-style report | Narrative reply text plus run-wide findings summary |

`BORO_MODEL` is used for steps 0-9. `BORO_VALIDATION_MODEL` is used for
steps 10-12 and falls back to `BORO_MODEL` when unset.

`--validation-mode` changes only the post-discovery stages:

- `filter` (default): run findings validation, then render LKML prose from
  `validated_findings[]`.
- `findings`: run findings validation and skip LKML rendering.
- `off`: skip findings validation and render LKML prose from raw `findings[]`.

`boro review --upstream-repo URI --upstream-branch BRANCH COMMIT_RANGE`
selects the Git repository and branch checked for follow-up fixes. It defaults to
`git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git`; use a
local path or `file://` URI to inspect a local Linux repository instead.
`--upstream-branch` defaults to `master`.

With `--fast`, the per-commit discovery pipeline is replaced by a single
review call using `BORO_MODEL`; the second-opinion pass, global validation,
LKML rendering, and summary reporting still use `BORO_VALIDATION_MODEL` as
described above.

### Upstream follow-up stage (lore.kernel.org + upstream branch)

Before discovery starts, boro runs one deterministic, stateless query
against `lore.kernel.org` to surface existing mailing-list activity around
the patch - v-bumps, `Fixes:` patches sent later, and substantive
maintainer review. It also fetches a blob-less snapshot of the upstream
selected branch directly into Git's transient `FETCH_HEAD` and checks its
commit messages for `Fixes: <12-char-reviewed-sha>` trailers. That catches
follow-up fixes which have reached the configured upstream branch but are
not discoverable in the configured lore window. The lore result feeds every
downstream discovery stage as part of the reference bundle, so concerns the
upstream community already raised can be used by later review stages. Git
`Fixes:` hits from the configured upstream branch are also added as
high-severity findings, so they survive validation and appear in the final
report even when the model would otherwise ignore reference-only context.
The query is:

```
lei q -I https://lore.kernel.org/all/ -f mboxo --threads -d mid --no-save \
    -- '"<patch-subject>" AND rt:<window>'
```

Config (all optional):

| Variable              | Default                       | Effect                                |
|-----------------------|-------------------------------|---------------------------------------|
| `BORO_LORE_ENABLED`   | `1`                           | Set to `0` to skip the stage entirely |
| `BORO_LORE_WINDOW`    | `1.year.ago..`                | Public-inbox `rt:` window.            |
| `BORO_LORE_MAX_BYTES` | `65536`                       | Max bytes kept from the lei result    |
| `BORO_LORE_INBOX_URL` | `https://lore.kernel.org/all` | Remote inbox                          |

The lore portion requires `lei` (from the `public-inbox` package) on
`$PATH`; the upstream branch lookup uses only `git` and still runs when
`lei` is not installed. It runs independently of the lore lookup, including
when `lei` is available. The upstream branch is fetched once per kernel
review into `FETCH_HEAD`; boro does not add or modify a configured remote. A
failed fetch, `lei` query, or model call is logged; the review continues with
whichever follow-up source remains available.

### Comment / code consistency stage

The sixth specialist stage (step 8 above) audits whether the comments
touched by a patch accurately describe the code they refer to. Unlike
the other specialists, which are tuned to find bugs, this stage flags
wording-level divergences too - a kernel-doc that claims two flags are
"cleared together" when the code clears them with two sequential
statements is reported even when correctness is unaffected, because
stale or imprecise comments are a known source of later regressions.

The stage runs procedurally: enumerate every distinct factual claim
made by comments in the diff (block, line, trailing, kernel-doc),
locate the code that backs each claim (the prompt makes tool use
mandatory when the referenced code lives in another function or
file), verify each claim against the located code, and emit a
finding for every contradiction - quoting both the comment text and
the contradicting code line(s).

Patterns explicitly flagged include atomicity / ordering claims that
the code does not implement (mutex-serialised is not atomic), locking
or caller-context claims contradicted by an actual caller, stale or
renamed symbol references in comments the patch touched, kernel-doc
`@param` / `Return:` / `Context:` mismatches, and pre/post-condition
claims the code does not establish.

Skipped on `--fast` (single-pass collapse).

### Extra options

The option `--fast` collapses the pipeline into a single findings-focused call
per commit and it can be used as a shortcut to get a really quick and cheap
review.

The option `--validation-mode` selects whether (and how) the global
findings-validation stage runs:

- `filter` (default): runs findings validation, then renders the per-
  commit LKML prose from the surviving findings only. The viewer / human
  report's Findings section shows `validated_findings`; the LKML section
  shows prose built from those survivors.
- `findings`: runs findings validation, **skips** the per-commit LKML
  pass entirely (saves one LLM call per commit; the human report's LKML
  section is empty in this mode). `scripts/boro-json-view` auto-detects
  this mode (and `filter`) and anchors `validated_findings` inline
  beside the diff at each finding's `location.file:line` - use
  `--use-validated=auto|always|never` to override.
- `off`: skip validation entirely. Per-commit LKML is rendered from raw
  `findings[]` after the second-opinion findings have been merged in.

The global flag `--json` emits a single pretty-printed JSON document on
stdout instead of the human report - the same shape consumed internally,
with per-commit `findings[]`, `lkml_report` (filter/off modes only),
`validated_findings[]` (filter/findings modes when validation
succeeded), and a `usage_summary`. Each finding may carry an optional
`location` object:

```json
{"file": "path/in/diff", "line": 42, "line_end": 45, "side": "RIGHT"}
```

`side` defaults to `RIGHT` (post-image / additions / context) when missing;
`LEFT` anchors against the pre-image (deletions). `line_end` is optional
for ranges. The model is instructed to **omit** `location` rather than
invent one when a finding can't be pinned to a hunk - and as a backstop,
boro parses each commit's unified diff once and drops any `location`
that doesn't anchor to a real hunk line on the given side (or whose file
isn't in the commit's changed paths). The finding's prose is preserved;
only the bad anchor is removed, so the viewer falls back to rendering it
as a commit-level comment.

For terminal viewing, `scripts/boro-json-view` consumes that JSON and
renders a GitHub-style "Files changed" view: per-file diff with old/new
line-number gutters, ANSI-colored diff body, and severity-colored inline
comment boxes anchored at each finding's location.

```bash
boro review HEAD~3..HEAD --json | scripts/boro-json-view | less -R
```

For `boro test`, the `--timeout SECONDS` flag bounds the in-VM command
(default 300s). Bump it when running long kselftests.

Use `boro test --plan COMMIT_RANGE` or `boro test --plan --config CONFIG_FOO`
to ask for a detailed test plan without invoking `vng -b` or booting the
kernel. For a multi-commit range, boro asks for one plan that covers the whole
series. Unlike the normal `test` picker, plan mode is not limited to one quick
command inside a minimal virtme-ng VM: it may name required hardware, kernel
config, setup steps, commands, and expected success or failure signals.

Prompt caching is **on by default**: boro sends Anthropic-style
`cache_control` markers on the system and initial user blocks so the
provider can serve the fixed prefix from prompt cache across the per-stage
tool-loop iterations (and across stages, since the system prompt is the
same). Subprocess CLIs (`--backend claude` / `opencode` / `codex`) manage
their own caching and ignore this. If the provider rejects the markers (model
families with a different caching mechanism, e.g. Gemini), boro retries
the same request once without markers and silently keeps caching off for
the remainder of the run. Pass `--no-prompt-caching` to skip the marker
shape (and the one-time fallback probe) when running against an endpoint
you know doesn't support it.

When reviewing a series of multiple commits the tool runs multiple workers in
parallel processing the review pipeline sequentially. At the end all the
findinds from all the workers are merged together to produce the LKML-style
report (step 10).

The tool aims to finish the whole range even when individual steps or API calls
fail: you may see partial results or per-commit error notes in the report
instead of the whole run aborting.

## Example output

- boro review:

```
 LKML-style report
────────────────────────────────────────────────────────────────────────
  Commit 56930b7600b9...
  ····························································
  commit 56930b7600b9d512e73e7c9468c0f362481546d5
  Author: Andrea Righi <arighi@nvidia.com>

  sched/core: Skip put_prev_task/set_next_task re-entry for sched_ext donors

  This commit avoids re-entering the sched_ext class during proxy-execution
  donor stabilization to prevent potential BPF vtable corruption or NULL
  pointer dereferences. It checks the donor's scheduling class before
  calling put_prev_task and set_next_task.

  > diff --git a/kernel/sched/core.c b/kernel/sched/core.c
  > index 75541e5bb66d1..1c161dd9d7440 100644
  > --- a/kernel/sched/core.c
  > +++ b/kernel/sched/core.c
  > @@ -7147,9 +7147,14 @@ static void __sched notrace __schedule(int sched_mode)
  [ ... ]
  > -			donor->sched_class->put_prev_task(rq, donor, donor);
  > -			donor->sched_class->set_next_task(rq, donor, true);
  > +			if (donor->sched_class != &ext_sched_class) {
  > +				donor->sched_class->put_prev_task(rq, donor, donor);
  > +				donor->sched_class->set_next_task(rq, donor, true);
  > +			}

  Does this direct reference to ext_sched_class cause a build regression when
  CONFIG_SCHED_CLASS_EXT is disabled?

  It appears that ext_sched_class is used here in __schedule() without a check
  for whether the sched_ext class is actually defined or enabled in the
  kernel configuration.

  Commit d430a73a84b4...
  ····························································
  commit d430a73a84b470169a8f10f15bb0d486b8fbcdae
  Author: Andrea Righi <arighi@nvidia.com>

  sched_ext: Fix migration-disabled tasks with proxy execution

  This patch addresses two races on the SCX_DSQ_LOCAL_ON deferred dispatch
  path under proxy execution: (1) mutex-blocked donors being fed through
  ops.enqueue() producing stale deferred LOCAL_ON entries, and (2) those
  deferred entries becoming stale by drain time. It short-circuits
  put_prev_task_scx() for blocked donors and skips stale entries in
  process_ddsp_deferred_locals().

  I have a couple of questions about potential regressions:

  > diff --git a/kernel/sched/ext.c b/kernel/sched/ext.c
  > index 1f9ae082811f0..859e2bf4cf808 100644
  > --- a/kernel/sched/ext.c
  > +++ b/kernel/sched/ext.c

  [ ... ]

  > @@ -4078,6 +4102,25 @@ static void process_ddsp_deferred_locals(struct rq *rq)
  >  		list_del_init(&p->scx.dsq_list.node);
  >  		clear_direct_dispatch(p);
  >
  > +		/*
  > +		 * Skip stale entries. The deferred LOCAL_ON entry was queued
  > +		 * by BPF's ops.enqueue() at wakeup time; by the time we drain,
  > +		 * @p's state may have moved on:
  > +		 *
  > +		 *   - task_on_cpu: @p is already running. The dispatch is moot
  > +		 *     and reading remote @p->migration_disabled here races
  > +		 *     with BPF trampoline prologs that flip it transiently.
  > +		 *
  > +		 *   - task_is_blocked: @p became a proxy-exec donor and is
  > +		 *     pinned to its current rq as part of the proxy chain.
  > +		 *     Cross-CPU dispatch would tear it out of the chain.
  > +		 *
  > +		 * In both cases the next ttwu / put_prev cycle re-fires
  > +		 * ops.enqueue() with fresh state.
  > +		 */
  > +		if (task_on_cpu(task_rq(p), p) || task_is_blocked(p))
  > +			continue;

  Could there be a TOCTOU race here? Both task_on_cpu(task_rq(p), p) and
  task_is_blocked(p) are read without holding the remote rq lock. If a
  concurrent put_prev_task or context switch on another CPU changes the
  task's state between this check and the subsequent
  dispatch_to_local_dsq() call, could the task transition into the blocked
  state right after the check passes?

  That would lead to the exact cross-CPU dispatch of a proxy-exec donor
  that this patch is trying to prevent.

  Additionally, when this check causes a deferred entry to be skipped, the
  task has already been removed from the deferred list via list_del_init()
  and had clear_direct_dispatch() called on it. The comment says "the next
  ttwu / put_prev cycle re-fires ops.enqueue() with fresh state."

  Is that guaranteed? If the task is a blocked donor stuck in a mutex
  chain that never resolves (e.g., a priority inversion deadlock), would
  it ever receive a ttwu? The first hunk handles blocked donors in
  put_prev_task_scx(), but that only fires if put_prev runs for that task
  on its current rq.

  Could a task end up on no dispatch queue and no deferred list after
  being skipped here, with no future event to re-enqueue it?

  >  		dsq = find_dsq_for_dispatch(sch, rq, dsq_id, task_cpu(p));
  >  		if (!WARN_ON_ONCE(dsq->id != SCX_DSQ_LOCAL))
  >  			dispatch_to_local_dsq(sch, rq, dsq, p, enq_flags);
```

- boro test:

```
Findings
────────────────────────────────────────────────────────────────────────
  8b9be1c3f479...  (0 findings)
    test: make -C tools/testing/selftests run_tests TARGETS="sched_ext" SKIP_TARGETS=""
    The kselftest for the sched_ext scheduler was executed successfully, running
    all 26 test cases without any failures or timeouts. The tests exercised
    various aspects of the scheduler's task lifecycle callbacks, including
    running and stopping operations, dispatch behaviors, and proxy-exec donor
    handling. While some tests intentionally triggered error conditions (like
    loading invalid BPF programs), these were handled gracefully as expected.
    The kernel appeared to be functioning correctly throughout the test
    execution.
```

## See also

```bash
boro --help
```

## License

- **Boro** is licensed under the **Apache License 2.0** - see
  [`LICENSE`](LICENSE).

- **`resources/prompts/kernel/`** is **third-party material** from
  **[Sashiko](https://github.com/sashiko-dev/sashiko)** and is also under the
  **Apache License 2.0**.

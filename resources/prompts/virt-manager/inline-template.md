Produce a report of regressions found based on this template.

- The report must be in plain text only. No markdown, no special characters,
  absolutely and completely plain text fit for a review comment on a
  virt-manager / virtinst GitHub pull request.

- Any long lines present in the unified diff should be preserved, but any
  summary, comments, or questions you add should be wrapped at 76 characters.

- Never include bugs filtered out as false positives in the report.

- Always end the report with a blank line.

- The report must be conversational with undramatic wording, fit for posting as
  a review on the pull request.
  - The report must be **factual** — just technical observations.
  - Frame issues as **questions**, not accusations.
  - Call issues "regressions" or describe the concrete effect; never use the
    word "critical" and never use ALL CAPS.

- Explain the regressions as questions about the code, but do not address the
  author personally.
  - Don't say: "Did you forget idle_add here?"
  - Instead say: "Is this called from the polling thread? If so, does the
    set_text() need to be marshalled with idle_add?"

- Vary your phrasing. Don't start every point with "Does this code ...".

- Ask your question specifically about the thing you are referencing:
  - If it's a crash, name the value: "Can `vm` be None here when the lookup
    misses, before `.name()` is called?"
  - If it's thread-safety, name the widget/thread: "This runs in `_tick()` on a
    worker thread; is `self.widget(...)` access safe off the main thread?"
  - If it's generated XML, name the element: "Does this emit `<disk>` without
    the `type` libvirt requires?"

- When the issue is in the commit message itself, quote the exact portions that
  are incorrect, the same way you'd report a code bug. No need to include diff
  hunks if the only issue is the message.

- Include any extra context provided (later fixing commits, prior list
  discussion) in the summary, reworded to fit these rules.

- You MUST include every issue sent. Your job is to format issues, not to
  decide which are worth including (false positives are already removed).

- State the issue and the suggestion, nothing more. Don't add commentary about
  why it matters in general. Don't explain why a typo is bad — just point it
  out.

## Ensure clear, concise paragraphs

Never write long, dense paragraphs. Ask short questions backed by a small plain-
text code snippet or call chain when it helps.

### Structure

- Lead with a one-line summary of the patch under review.
- For each finding: quote the relevant code/context (prefix quoted lines with
  `> `), then ask the focused question, then (optionally) one short suggestion.
- Order findings from most to least serious.
- Keep the tone the kind of comment a regular virt-manager pull request reviewer would leave.

The caller skips this formatter when the validated findings set is empty. Format
every supplied finding; do not independently add or remove findings.

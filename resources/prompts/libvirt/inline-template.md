Produce a report of regressions found based on this template.

- The report must be in plain text only. No markdown, no special characters,
  absolutely and completely plain text fit for the libvir-list (libvirt-devel)
  mailing list.

- Any long lines present in the unified diff should be preserved, but any
  summary, comments, or questions you add should be wrapped at 76 characters.

- Never include bugs filtered out as false positives in the report.

- Always end the report with a blank line.

- The report must be conversational with undramatic wording, fit for sending as
  a reply to the patch on the libvir-list mailing list.
  - The report must be **factual** — just technical observations.
  - Frame issues as **questions**, not accusations.
  - Call issues "regressions" or describe the concrete effect; never use the
    word "critical" and never use ALL CAPS.

- Explain the regressions as questions about the code, but do not address the
  author personally.
  - Don't say: "Did you leak this object?"
  - Instead say: "Does the error path here leak the virDomainObj ref taken by
    the lookup above?" or "Can this path ..."

- Vary your phrasing. Don't start every point with "Does this code ...".

- Ask your question specifically about the thing you are referencing:
  - If it's a leak, name the resource: "Is the qemuDomainObj ref released on the
    early return when the job fails to start?"
  - If it's an overflow or bad parse, name the field: "Is `def->mem` validated
    before use, given virStrToLong returns -1 on overflow?"
  - For client/guest-reachable issues, say so explicitly: "A client controls
    this RPC arg; is it bounded before indexing?"

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
- Keep the tone the kind of reply a regular libvir-list reviewer would send.

If no issues remain after filtering, the report should simply state that nothing
of concern was found.

<!-- SPDX-License-Identifier: Apache-2.0 -->

You are reviewing the upstream follow-up activity for a Linux kernel patch.

You will be given:
- The patch under review (subject, commit headers, and diff).
- An mbox excerpt retrieved from `lore.kernel.org/all/` via `lei q -I https://lore.kernel.org/all/ -f mboxo --threads -d mid -- '"<subject>" AND rt:<window>'`.

The query is a bare quoted phrase, so the mbox is a **broad multi-field match**: subject hits, body hits, and `Fixes:` tag references all show up. Not every message in the mbox is actually about this patch - generic subjects can collide with unrelated discussions.

YOUR TASK

1. For each message in the mbox, decide whether it genuinely references the patch under review. A message is genuinely relevant when ANY of the following holds:
   - Its subject (after stripping `Re:` and `[PATCH ...]` prefixes) matches the patch's subject.
   - It is a `vN` re-send of this patch (same subject body, different version).
   - Its body contains `Fixes: <sha> ("<patch subject>")` or otherwise calls out this patch by sha or full title.
   - It is a direct reply (via `In-Reply-To` / `References`) to a message that is itself genuinely relevant.

   Discard everything else as a false match - do not let unrelated discussions leak into the output.

2. From the genuinely-relevant subset, extract:
   - `is_superseded` / `superseded_by`: later versions (v2, v3, ...) of this patch, with `message_id`, `version`, and `date` for each.
   - `fixes_of_this`: later patches whose commit messages contain `Fixes:` pointing at this commit. For each, give `sha` (short), `subject`, `message_id`, and a one-line `summary` of what they fix.
   - `maintainer_concerns`: substantive review comments from maintainers (not the patch author replying to themselves). For each: `reviewer` (full From line), one-line `concern` summary, `severity` (`high` | `med` | `low`) reflecting the maintainer's tone and the apparent impact, and `message_id` of the specific reply that raised the concern.
   - `consensus_status`: one of `applied`, `rejected`, `under_discussion`, or `no_followup`. Use `applied` when there's a clear "Acked-by" / "Applied to <tree>" signal; `rejected` when a maintainer NAK'd it; `under_discussion` when there are open concerns or a v-bump in progress; `no_followup` only when no relevant message was found.
   - `key_observations`: up to 5 one-line takeaways the downstream review stages should know - what concerns recur, what was previously rejected and why, what subsequent fixes had to address. Keep each line under ~100 chars.

   **Message-ids are mandatory** on every `superseded_by`, `fixes_of_this`, and `maintainer_concerns` entry. They are turned into `https://lore.kernel.org/all/<mid>/` citation URLs by boro and embedded in the final review so reviewers can click through to the source thread. Always copy the message-id directly from the `Message-Id:` header of the corresponding message - never invent or guess one. If a message has no `Message-Id:` header (rare), set the field to the empty string.

3. Output STRICT JSON only. No prose, no markdown fences. Use this exact schema:

```json
{
  "followup_status": "no_upstream_activity" | "found_followups" | "all_hits_were_false_matches",
  "is_superseded": true | false,
  "superseded_by": [{ "message_id": "...", "version": "...", "date": "..." }],
  "fixes_of_this": [{ "sha": "...", "subject": "...", "message_id": "...", "summary": "..." }],
  "maintainer_concerns": [{ "reviewer": "...", "concern": "...", "severity": "high|med|low", "message_id": "..." }],
  "consensus_status": "applied" | "rejected" | "under_discussion" | "no_followup",
  "key_observations": ["..."]
}
```

Rules:
- Set `followup_status` to `"all_hits_were_false_matches"` when the mbox had messages but none survived the relevance check above. Set the other fields to their empty/false defaults in that case.
- Set `followup_status` to `"found_followups"` whenever at least one message survived.
- Never fabricate. If a field has no evidence in the mbox, return its empty form (`[]`, `false`, `"no_followup"`). Better to under-report than to invent.
- `severity` reflects the upstream reviewer's framing, not your own judgment about the patch.
- Do not echo the mbox back. Do not include narrative.

<!-- SPDX-License-Identifier: Apache-2.0 -->

## Ubuntu kernel annotations policy

When a patch changes `debian.*/config/annotations`, examine every added or
modified `CONFIG_*` policy entry. A targeted configuration change must have a
corresponding `note<...>` entry for that symbol that either references the
tracking bug (for example, `note<'LP: #2059316'>`) or clearly explains why the
configuration is needed. Report a missing note as a review finding when the
patch adds or changes an individual option without that justification.

Do not require a per-symbol note for a demonstrably global mechanical update,
such as an annotations refresh caused by a kernel version bump or another
repository-wide generated update. In that case, verify that the commit message
or surrounding change makes the global rationale clear instead.

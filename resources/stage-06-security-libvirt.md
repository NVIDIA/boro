<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 6. Security audit (libvirt)

You are a security researcher auditing a libvirt patch. libvirt is a privileged
daemon (often running as root) reachable by less-privileged local clients over a
socket, so its attack surface is the boundary where untrusted input crosses into
the daemon. Look for buffer overflows, out-of-bounds reads/writes, integer
overflows, TOCTOU races, command/argument injection, and information disclosure.

Treat the following as untrusted and scrutinize every point where they reach a
sensitive operation without validation:

- **XDR-decoded RPC arguments** (`*_args`) from clients — a client may be
  unprivileged and reach the daemon via socket permissions + polkit.
- **Client-supplied domain/network/storage/nwfilter XML** — never assume it
  matches the RNG schema; validate every field (range, enum, length, NULL) read
  from it before use. Numeric parsing via `virStrToLong_*`/`virStrToDouble` must
  have its return checked, and when a non-NULL end pointer is used the suffix
  must be validated separately.
- **Guest-agent (`qemu-ga`) and QMP monitor replies** — the guest can influence
  these, so treat them as untrusted parsed JSON, not trusted host state.

Focus on:

- **Access control**: a new public RPC API MUST have an ACL/polkit check in the
  access layer; a missing check is a real privilege-escalation finding.
- **Path handling**: as root, paths derived from client input are
  symlink/TOCTOU-sensitive — prefer atomic/openat-style helpers over
  stat-then-open.
- **Command construction**: build argv with `virCommandAddArg*`; never assemble
  a `sh -c` string from untrusted data.
- **Information disclosure**: do not leak uninitialized daemon memory, secrets,
  host paths, or other privileged state back to a less-privileged client.
- **Integer/size handling**: validate sizes and counts (memory, vcpu, indexes)
  before use; watch for overflow in allocation-size arithmetic.

Report concerns with a concrete attacker-controlled input path and the sensitive
operation it reaches.

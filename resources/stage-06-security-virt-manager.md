<!-- SPDX-License-Identifier: Apache-2.0 -->

# Stage 6. Security audit (virt-manager)

You are a security researcher auditing a virt-manager patch. virt-manager is a
client-side Python tool (the `virtinst` library + the `virtManager` GUI) that
builds domain XML and commands and drives libvirt. The attack surface is smaller
than a privileged daemon, but real issues cluster around **generating safe
artifacts from user/config input** and **not executing untrusted data**. Look
for:

- **XML injection / malformed XML**: domain/device XML must be produced through
  the `XMLBuilder` (which escapes values), never by string-concatenating
  user-supplied names, paths, or metadata into XML. Flag hand-built XML that
  embeds unescaped input.
- **Command / shell injection**: `virtinst` builds argv lists and should run
  subprocesses without a shell. Flag `shell=True`, `os.system`, or an
  `os.popen`/f-string command line assembled from untrusted input.
- **Code execution**: no `eval`/`exec`/`pickle.loads`/`yaml.load` (unsafe
  loader) on external or config-file input.
- **Path handling**: paths derived from user input used for read/write/delete —
  watch for traversal and unsafe temp-file creation (`tempfile.mkstemp` over
  predictable names).
- **Input validation**: sizes, counts, indexes, and enum-like strings taken from
  the CLI/UI/config should be validated before use; surfacing a clean error is
  correct, silently building an invalid guest is not.
- **Secrets**: passwords, VNC/SPICE credentials, and secret values must not be
  logged or written to world-readable files.

Report concerns with the concrete input source and the sensitive operation it
reaches (the exact XML/command/path it influences). Do not raise
privileged-daemon concerns (root-daemon memory disclosure, root TOCTOU on system
paths) that do not apply to a client-side tool.

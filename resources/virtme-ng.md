---
name: virtme-ng
description: "Build and test Linux kernels using virtme-ng (`vng`). Use `vng --build` to compile, `vng` to run the built kernel, and `vng -- <command>` to run commands inside the VM. Supports remote build hosts, kconfig, kselftests, applying patches from lore.kernel.org (b4), and running released kernels."
---

<!-- SPDX-License-Identifier: Apache-2.0 -->

# virtme-ng Skill

Use the `vng` CLI to build and test Linux kernels from source. virtme-ng compiles a minimal kernel quickly, then runs it in QEMU on a copy-on-write snapshot of the host, so tests are safe and isolated.

**When the user asks to build kernels, test kernels, run something in a kernel VM, or use virtme-ng:** use the commands below. Always run from the kernel source directory (or pass the path and `cd` there first) unless the user specifies otherwise.

## Building a kernel

Build from the current kernel source directory (creates a minimal `.config` if missing):

```bash
vng --build
```

Short form:

```bash
vng -b
```

Build a specific tag or commit:

```bash
vng --build --commit v6.2-rc4
# or
vng -b -c v6.1-rc3
```

Build with verbose output:

```bash
vng --build --verbose
```

Build on a remote host (e.g. faster or different environment):

```bash
vng --build --build-host builder
```

With a chroot on the remote host:

```bash
vng --build --build-host builder --build-host-exec-prefix "schroot -c chroot:kinetic-amd64 -- "
```

Build with custom kernel config options:

```bash
vng --build --configitem CONFIG_KASAN=y --configitem CONFIG_DEBUG_INFO=y
```

Build for another architecture (e.g. arm64; may need `--root` for a compatible rootfs):

```bash
vng --build --arch arm64 --root /path/to/chroot
```

Build with a separate output directory:

```bash
export KBUILD_OUTPUT=.virtme/build
vng --build --verbose
# then run with: vng --verbose -- O=.virtme/build
```

## Running the built kernel

After building, run the kernel (interactive shell in the VM):

```bash
vng
```

Run with a one-off command and stream output back to the host:

```bash
vng -- uname -r
```

When running from scripts or automation without a real terminal, wrap with `script` to provide a PTY:
```bash
script -q -c 'vng -- uname -r' /dev/null 2>&1
```

Run with networking:

```bash
vng --net user
```

Run in verbose mode (e.g. to debug boot):

```bash
vng --verbose
```

## Running without building (released or installed kernels)

Run the **host** kernel in a VM (no build, quick sanity check):

```bash
vng -r
```

Run a specific **installed** kernel (e.g. from the system):

```bash
vng -r 6.2.0-21-generic
```

Run a **vanilla** kernel version (downloaded from Ubuntu mainline-style builds):

```bash
vng -r v6.6
```

Run a custom kernel image by path:

```bash
vng -r ./boot/vmlinuz-6.2.0-1003-lowlatency
```

Run host kernel with a one-off command:

```bash
vng -r -- uname -r
```

## Kernel config only

Generate a minimal `.config` without building (optional; `vng --build` creates one if missing):

```bash
vng --kconfig
```

To apply specific config options without editing `.config` by hand, run a build with `--configitem` (e.g. `vng -v --build --configitem CONFIG_KASAN=y --configitem CONFIG_DEBUG_INFO=y`). For a different architecture, use `vng --kconfig` then edit, or build with `--arch`.

## Cleaning

Clean the kernel tree (git clean style); use when you want a fresh build:

```bash
vng --clean
```

Clean on a remote build host:

```bash
vng --clean --build-host HOSTNAME
```

## Common workflows

**“Build this kernel and run uname in it”**

```bash
cd /path/to/linux
vng --build
vng -- uname -r
```

**“Build and boot, then I’ll use the shell”**

```bash
vng --build
vng
# user types "exit" to leave VM
```

**“Test the current tree with KASAN”**

```bash
vng --build --configitem CONFIG_KASAN=y
vng -- uname -r
```

**“Just run the host kernel in a VM and run a command”**

```bash
vng -r -- uname -r
```

**“Build on remote host ‘builder’ and then run here”**

```bash
vng --build --build-host builder
vng
```

## Kernel selftests (kselftests)

Run in-tree kernel selftests (e.g. `sched_ext`, `vm`, `net`, `seccomp`) inside a virtme-ng VM. Build the kernel first, install headers and build the selftest **on the host**, then run only the test inside the VM (the guest filesystem is copy-on-write and host writes don't propagate, so headers/test binaries must be built outside). From the kernel tree, after `vng --build`:

```bash
# 1. Install kernel headers (host)
make headers_install

# 2. Build the selftest (host)
make -j"$(nproc)" -C tools/testing/selftests/sched_ext

# 3. Run the test inside the VM (use script wrapper for automation)
script -q -c 'vng -- make -C tools/testing/selftests run_tests TARGETS="sched_ext" SKIP_TARGETS=""' /dev/null 2>&1
```

Use `TARGETS="net"`, `TARGETS="vm"`, etc. for other tests. To run selftests on the host kernel (no build), use `vng -r -- make -C tools/testing/selftests run_tests TARGETS="sched_ext" SKIP_TARGETS=""` (still build the test on the host first). Kselftests can take 10+ minutes; use a sufficient timeout when automating. Some tests (e.g. `sched_ext`) require specific `CONFIG_*` options — check `tools/testing/selftests/<test>/config` and rebuild with `--configitem` or `--config tools/testing/selftests/<test>/config` if needed.

## Applying patches from lore.kernel.org

Apply patch series from the kernel mailing list (lore.kernel.org) using **b4 shazam**. Requires `b4` (`pip install b4`), a git repo, and a clean working tree; git `user.name` and `user.email` must be set. The message ID is in the lore.kernel.org email URL or in the Message-Id header.

```bash
cd /path/to/linux
b4 shazam <message-id>
```

Example message ID: `20251029191111.167537-1-author@example.com`. This downloads the series, applies patches, and creates git commits with proper authorship.

## Kernel source info

Inspect the current kernel tree from its directory:

```bash
# Version
make kernelversion

# Git commit and branch
git rev-parse HEAD
git branch --show-current

# Uncommitted changes
git status --short

# Config present and architecture (matches one of the arch symbols)
test -f .config && grep -E '^CONFIG_(X86_64|ARM64|ARM|PPC64|RISCV|S390)=y' .config || true
```

## Validating patch series (build + boot each commit)

To validate a range of commits (e.g. “ensure each commit builds and boots”):

1. Get commits: `git rev-list --reverse START^..END`
2. Save current HEAD: `git rev-parse HEAD`
3. For each commit: checkout → **build** (`vng -v --build`, or `vng -v --build --build-host HOST`) → **boot** (mandatory): e.g. `script -q -c 'vng -- uname -r' /dev/null 2>&1`. Record both build and boot result.
4. Restore: `git checkout SAVED_HEAD`

A commit passes only if **both** build and boot succeed. A kernel that builds but does not boot counts as failed. Allow 10–60+ minutes per commit; use `--build-host` when the user mentions a remote build server.

## Automation notes

- **Building:** Run `vng -v --build` (or with `--build-host`, `--configitem`) from the kernel tree. Do not use `vng -- <cmd>` to build; that runs the already-built kernel in a VM.
- **Each `vng -- <cmd>` boots a new VM;** state does not persist. Combine related commands in one run (e.g. `vng -- 'modprobe foo && dmesg | grep foo'`), wrapped in `script` when not in a real terminal.
- **Architecture:** Only pass `--arch` when the user explicitly requests a specific architecture (e.g. "test on arm64"); otherwise the host architecture is used.
- **Timeouts:** Builds and kselftests often need 10–60+ minutes; use a sufficient timeout when automating.

## Options reference (summary)

| Intent              | Option / example |
|---------------------|-------------------|
| Build               | `vng --build` / `vng -b` |
| Run built kernel    | `vng` |
| Run host kernel     | `vng -r` |
| Run other kernel    | `vng -r <version or path>` |
| Run command in VM   | `vng -- <command>` or `vng -r -- <command>` |
| Minimal config only | `vng --kconfig` |
| Remote build        | `vng --build --build-host HOST` |
| Extra kconfig       | `--configitem CONFIG_X=y` |
| Verbose             | `--verbose` |
| Clean               | `vng --clean` |
| Memory size         | `--memory 2G` |
| Network             | `--net user` |

## PTY (pseudo-terminal) requirement

**vng may require a pseudo-terminal (PTY) in some environments.** In automated environments without a real terminal (e.g. CI, scripts, or agent-driven shells), wrap `vng` commands with `script`:

```bash
script -q -c "vng -- uname -r" /dev/null 2>&1
```

- `-q`: quiet (no script start/stop messages)
- `-c`: run the given command then exit
- `/dev/null`: discard the typescript file; only stdout/stderr are needed

Examples for automation:

```bash
# Run a command in the built kernel
script -q -c "vng -- uname -r" /dev/null 2>&1

# Run on host kernel
script -q -c "vng -r -- uname -r" /dev/null 2>&1

# Boot test after a build
script -q -c "vng -- dmesg | head -5" /dev/null 2>&1
```

Interactive use in a real terminal (e.g. `vng` or `vng --build`) does not need this wrapper.

## Notes

- Kernels built with virtme-ng have a `-virtme` suffix in `uname -r`.
- Default VM memory is 1G; use `--memory` (e.g. `--memory 2G`, `--memory 512M`) if tests need more.
- For cross-arch (e.g. arm64, riscv64), use `--arch` and usually `--root` with a matching rootfs; virtme-ng can create a rootfs from an Ubuntu cloud image if needed. Only set `--arch` when the user explicitly requests a specific architecture; otherwise leave it unset.
- User config can go in `~/.config/virtme-ng/virtme-ng.conf` or `~/.virtme-ng.conf` (e.g. `default_opts` for a permanent `--build-host`).

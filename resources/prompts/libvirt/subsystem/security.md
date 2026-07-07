# Security Drivers

`src/security/` implements the confinement drivers: SELinux, AppArmor, and DAC
(uid/gid) labeling, plus the stacked `virSecurityManager`. These directly
enforce the host/guest isolation boundary, so bugs here are often Critical.

## Core invariants

- **Label set/restore must balance.** Every resource labeled for a domain
  (disks, PTYs, hostdevs, hugepage/memory paths, TPM, sockets) on start must be
  restored on stop/teardown and on the start-failure unwind. A leaked label or a
  restore applied to the wrong path weakens or breaks confinement.
- **Restore must not follow a symlink** to relabel an arbitrary host file. Path
  arguments are domain-influenced; the relabel must target the intended object,
  not whatever a symlink points at (TOCTOU). This is a classic libvirt CVE
  pattern — scrutinize new labeling of client-supplied paths.
- The transactional label model (`virSecurityManagerTransactionStart/Commit`)
  applies labels in the namespace; new labeled resources must join the
  transaction so they're applied/rolled back atomically.
- DAC: chown/chmod of guest-visible files must use the resolved domain uid/gid
  and be undone on teardown; don't widen permissions beyond what's required.

## Untrusted input

- Disk/hostdev/channel paths come from domain XML. Confirm the object is what it
  claims (a regular file vs a device vs a symlink) before relabeling.
- seclabel `<seclabel>` settings (type, relabel, model) from the client must be
  validated against the configured security model.

## Common findings

- Label set on start without a matching restore on every stop/error path.
- Relabel of a client-supplied path that can be redirected via a symlink.
- New domain resource type not added to the labeling/transaction set.
- DAC ownership change not reverted on teardown, or applied with wrong uid/gid.

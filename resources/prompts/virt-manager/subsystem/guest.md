# Guest / Install

`virtinst/guest.py` (the `Guest` XMLBuilder), `virtinst/install/` (the
`Installer`, boot/media handling), `domcapabilities.py`, and `osdict.py` (the
osinfo-db lookup) drive VM creation.

## Core invariants

- `Guest.set_defaults()` (and the `_add_default_*` device helpers it calls)
  derive a large amount of config (machine type, firmware, CPU, default disk/net/
  graphics) from the OS variant and host `domcapabilities`. A change here affects
  *every* newly created VM — verify it's gated on the relevant capability/os and
  doesn't regress other guest types.
- OS detection via `osdict`/osinfo-db must handle an unknown/missing OS id
  gracefully (fall back, don't crash). Don't assume a given os entry exists.
- `domcapabilities` reports what the host/QEMU supports; feature selection
  (firmware, CPU mode, machine) must consult it rather than hardcoding, or
  creation fails on hosts that lack the feature.
- The `Installer` sets up boot media/location and tears down transient install
  config after first boot; a failed install must not leave the domain defined in
  a broken transient state.

## Install flow

- `Installer.start_install(guest)` defines the domain and begins the install;
  errors must propagate so the caller can clean up a partially-created VM/storage.
  Don't swallow a define/create failure as success.

## Common findings

- A `set_defaults` change that regresses some arch/os/host (e.g. picks a machine
  or firmware not valid everywhere).
- Unknown OS id / missing osinfo entry dereferenced (crash).
- Hardcoded feature not checked against `domcapabilities`.
- Install failure not propagated, leaving an orphaned domain or storage volume.

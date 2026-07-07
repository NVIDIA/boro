# cgroups / systemd

`src/util/vircgroup*` and `src/util/virsystemd*` place domains into cgroups
(v1 and v2/unified), set resource limits, and integrate with systemd/machined
for scope/slice creation.

## Core invariants

- cgroup v1 vs **v2 (unified)** differ in hierarchy and controller availability;
  code must handle the unified layout and not assume a controller is mounted.
  Check `virCgroupHasController` before writing a controller's knobs.
- A domain's cgroup is created on start and **removed on stop** — a leaked
  scope/slice or cgroup directory accumulates. Failure mid-setup must remove
  what was created.
- Resource values (CPU shares/quota, memory limits, device ACLs, IO weights)
  come from domain XML; validate ranges and unit conversions (bytes vs KiB, the
  cgroup-specific min/max) before writing. An out-of-range write to a cgroup
  knob fails or clamps unexpectedly.
- The **device cgroup / BPF device filter** controls which host devices the
  guest can access; an over-broad allow rule is a confinement weakness. Allow
  exactly the devices the domain config grants, and remove the rule on unplug.

## systemd integration

- Scope/slice creation goes through machined over D-Bus; handle the case where
  systemd is unavailable (fallback path) and don't deadlock the event loop on a
  synchronous D-Bus call.

## Common findings

- Controller knob written without checking the controller exists (esp. on v2).
- cgroup/scope not removed on stop or on a start-failure unwind.
- Device-ACL/BPF rule too permissive, or not removed on device unplug.
- Limit value from XML not range-checked or wrongly unit-converted.

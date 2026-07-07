# Subsystem Guide Index (Libvirt)

Load subsystem guides based on what the code touches. Each guide contains
libvirt-subsystem-specific invariants, API contracts, and common bug patterns.

The triggers column includes path names, function calls, and symbol regexes.
Err on the side of inclusion: only exclude a guide if it is clearly irrelevant.

## Subsystem Guides

| Subsystem | Triggers | File |
|-----------|----------|------|
| QEMU driver | src/qemu/, qemuProcess, qemuDomain, qemuMonitor, qemuMigration, virQEMUCaps | qemu-driver.md |
| Domain/object XML config | src/conf/, virDomainDef, *DefParse*, *DefFormat*, virXPath, RNG schema | domain-conf.md |
| RPC / daemon dispatch | src/rpc/, src/remote/, src/admin/, virNetServer, remoteDispatch, XDR, virNetMessage, keepalive | rpc.md |
| Virtual networks | src/network/, virNetwork, bridge, dnsmasq, firewall, virNetDev | network.md |
| Storage pools/volumes | src/storage/, virStoragePool, virStorageVol, storageBackend, qcow | storage.md |
| Node devices / mdev | src/node_device/, virNodeDevice, mdev, udev, node_device_conf | nodedev.md |
| Security drivers | src/security/, virSecurityManager, selinux, apparmor, DAC, seclabel | security.md |
| Core utilities | src/util/, virBuffer, virHash, virCommand, virFile, virJSON, virString, virStrToLong | util.md |
| cgroups / systemd | src/util/vircgroup, src/util/virsystemd, virCgroup, machined | cgroup.md |
| Secrets | src/secret/, virSecret, secretDef | secret.md |
| Network filters | src/nwfilter/, virNWFilter, ebtables, iptables, learnIPAddress | nwfilter.md |
| CPU models | src/cpu/, virCPUDef, cpuBaseline, cpuCompare, cpuDecode | cpu.md |
| Host device assignment | src/hypervisor/, virHostdev, PCI, USB, VFIO, mdev, virpci, virusb | hostdev.md |
| Event loop | src/util/virevent, virEventAdd, virEventPoll, virNetClient, timeouts | event.md |
| Locking / concurrency | virObjectLock, virMutex, virRWLock, virDomainObjBeginJob, virThreadPool, virCond | locking.md |

## Optional Patterns

Load only when explicitly requested:

- **Core utilities** (util.md): also load whenever string parsing, `virBuffer`,
  or `virCommand` appear, even if the primary subsystem is a specific driver.
- **Locking** (locking.md): always relevant when threads, jobs, the event loop,
  or per-object locks appear.

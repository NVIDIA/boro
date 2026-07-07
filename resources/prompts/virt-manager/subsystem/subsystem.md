# Subsystem Guide Index (Virt-manager)

Load subsystem guides based on what the code touches. Each guide contains
virt-manager-subsystem-specific invariants, API contracts, and common bug
patterns across the `virtinst` library and the `virtManager` GTK GUI.

The triggers column includes path names, function calls, and symbol regexes.
Err on the side of inclusion: only exclude a guide if it is clearly irrelevant.

## Subsystem Guides

| Subsystem | Triggers | File |
|-----------|----------|------|
| XML builder | virtinst/xmlbuilder.py, virtinst/xmlapi.py, XMLProperty, XMLChildProperty, XMLBuilder | xmlbuilder.md |
| Devices | virtinst/devices/, DeviceDisk, DeviceInterface, DeviceController, DeviceHostdev | devices.md |
| Guest / install | virtinst/guest.py, virtinst/install/, Installer, domcapabilities, osdict | guest.md |
| CLI parsing | virtinst/cli.py, virtinstall.py, virtxml.py, virtclone.py, Parser, --disk/--network | cli.md |
| Connection / polling | virtManager/connection.py, connmanager, vmmConnection, _tick, fetch_ objects | connection.md |
| Domain / objects | virtManager/object/, vmmDomain, vmmLibvirtObject, object/domain.py | domain.md |
| GUI windows/dialogs | virtManager/details/, createvm.py, addhardware.py, vmwindow.py, manager.py, *.ui, widget | ui.md |
| Storage | virtinst/storage.py, StoragePool, StorageVolume, virtManager storage browse | storage.md |
| Networking | virtinst/network.py, Network, virtManager host network UI | network.md |
| libvirt API usage | virtManager/lib/, libvirt event loop, libvirtError, lifecycle callbacks, enum map | libvirt-api.md |
| Console / viewers | virtManager/details/console, viewers, VNC, SPICE, vmmConsolePages | console.md |
| Snapshots | virtinst/snapshot.py, DomainSnapshot, checkpoint, virtManager snapshots | snapshot.md |
| Threading / vmmGObject | virtManager/baseclass.py, asyncjob.py, idle_add, vmmGObject, vmmAsyncJob, GLib | threading.md |

## Optional Patterns

Load only when explicitly requested:

- **Threading** (threading.md): always relevant when polling, async jobs,
  libvirt events, or GTK widget access from a callback appear.
- **XML builder** (xmlbuilder.md): also load whenever a new domain/device XML
  element or attribute is added, even if the primary subsystem is the GUI.

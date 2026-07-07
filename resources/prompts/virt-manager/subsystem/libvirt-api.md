# libvirt API Usage

Virt-manager talks to libvirt through the `libvirt-python` bindings. This guide
covers correct use of those bindings and the event loop, plus the enum mapping
in `virtManager/lib/`.

## Core invariants

- **Error handling**: libvirt binding calls raise `libvirt.libvirtError` on
  failure (not return codes). Wrap calls that can fail and either handle or
  surface the error; an uncaught `libvirtError` on a normal action is a crash.
  Use `e.get_error_code()` to distinguish expected cases (e.g. NO_DOMAIN) from
  real failures rather than matching message strings.
- **Object/connection lifetime**: objects (`virDomain`, `virStoragePool`, ...)
  depend on their `virConnect`; keep the connection alive while using them. A
  closed connection invalidates its objects.
- **Event loop**: callbacks (lifecycle, agent, etc.) only fire if the event
  implementation is registered and the loop is running. Register on connect,
  and **deregister** (`domainEventDeregisterAny`, close handlers) on
  disconnect — a callback into a torn-down object is a bug. Keep the callback
  id returned by `*EventRegisterAny` to deregister later.
- **Flags and capabilities**: pass the correct `VIR_*` flags; an operation that
  needs `_AFFECT_CONFIG`/`_AFFECT_LIVE` must pass the right combination or it
  silently affects the wrong state. Check `getLibVersion`/capabilities before
  using a newer API.

## Enum mapping

- `virtManager/lib/libvirtenummap` translates libvirt integer constants to
  names; a new state/event constant must be added there or the UI shows
  "Unknown". Don't hardcode integer values.

## Common findings

- Unhandled `libvirtError` on a routine call (crash).
- Event callback registered but never deregistered (callback into dead object).
- Wrong/`_AFFECT_*` flags so the change hits live vs persistent config
  incorrectly.
- New libvirt enum value not added to the enum map.

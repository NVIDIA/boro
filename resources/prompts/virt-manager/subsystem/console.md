# Console / Viewers

`virtManager/details/console*` and the viewer backends embed the guest graphical
console (VNC/SPICE) via gtk-vnc / spice-gtk, plus serial/text consoles.

## Core invariants

- Connection parameters (host, port, socket path, TLS settings, password) are
  derived from the domain's graphics XML and the (possibly remote) connection.
  A SPICE/VNC **password** must not be logged or exposed; handle it as a secret.
- For remote connections, the console may need an SSH tunnel/socket forward; the
  tunnel must be torn down when the console closes or the VM stops, or it leaks
  fds/processes.
- Viewer widgets are GTK objects on the main thread; libvirt/stream events that
  drive them arrive on other threads and must be marshalled (see threading.md).
- Reconnect/resize/state-change handling must cope with the guest powering off
  or the graphics device changing (hot(un)plug) without crashing — guard against
  the viewer's underlying object becoming invalid.

## Common findings

- Console password logged or placed in a world-readable location.
- SSH tunnel / forwarded socket / fd not cleaned up on console close or VM stop.
- Viewer widget updated from a stream/event thread without marshalling.
- Crash when the guest stops or the graphics device changes mid-session.

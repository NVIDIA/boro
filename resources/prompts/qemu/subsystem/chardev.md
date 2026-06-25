# Character Devices

`chardev/` implements QEMU's character backends (serial, socket, pty, ...) behind
the `Chardev` interface. Front-end devices write to and read from them; socket
backends carry untrusted external input.

## Core invariants

- The front-end/back-end flow-control contract: `qemu_chr_fe_write`/`write_all`
  may write fewer bytes than requested (non-blocking). Handle short writes;
  don't assume all bytes were consumed.
- Receive path: `IOCanReadHandler` must report only what the front-end can
  accept, and the `IOReadHandler` must not be handed more than that. Mismatch
  overruns the front-end buffer.
- Socket backends parse external bytes; bound any length/framing field and don't
  index past the received buffer.
- Lifecycle: `qemu_chr_fe_init`/`qemu_chr_fe_deinit` and handler
  (set/remove via `qemu_chr_fe_set_handlers`) must balance. A backend freed while
  a handler or watch is still registered is a UAF.
- Watches (`qemu_chr_fe_add_watch`) and reconnect timers must be removed on
  teardown.

## Common findings

- Short-write not handled (data loss / busy loop).
- `can_read` returning more than the front-end buffer holds → overrun.
- Handler/watch left registered after backend teardown → UAF.
- External socket input length not validated.

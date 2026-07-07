# GUI Windows and Dialogs

`virtManager/details/`, `createvm.py` (new-VM wizard), `addhardware.py`,
`vmwindow.py`, `manager.py`, and the Glade `.ui` files implement the GTK GUI.
Widgets are wired to handlers and to the `vmmGObject` lifecycle.

## Core invariants

- Handlers connected from `.ui`/Glade (`on_*`, `_signal_*`) run on the main
  thread. They may call into libvirt objects — long/blocking calls must be
  pushed to a `vmmAsyncJob` so the dialog doesn't freeze (see threading.md).
- `self.widget("name")` looks up a widget from the builder; a typo'd or
  renamed-in-`.ui` name returns None and the subsequent call crashes. Keep code
  and `.ui` names in sync when either changes.
- Building config from dialog fields: validate user input before constructing
  the `virtinst` object, and surface errors with the standard error dialog
  rather than raising. Empty/optional fields must map to "unset", not an empty
  string that becomes bad XML.
- Dialogs must reset state between uses (the same dialog instance is often
  reused); leftover state from a previous invocation is a common bug.
- `_cleanup()` must drop widget/object references and signal connections added
  by the dialog (see threading.md).

## Common findings

- `self.widget("...")` name out of sync with the `.ui` file (None → crash).
- Blocking libvirt call directly in a dialog handler (UI freeze).
- Dialog state not reset on reopen.
- User input turned into XML without validation / empty-vs-unset confusion.
- Signal/reference added without matching cleanup (leak; callback on dead
  widget).

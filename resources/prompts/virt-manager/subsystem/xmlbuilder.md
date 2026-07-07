# XML Builder

`virtinst/xmlbuilder.py` (+ `xmlapi.py`) is the heart of `virtinst`: `XMLBuilder`
subclasses declare `XMLProperty` / `XMLChildProperty` descriptors that map Python
attributes to libvirt XML via XPath. Every device and the `Guest` itself is an
`XMLBuilder`.

## Core invariants

- An `XMLProperty(xpath, ...)` binds a Python attribute to an XML location. The
  xpath must match the libvirt schema element/attribute exactly; a typo
  silently no-ops (value never lands in the XML). `is_bool`/`is_int`/`is_yesno`/
  `is_onoff` must match how libvirt represents the value. A boolean property
  (`is_yesno`/`is_onoff`) also needs the CLI arg that feeds it to pass
  `is_onoff=True` (see cli.md), or `on`/`yes` text reaches the property as a raw
  string and serializes verbatim.
- **Round-trip stability**: parsing an existing domain's XML and re-formatting it
  must not drop or reorder meaningful content. A new property must both parse
  from and format to XML; adding only a setter (or only the xpath) breaks
  edit-existing-VM flows (`virt-xml`).
- `XMLChildProperty` manages lists of child `XMLBuilder` objects (e.g. a Guest's
  devices). Adding/removing children must go through the child-property API so
  the underlying XML nodes are kept in sync; manipulating the DOM directly
  around it corrupts state.
- `set_defaults()` / `_add_parse_bits` hooks fill in implied values; values set
  there must be conditional so they don't override an explicit user setting.

## Validation and types

- A value assigned to a property should be the right Python type; the builder
  stringifies it into XML. Assigning `None` typically removes the node — make
  sure that's intended, not an accidental clear.
- Don't build XML by string concatenation alongside the builder; mixing the two
  representations leads to lost edits.

## Common findings

- xpath typo / wrong `is_*` flag so the property never round-trips correctly.
- New attribute added with format-only or parse-only support (breaks `virt-xml`
  edit of an existing guest).
- `set_defaults` overriding an explicitly-set value.
- Child node added outside the `XMLChildProperty` API (state desync).

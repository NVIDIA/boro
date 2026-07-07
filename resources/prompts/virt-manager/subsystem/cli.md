# CLI Parsing

`virtinst/cli.py` implements the option parsing for `virt-install`, `virt-xml`,
and `virt-clone`: the `Parser*`/`_VirtCLIArgument` machinery that turns
`--disk path=...,size=...`-style options into `virtinst` object properties.

## Core invariants

- Each CLI sub-option maps to an object attribute via the parser tables. A new
  `--foo` sub-option must be registered in the right parser, mapped to the
  matching `XMLProperty`-backed attribute, and documented in the man page; an
  unregistered or mis-mapped sub-option is silently ignored or errors obscurely.
- Option value parsing must handle the documented forms (comma-separated
  key=value, lists, `on/off`, sizes). Splitting/quoting bugs cause values to be
  mis-assigned (e.g. a path containing a comma).
- **Boolean sub-options need `is_onoff=True`.** When a sub-option feeds an
  attribute whose `XMLProperty` is `is_yesno`/`is_onoff`/`is_bool`, its
  `add_arg(...)` must pass `is_onoff=True` so `on`/`off`/`yes`/`no` text is
  normalized to a Python bool before the property serializes it. Omit it and the
  raw string is stored, then serialized verbatim — an invalid `iommufd="on"`
  instead of `iommufd="yes"` (only a literal `=yes`/`=no` happens to pass
  through). A new boolean arg that lacks `is_onoff=True` while an adjacent arg
  (e.g. `rom.bar`) sets it is the tell.
- `virt-xml` edits existing domains: a parser used for edit must support both
  setting and clearing a value, and must round-trip (see xmlbuilder.md), or
  editing drops unrelated config.
- Validation is largely deferred to `virtinst` object `validate()` and to
  libvirt at define time; cli.py should produce a clear error for malformed
  *option syntax*, but isn't expected to re-validate every semantic constraint.

## Backward compatibility

- Removing/renaming an option or sub-option, or changing a default, breaks
  existing scripts. Such changes need deprecation handling and a changelog note.

## Common findings

- New sub-option not wired to its object attribute (silently ignored).
- Boolean sub-option added without `is_onoff=True` though its property is
  `is_yesno`/`is_onoff` (raw `on`/`yes` string serialized into the XML).
- Comma/quote splitting mishandling a value containing the delimiter.
- `virt-xml` parser that can set but not clear a value, or doesn't round-trip.
- Backward-incompatible option/default change without deprecation.

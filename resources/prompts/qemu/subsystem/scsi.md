# SCSI

SCSI device emulation parses guest-supplied CDBs (command descriptor blocks) and
transfers data via the block layer. CDB fields and transfer lengths are
untrusted.

## Core invariants

- The CDB and its length fields come from the guest. Decode length/LBA from the
  CDB only after bounding them against the device capacity and the allocated
  buffer.
- `SCSIRequest` lifecycle: `scsi_req_new` → `scsi_req_enqueue` → data phase →
  `scsi_req_complete` → `scsi_req_unref`. Each `scsi_req_ref` needs a matching
  unref; an aborted/reset request must be cancelled and unref'd, not leaked.
- Sense data and INQUIRY/MODE responses are fixed-size buffers; writing a guest-
  derived length into them overflows. Bound to `sizeof`.
- Transfer length vs buffer length mismatch (`req->cmd.xfer`) is a frequent OOB
  source — verify they agree before `memcpy`/DMA.

## Common findings

- OOB write into a sense/inquiry buffer from an unchecked allocation length.
- LBA/length not bounded against device capacity.
- `SCSIRequest` leak or double-unref on cancel/reset paths.
- DMA buffer not unmapped on the error path.

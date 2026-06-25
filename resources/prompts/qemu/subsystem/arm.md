# ARM Machines and SoC Models (hw/arm/)

`hw/arm/` holds ARM board/machine and SoC integration code (e.g.
`hw/arm/virt.c`, `hw/arm/boot.c`, SoC files like `hw/arm/bcm2835_peripherals`).
This is **integration** code: it wires devices onto buses, lays out the guest
physical memory map, routes IRQs, and generates boot info / device trees. Bugs
here are usually wrong wiring, memory-map errors, and missing reset rather than
classic parsing OOB — though `hw/arm/boot.c` does parse guest-provided images.

## Core invariants

- **Memory map**: `memory_region_add_subregion` placements must not overlap
  unintentionally and must stay within the machine's address space. Region
  *sizes* must match the modeled device; an oversized MMIO region lets the guest
  reach unimplemented offsets. Check base + size arithmetic for overflow and for
  collisions between SoC blocks.
- **IRQ wiring**: `sysbus_connect_irq`, `qdev_connect_gpio_out`, and GIC SPI
  line numbers must be in range for the configured GIC (`num_irq`). An
  out-of-range SPI index or an off-by-one in the IRQ map is a real bug. Verify
  the IRQ count matches what the SoC instantiates.
- **Device tree (FDT)**: `hw/arm/boot.c` and machine `.fdt` builders generate
  DTB nodes. Properties (reg, interrupts, clocks, #address-cells) must match the
  actual memory map and IRQ wiring — a mismatch boots a broken guest. When the
  machine copies a user-provided DTB, sizes/offsets from that blob are untrusted.
- **Boot image loading**: kernel/initrd/DTB loading in `arm_load_kernel` uses
  guest-influenced sizes and load addresses; bound them against RAM size and
  check `load_image_*` return values. A load address + size that exceeds RAM, or
  an unchecked negative return, is a bug.
- **Reset**: every guest-visible register/state a board or SoC device adds must
  be restored in its `reset` handler and covered by VMState (see qdev.md,
  migration.md).
- **SoC realize ordering**: child controllers must be realized and their
  clocks/resets connected before use; a `realize` that wires an IRQ from a
  not-yet-realized child, or skips error checking on `qdev_realize`/
  `sysbus_realize`, leaves a half-built machine (see qdev.md `Error **errp`).

## Common findings

- MMIO region size larger/smaller than the device's register window.
- Overlapping or out-of-RAM memory-map placement; base+size overflow.
- GIC SPI / IRQ index out of range, or off-by-one in the IRQ map.
- FDT node disagreeing with the actual reg/interrupt wiring.
- Unchecked image-load return or load address exceeding RAM.
- Missing reset of board/SoC state; missing VMState for migratable state.

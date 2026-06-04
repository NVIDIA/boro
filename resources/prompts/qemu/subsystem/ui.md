# UI / Display

`ui/` renders guest framebuffers (`DisplaySurface`) and handles input. The guest
controls framebuffer geometry and contents; remote protocols (VNC) add untrusted
network input.

## Core invariants

- Surface geometry (width, height, stride, bpp) influences buffer-size math.
  The real backing size is `stride * height` (stride may exceed
  `width * bytes_per_pixel` due to padding) — validate that this doesn't
  overflow and that accesses fit the allocated surface before blitting.
  Guest-driven resizes are a classic OOB. Use the `surface_width()`/
  `surface_height()`/`surface_stride()` accessors rather than raw arithmetic.
- Update/dirty rectangles must be clipped to the surface bounds before copying;
  an unclipped rect from the guest or client overruns the buffer.
- VNC: client messages (`SetPixelFormat`, `FramebufferUpdateRequest`,
  rectangle encodings) are untrusted — bound all coordinates/lengths and check
  allocations.
- `DisplaySurface`/`pixman_image` lifetime: surfaces replaced on resize must be
  freed exactly once; callbacks holding the old surface across a resize are a
  UAF.

## Common findings

- Integer overflow in surface size / stride math.
- Unclipped blit rectangle → OOB read/write.
- VNC client-controlled coordinate or length not bounded.
- Stale `DisplaySurface` pointer used after a resize.

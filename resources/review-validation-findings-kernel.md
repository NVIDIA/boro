<!-- SPDX-License-Identifier: Apache-2.0 -->

Kernel-specific linkage rules for absence/export claims: check Kbuild/Makefile
ownership and aggregator `#include "*.c"` files for linkage claims.
`EXPORT_SYMBOL*()` is only required across a loadable-module boundary, not
between built-in objects or within one textual translation unit.

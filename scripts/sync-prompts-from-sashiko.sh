#!/usr/bin/env sh
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Copy (or refresh) the kernel prompt bundle from sashiko's upstream repo so this project
# stays aligned with third_party/prompts/kernel layout.
#
# Default: shallow-clone https://github.com/sashiko-dev/sashiko.git into a cache under
# ./.cache/sashiko-prompts-src (override with SASHIKO_CACHE), then rsync into third_party/prompts/kernel.
#
# Override: set SASHIKO=/path/to/local/checkout to skip git and copy from that tree instead.
set -eu
ROOT="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
DST="$ROOT/third_party/prompts/kernel"

if [ -n "${SASHIKO:-}" ]; then
  SRC="$SASHIKO/third_party/prompts/kernel"
else
  URL="${SASHIKO_URL:-https://github.com/sashiko-dev/sashiko.git}"
  CACHEDIR="${SASHIKO_CACHE:-$ROOT/.cache/sashiko-prompts-src}"
  mkdir -p "$(dirname "$CACHEDIR")"
  if [ -d "$CACHEDIR/.git" ]; then
    if ! git -C "$CACHEDIR" pull --ff-only; then
      echo "sync-prompts: git pull failed; removing cache and re-cloning" >&2
      rm -rf "$CACHEDIR"
      git clone --depth 1 "$URL" "$CACHEDIR"
    fi
  else
    git clone --depth 1 "$URL" "$CACHEDIR"
  fi
  SRC="$CACHEDIR/third_party/prompts/kernel"
fi

if [ ! -d "$SRC" ]; then
  echo "sync-prompts: missing directory: $SRC" >&2
  if [ -z "${SASHIKO:-}" ]; then
    echo "Clone or pull may have failed. Try removing SASHIKO_CACHE ($SASHIKO_CACHE) and re-run." >&2
  else
    echo "Set SASHIKO to a sashiko checkout containing third_party/prompts/kernel, or unset SASHIKO to use git." >&2
  fi
  exit 1
fi

mkdir -p "$DST"
rsync -a --delete "$SRC/" "$DST/"
echo "Synced prompts: $SRC -> $DST"

# third_party/prompts/kernel/ is Apache-2.0 (Sashiko). rsync --delete drops extra files; restore pointer each sync.
cat > "$DST/LICENSE.boro-notice" <<'EOF'
These Markdown and companion files are synced from the Sashiko project's kernel prompt tree.
They are licensed under the Apache License, Version 2.0.
Full text (boro repository root): ../../../LICENSE
Upstream: https://github.com/sashiko-dev/sashiko
EOF

python3 "$ROOT/scripts/update-subsystem-map-from-sashiko.py" \
  "$DST" \
  "$ROOT/src/prompts.rs"

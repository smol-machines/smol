#!/usr/bin/env bash
# Keep the engine release's disk templates beside the CLI executable.
set -euo pipefail

SOURCE_DIR="${1:?usage: copy-disk-templates.sh SOURCE DESTINATION}"
DESTINATION="${2:?usage: copy-disk-templates.sh SOURCE DESTINATION}"
templates=()
for name in storage-template.ext4 overlay-template.ext4; do
  if [ -f "$SOURCE_DIR/$name.zst" ] && [ -s "$SOURCE_DIR/$name.zst" ]; then
    templates+=("$SOURCE_DIR/$name.zst")
  elif [ -f "$SOURCE_DIR/$name" ] && [ -s "$SOURCE_DIR/$name" ]; then
    templates+=("$SOURCE_DIR/$name")
  else
    echo "error: engine release is missing nonempty $name or $name.zst" >&2
    exit 1
  fi
done

# Validate both before copying either, so a missing asset never yields a
# seemingly complete CLI that later needs host mkfs.ext4 to capture a machine.
mkdir -p "$DESTINATION"
for template in "${templates[@]}"; do
  cp "$template" "$DESTINATION/"
done

#!/usr/bin/env nix-shell
# shellcheck shell=bash disable=SC1008
#!nix-shell -i bash -p librsvg imagemagick
# Regenerates favicon.ico + apple-touch-icon.png from favicon.svg.
# Run after editing favicon.svg; commit the regenerated binaries.
set -euo pipefail

cd "$(dirname "$0")"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for size in 16 32 48; do
  rsvg-convert -w "$size" -h "$size" favicon.svg -o "$tmp/fav-$size.png"
done
magick "$tmp/fav-16.png" "$tmp/fav-32.png" "$tmp/fav-48.png" favicon.ico

rsvg-convert -w 180 -h 180 favicon.svg -o apple-touch-icon.png

echo "regenerated:"
ls -lh favicon.ico apple-touch-icon.png

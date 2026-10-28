#!/usr/bin/env bash
# Regenerate the PNG icon set under hicolor/ from the SVG sources (needs inkscape).
# mdread.svg is the full design (128 px grid), used at 32 px and up; mdread-small.svg
# is a simplified, pixel-aligned version (16 px grid) for the 16-24 px sizes, where
# the folded corner, text lines and arrow would otherwise blur into noise.
# ico/ holds the extra 20 and 40 px frames of the Windows icon; make_ico.py packs
# everything into mdread.ico.
set -euo pipefail
cd "$(dirname "$0")"
render() {
  mkdir -p "$(dirname "$3")"
  inkscape "$1" --export-type=png --export-width="$2" --export-height="$2" --export-filename="$3"
}
for n in 32 48 64 128 256 512; do render mdread.svg "$n" "hicolor/${n}x${n}/apps/mdread.png"; done
for n in 16 22 24; do render mdread-small.svg "$n" "hicolor/${n}x${n}/apps/mdread.png"; done
render mdread-small.svg 20 ico/20x20.png
render mdread.svg 40 ico/40x40.png
python3 make_ico.py

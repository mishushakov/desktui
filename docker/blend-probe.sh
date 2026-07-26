#!/bin/sh
# Does this terminal blend an RGBA placement over another placement?
#
# The cursor wants to be its own image above the tiles rather than pixels blended into
# them -- see RENDERING.md, "The cursor as a placement". That rests on two things the
# spec states and neither Ghostty nor kitty documents the practice of: at equal `z` the
# higher image id composites above, and overlapping semi-transparent placements blend.
#
# This draws a green square, then puts a half-transparent red square over its middle
# with a higher id. Run it and look:
#
#   green with a DARK YELLOW / OLIVE middle   -> blended. The cursor can be a placement.
#   green with a SOLID RED middle             -> not blended, drawn opaque.
#   green with a BLACK or RED-ON-BLACK middle -> composited against the cell background.
#   green with nothing over it                -> the second placement was dropped.
#
# The last three all mean the cursor stays blended into tiles, and the plan needs
# rethinking rather than writing.

set -eu

green=8000
red=8001

# 64x64 of opaque green, and 32x32 of red at half alpha, as raw RGBA.
python3 - <<'PY' >/tmp/blend-probe-green.b64
import base64, sys
px = bytes([0x20, 0xc0, 0x40, 0xff]) * (64 * 64)
sys.stdout.write(base64.standard_b64encode(px).decode())
PY

python3 - <<'PY' >/tmp/blend-probe-red.b64
import base64, sys
px = bytes([0xe0, 0x20, 0x20, 0x80]) * (32 * 32)
sys.stdout.write(base64.standard_b64encode(px).decode())
PY

printf '\n  blend probe: look at the square below, then read %s\n\n' "$0"

# Row 3 or so, well clear of the prompt, and eight rows of room under it.
printf '\033[3;3H'
printf '\033_Ga=T,q=2,C=1,z=-1,f=32,i=%s,p=1,s=64,v=64;%s\033\\' "$green" "$(cat /tmp/blend-probe-green.b64)"

# Same z, higher id, offset into the middle of the green one. X/Y are sub-cell offsets,
# so the cell move does the coarse positioning and X/Y the rest.
printf '\033[4;5H'
printf '\033_Ga=T,q=2,C=1,z=-1,f=32,i=%s,p=1,s=32,v=32,X=3,Y=4;%s\033\\' "$red" "$(cat /tmp/blend-probe-red.b64)"

printf '\033[12;1H'

# Leave the screen as it was found.
printf 'press enter to clear '
read -r _
printf '\033_Ga=d,d=I,i=%s,q=2\033\\' "$green"
printf '\033_Ga=d,d=I,i=%s,q=2\033\\' "$red"
rm -f /tmp/blend-probe-green.b64 /tmp/blend-probe-red.b64
printf '\033[2J\033[H'

#!/bin/sh
# Two questions the cursor-as-a-placement plan rests on, and neither Ghostty nor kitty
# documents the practice of. See RENDERING.md, "The cursor as a placement".
#
#   1. Does a semi-transparent placement blend over another placement?
#      The spec says overlapping semi-transparent placements blend, and that at equal `z`
#      the higher image id is on top. If it does not, a cursor drawn this way is a solid
#      or black rectangle instead of a pointer.
#
#   2. Do the `X`/`Y` sub-cell offsets work?
#      They are what lets a pointer sit between cells. If they are ignored, it can only
#      ever snap to a cell corner.
#
# Payloads are deliberately tiny: two pixels square, stretched over a block of cells with
# `c`/`r`, which is how the menu's own backdrop is drawn. A single graphics escape may
# carry at most 4096 bytes of base64 before it has to be chunked, and a probe that tripped
# over that would look exactly like a terminal that cannot blend.

set -eu

# Two pixels square of RGBA each: opaque green, red at half alpha, opaque white.
green=$(printf '\040\300\100\377\040\300\100\377\040\300\100\377\040\300\100\377' | base64 | tr -d '\n')
red=$(printf '\340\040\040\200\340\040\040\200\340\040\040\200\340\040\040\200' | base64 | tr -d '\n')
white=$(printf '\377\377\377\377\377\377\377\377\377\377\377\377\377\377\377\377' | base64 | tr -d '\n')

printf '\n  blend probe\n\n'

# ---------------------------------------------------------------- 1. blending
# A green block, then a half-transparent red one over its middle: same z, higher id.
printf '\033[4;3H'
printf '\033_Ga=T,q=2,C=1,z=-1,f=32,i=8000,p=1,s=2,v=2,c=24,r=8;%s\033\\' "$green"
printf '\033[6;9H'
printf '\033_Ga=T,q=2,C=1,z=-1,f=32,i=8001,p=1,s=2,v=2,c=12,r=4;%s\033\\' "$red"

# ---------------------------------------------------------------- 2. sub-cell offsets
# Two white blocks side by side on the green, the second nudged down by three pixels
# inside its first cell. If X/Y work they are staggered; if ignored they line up.
printf '\033[14;3H'
printf '\033_Ga=T,q=2,C=1,z=-1,f=32,i=8002,p=1,s=2,v=2,c=6,r=3;%s\033\\' "$white"
printf '\033[14;11H'
printf '\033_Ga=T,q=2,C=1,z=-1,f=32,i=8003,p=1,s=2,v=2,c=6,r=3,X=0,Y=6;%s\033\\' "$white"

printf '\033[19;1H'
cat <<'EOF'
  1. the middle of the green block, above:
       dark yellow / olive  -> blends. the cursor can be a placement.
       solid red            -> drawn opaque, no blending.
       black, or red on black -> composited against the cell background.
       nothing, still green  -> the second placement was dropped.

  2. the two white blocks, lower down:
       staggered, the right one a few pixels lower  -> X/Y offsets work.
       level with each other                       -> X/Y ignored.

EOF

printf '  press enter to clear '
# EOF rather than a keypress must not skip the cleanup below.
read -r _ || true
for id in 8000 8001 8002 8003; do
    printf '\033_Ga=d,d=I,i=%s,q=2\033\\' "$id"
done
printf '\033[2J\033[H'

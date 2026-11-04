#!/usr/bin/env bash
# Transmit a PNG through the kitty f=100 (PNG passthrough) path so the
# terminal's own decode_png does the work end to end. This is the path
# that once sheared RGB8 (color type 2) screenshots - grim, hyprshot and
# friends - into diagonal "interlaced" garbage, because it read a 3bpp
# decode at a 4bpp stride.
# Usage: png_passthrough.sh FILE [COLS] [ROWS]
set -e
F=$1
C=${2:-90}
R=${3:-40}
B64=$(base64 -w0 "$F")
LEN=${#B64}
i=0
first=1
while [ $i -lt $LEN ]; do
  chunk=${B64:$i:3072}
  more=0
  [ $((i + 3072)) -lt $LEN ] && more=1
  if [ $first = 1 ]; then
    printf '\033_Ga=T,f=100,c=%s,r=%s,m=%s;%s\033\\' "$C" "$R" "$more" "$chunk"
    first=0
  else
    # Continuation chunks repeat only m= (no a=; its default is T.
    # a=d would DELETE the image - a classic misread of the spec.)
    printf '\033_Gm=%s;%s\033\\' "$more" "$chunk"
  fi
  i=$((i + 3072))
done

#!/bin/bash
# The viewer rewinding, as a GIF: what is in the README.
#
#   tools/demo/rewind.sh <store> "<moment>" <look-for> [out.gif] ["caption for what was found"]
#   tools/demo/rewind.sh /var/lib/timeless-acct "2026-09-29 19:48:20" rustc docs/rewind.gif
#
# It drives the real viewer on a real store, in a tmux server of its own,
# and draws what it showed. Needs tmux, chromium, ffmpeg, and a font with
# box and block characters (it asks for CaskaydiaMono Nerd Font Mono).
# What is on the screen is the host's own: look before it is shared.
set -euo pipefail
store=$1; moment=$2; look_for=$3; out=${4:-rewind.gif}; found=${5:-}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
work="$repo/target/demo"
bin="$repo/target/release/timeless-acct"
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 1; }
rm -rf "$work/frames" "$work/html" "$work/png"
mkdir -p "$work/frames" "$work/html" "$work/png"

python3 "$here/record.py" --bin "$bin" --data "$store" --at "$moment" --look-for "$look_for" \
  --frames "$work/frames" ${found:+--found "$found"}

while IFS=$'\t' read -r name ms caption; do
  python3 "$here/render.py" "$work/frames/$name" "$work/html/${name%.ansi}.html" "$caption"
done < "$work/frames/manifest.tsv"

# A page has to be asked for by its whole path, or the picture is of an error.
ls "$work"/html/*.html | xargs -P 6 -I{} sh -c '
  chromium --headless --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
    --window-size=1066,672 --screenshot="$1/png/$(basename "$2" .html).png" "file://$2" >/dev/null 2>&1' sh "$work" {}

awk -F'\t' -v w="$work" '{n=$1; sub(/\.ansi$/, ".png", n)
  printf "file %s/png/%s\nduration %.3f\n", w, n, $2/1000; last=n}
  END{printf "file %s/png/%s\n", w, last}' "$work/frames/manifest.tsv" > "$work/list.txt"
ffmpeg -y -loglevel error -f concat -safe 0 -i "$work/list.txt" \
  -vf "split[a][b];[a]palettegen=max_colors=64:stats_mode=full[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" \
  -fps_mode vfr -loop 0 "$out"
ls -la "$out"

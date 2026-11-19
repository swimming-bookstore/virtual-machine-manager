#!/bin/sh
# Why Guest IP says "no ARP row". Run this on the Mac while the guest is up:
#   ./debug-arp.sh
set -eu
set -f
cd "$(dirname "$0")"
SUP="${HOME}/Library/Application Support/virtual-machine-manager"

# Compare the way the app does: lowercase, six octets, leading zeros. No awk.
norm_mac() {
  token=$(printf '%s' "$1" | tr 'A-F' 'a-f' | tr '-' ':')
  token=${token##*,}
  token=${token%% *}
  old_ifs=$IFS
  IFS=:
  # shellcheck disable=SC2086
  set -- $token
  IFS=$old_ifs
  if [ "$#" -eq 7 ]; then
    shift
  fi
  if [ "$#" -ne 6 ]; then
    return 1
  fi
  out=""
  for part in "$@"; do
    part=$(printf '%s' "$part" | tr -cd '0-9a-f')
    case "$part" in
      [0-9a-f]) part="0$part" ;;
      [0-9a-f][0-9a-f]) ;;
      *) return 1 ;;
    esac
    if [ -z "$out" ]; then
      out=$part
    else
      out="$out:$part"
    fi
  done
  printf '%s\n' "$out"
}
wanted() {
  for item in $macs; do
    seen=$(norm_mac "$item" || true)
    if [ -n "$seen" ] && [ "$seen" = "$1" ]; then
      return 0
    fi
  done
  return 1
}
# Four dotted numbers. Sets a b c d.
split4() {
  old_ifs=$IFS
  IFS=.
  # shellcheck disable=SC2086
  set -- $1
  IFS=$old_ifs
  if [ "$#" -ne 4 ]; then
    return 1
  fi
  a=$1 b=$2 c=$3 d=$4
  case "$a$b$c$d" in
    *[!0-9]*) return 1 ;;
  esac
  return 0
}
host_side() {
  split4 "$1" || return 1
  [ "$a" = 192 ] && [ "$b" = 168 ] && [ "$c" -ge 64 ] && { [ "$d" = 1 ] || [ "$d" = 2 ]; }
}
usable() {
  split4 "$1" || return 1
  [ "$a" -ne 0 ] && [ "$a" -ne 127 ] && [ "$a" -lt 224 ] && [ "$d" -ne 0 ] && [ "$d" -ne 1 ] && [ "$d" -ne 255 ]
}
# Count colon-separated pieces. 6 or 7 is a MAC token.
colon_count() {
  old_ifs=$IFS
  IFS=:
  # shellcheck disable=SC2086
  set -- $1
  IFS=$old_ifs
  echo "$#"
}

echo "=== mac files (what the app matches) ==="
macs=""
found=0
for f in "$SUP"/machines/*/mac "$SUP"/snapshots/debian/mac; do
  if [ -f "$f" ]; then
    found=1
    text=$(tr -d '[:space:]' < "$f")
    echo "$f: $text"
    # One line. macOS awk rejects a newline in -v ("newline in string").
    macs="${macs}${text} "
  fi
done
if [ "$found" -eq 0 ]; then
  echo "(none under $SUP)"
fi

ARPBIN=/usr/sbin/arp
if [ ! -x "$ARPBIN" ]; then
  ARPBIN=arp
fi

echo
echo "=== $ARPBIN -an ==="
arptext=$("$ARPBIN" -an 2>&1 || true)
printf '%s\n' "$arptext"

echo
echo "=== bridge addresses (.1 and .2 are this Mac) ==="
ifconfig -a 2>/dev/null | awk '
  !/^[ \t]/ { name = $1; sub(/:$/, "", name) }
  name ~ /^(bridge|vmnet|vmenet)/ && $1 == "inet" { print name, $2 }
' || true

echo
echo "=== why each ARP row is kept or dropped ==="
echo "match MACs (files only, not ARP):"
for item in $macs; do
  seen=$(norm_mac "$item" || true)
  echo "  $item -> ${seen:-unparsed}"
done
# Shell, not awk. macOS awk printed the match MACs and then no rows.
keep=0
rows=0
while IFS= read -r line || [ -n "$line" ]; do
  if [ -z "$line" ]; then
    continue
  fi
  rows=$((rows + 1))
  case "$line" in
    *incomplete*)
      echo "DROP  incomplete (no MAC yet)  $line"
      continue
      ;;
  esac
  echo "ROW  $line"
  host=${line#*(}
  host=${host%%)*}
  if [ "$host" = "$line" ]; then
    host=""
  fi
  mac=""
  for word in $line; do
    n=$(colon_count "$word")
    if [ "$n" -eq 6 ] || [ "$n" -eq 7 ]; then
      mac=$word
      break
    fi
  done
  mac=$(norm_mac "$mac" || true)
  if [ -z "$host" ] || [ -z "$mac" ]; then
    echo "DROP  unparsed  $line"
    continue
  fi
  if host_side "$host"; then
    echo "DROP  this Mac (.1 or .2)  $host  $mac"
    continue
  fi
  if ! usable "$host"; then
    echo "DROP  not a guest IP  $host  $mac"
    continue
  fi
  if wanted "$mac"; then
    keep=$((keep + 1))
    echo "KEEP  $host  $mac"
  else
    echo "DROP  MAC is not in a mac file  $host  $mac"
  fi
done <<EOF
$arptext
EOF
echo "rows: $rows  keep: $keep"
if [ "$keep" -eq 0 ]; then
  echo 'KEEP  (none) — this is the app log "no ARP row"'
fi

echo
echo "=== worker address (the call the app makes) ==="
WORKER=""
for candidate in \
  "./virtual-machine-manager.app/Contents/MacOS/virtual-machine-worker" \
  "./target/release/virtual-machine-worker" \
  "./target/debug/virtual-machine-worker"
do
  if [ -x "$candidate" ]; then
    WORKER=$candidate
    break
  fi
done
if [ -z "$WORKER" ]; then
  echo "no worker. Run: cargo build --release && ./build-macos.sh"
  exit 0
fi
echo "worker: $WORKER"
disks=0
for DISK in "$SUP"/machines/*/disk.img; do
  if [ ! -f "$DISK" ]; then
    continue
  fi
  disks=1
  MACFILE="$(dirname "$DISK")/mac"
  echo "-- $DISK"
  if [ -f "$MACFILE" ]; then
    MAC=$(tr -d '[:space:]' < "$MACFILE")
    echo "mac: $MAC"
    "$WORKER" address --disk "$DISK" --mac "$MAC" || true
  else
    echo "no mac file beside this disk"
    "$WORKER" address --disk "$DISK" || true
  fi
done
if [ "$disks" -eq 0 ]; then
  echo "no disk under $SUP/machines"
fi

#!/bin/sh
# Why Guest IP says "no ARP row". Run this on the Mac while the guest is up:
#   ./debug-arp.sh
set -eu
cd "$(dirname "$0")"
SUP="${HOME}/Library/Application Support/virtual-machine-manager"

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
"$ARPBIN" -an || true

echo
echo "=== bridge addresses (.1 and .2 are this Mac) ==="
ifconfig -a 2>/dev/null | awk '
  !/^[ \t]/ { name = $1; sub(/:$/, "", name) }
  name ~ /^(bridge|vmnet|vmenet)/ && $1 == "inet" { print name, $2 }
' || true

echo
echo "=== why each ARP row is kept or dropped ==="
"$ARPBIN" -an | awk -v macs="$macs" '
BEGIN {
  n = split(macs, line, " ")
  for (i = 1; i <= n; i++) {
    m = norm(line[i])
    if (m != "") print "match MAC  " m
  }
}
function hexval(s,    i, c, v, h) {
  h = "0123456789abcdef"
  v = 0
  for (i = 1; i <= length(s); i++) {
    c = index(h, substr(s, i, 1))
    if (c == 0) return -1
    v = v * 16 + c - 1
  }
  return v
}
function norm(token,    n, i, start, parts, v, piece, out) {
  token = tolower(token)
  n = split(token, parts, ":")
  if (n == 7) start = 2
  else if (n == 6) start = 1
  else return ""
  out = ""
  for (i = start; i <= n; i++) {
    gsub(/[^0-9a-f]/, "", parts[i])
    v = hexval(parts[i])
    if (v < 0 || v > 255 || length(parts[i]) == 0) return ""
    piece = sprintf("%02x", v)
    out = (out == "" ? piece : out ":" piece)
  }
  return out
}
function wanted(mac,    i, n, line) {
  n = split(macs, line, " ")
  for (i = 1; i <= n; i++) {
    if (mac != "" && norm(line[i]) == mac) return 1
  }
  return 0
}
function host_side(host,    n, p) {
  n = split(host, p, ".")
  if (n != 4) return 0
  if (p[1] != 192 || p[2] != 168) return 0
  if (p[3] + 0 < 64) return 0
  return (p[4] == 1 || p[4] == 2)
}
function usable(host,    n, p) {
  n = split(host, p, ".")
  if (n != 4) return 0
  if (p[1] + 0 == 0 || p[1] + 0 == 127 || p[1] + 0 >= 224) return 0
  if (p[4] + 0 == 0 || p[4] + 0 == 1 || p[4] + 0 == 255) return 0
  return 1
}
{
  if ($0 ~ /incomplete/) {
    print "DROP  incomplete (no MAC yet)  " $0
    next
  }
  host = ""
  if (match($0, /\([^)]+\)/)) host = substr($0, RSTART + 1, RLENGTH - 2)
  mac = ""
  for (i = 1; i <= NF; i++) {
    n = split($i, p, ":")
    if (n == 6 || n == 7) { mac = norm($i); break }
  }
  if (host == "" || mac == "") {
    print "DROP  unparsed  " $0
    next
  }
  if (host_side(host)) {
    print "DROP  this Mac (.1 or .2)  " host "  " mac
    next
  }
  if (!usable(host)) {
    print "DROP  not a guest IP  " host "  " mac
    next
  }
  if (wanted(mac)) {
    keep++
    print "KEEP  " host "  " mac
  } else {
    print "DROP  MAC is not in a mac file  " host "  " mac
  }
}
END {
  if (keep == 0) print "KEEP  (none) — this is the app log \"no ARP row\""
}
' || echo "awk failed; the worker check below still runs"

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

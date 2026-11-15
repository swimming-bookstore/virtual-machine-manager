#!/bin/sh
# Build the native Mac app. SwiftUI, no third-party UI library. Run this on macOS 14+, not Linux.
set -eu
cd "$(dirname "$0")"
if [ "$(uname)" != "Darwin" ]; then
  echo "virtual-machine-manager.app is a Mac app. Run this on macOS 14+." >&2
  exit 1
fi
if ! command -v swiftc >/dev/null 2>&1; then
  echo "swiftc not found. Install the Xcode command line tools." >&2
  exit 1
fi
export MACOSX_DEPLOYMENT_TARGET=14.0
APP="virtual-machine-manager.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp Info.plist "$APP/Contents/Info.plist"
swiftc -parse-as-library -O \
  -target "$(uname -m)-apple-macosx14.0" \
  -framework SwiftUI \
  -framework Virtualization \
  -o "$APP/Contents/MacOS/virtual-machine-manager" \
  Sources/App.swift Sources/Welcome.swift Sources/QuickStart.swift Sources/Guest.swift Sources/Install.swift
chmod +x "$APP/Contents/MacOS/virtual-machine-manager"
WORKER=""
for candidate in target/release/virtual-machine-worker target/debug/virtual-machine-worker; do
  if [ -x "$candidate" ]; then
    WORKER="$candidate"
    break
  fi
done
if [ -n "$WORKER" ]; then
  cp "$WORKER" "$APP/Contents/MacOS/virtual-machine-worker"
  chmod +x "$APP/Contents/MacOS/virtual-machine-worker"
else
  echo "No virtual-machine-worker binary. Quick start cannot download until: cargo build --release" >&2
fi
codesign --force --deep --sign - --entitlements virtual-machine-manager.entitlements "$APP"
echo "Built $APP. Open it with: open $APP"

#!/bin/sh
# Build target/SupvanPrinter.app: the Swift menu-bar shell plus the Rust
# service it supervises. No Xcode project — swiftc and a hand-made bundle.
#
#   macos/build-app.sh            # build
#   SUPVAN_SIGN_IDENTITY="Apple Development: …" macos/build-app.sh
#
# Signing: a real identity keeps the bundle's code requirement stable, so
# macOS asks for Bluetooth once rather than after every rebuild. Without one
# (or with SUPVAN_SIGN_IDENTITY=-) the bundle is ad-hoc signed, which works
# but re-prompts whenever the app changes.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
CARGO=${CARGO:-cargo}
APP="$ROOT/target/SupvanPrinter.app"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)

cd "$ROOT"
"$CARGO" build --release -p supvan-app

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
swiftc -O -o "$APP/Contents/MacOS/SupvanPrinter" macos/SupvanPrinter/main.swift
cp target/release/supvan-printer-app "$APP/Contents/MacOS/supvan-printer-app"
sed "s/@VERSION@/$VERSION/g" macos/SupvanPrinter/Info.plist > "$APP/Contents/Info.plist"

IDENTITY=${SUPVAN_SIGN_IDENTITY:-}
if [ -z "$IDENTITY" ]; then
    IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/^ *1) \([0-9A-F]\{40\}\) .*/\1/p')
fi
[ -n "$IDENTITY" ] || IDENTITY=-

sign() {
    codesign --force --sign "$1" "$APP/Contents/MacOS/supvan-printer-app" &&
        codesign --force --sign "$1" "$APP"
}
if ! sign "$IDENTITY" 2>/dev/null; then
    echo "signing with $IDENTITY failed; falling back to ad-hoc" >&2
    sign -
    IDENTITY=-
fi
if [ "$IDENTITY" = - ]; then echo "signed: ad-hoc"; else echo "signed: identity"; fi
echo "built $APP"

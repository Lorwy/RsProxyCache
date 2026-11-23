#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

check_link() {
    triple=$1
    sdk=$2
    slice=$3
    xcrun --sdk "$sdk" swiftc -target "$triple" -sdk "$(xcrun --sdk "$sdk" --show-sdk-path)" \
        -module-cache-path "$work/module-cache" \
        -F "$PWD/target/sdk/RsProxyCache.xcframework/$slice" -framework RsProxyCache \
        sdk/ios/link-check.swift -o "$work/$triple"
    printf 'Swift import and link OK (%s)\n' "$triple"
}
check_link arm64-apple-ios15.0 iphoneos ios-arm64
check_link arm64-apple-ios15.0-simulator iphonesimulator ios-arm64_x86_64-simulator
check_link x86_64-apple-ios15.0-simulator iphonesimulator ios-arm64_x86_64-simulator

#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
export CARGO_TARGET_DIR="$PWD/target"
export IPHONEOS_DEPLOYMENT_TARGET=15.0
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    cargo build --locked --release --lib --target "$target"
done
for platform in iphoneos iphonesimulator; do
    framework="$work/$platform/RsProxyCache.framework"
    mkdir -p "$framework/Headers" "$framework/Modules"
    cp sdk/ios/RsProxyCache.h "$framework/Headers/"
    cat > "$framework/Modules/module.modulemap" <<'MAP'
framework module RsProxyCache {
    umbrella header "RsProxyCache.h"
    export *
    link framework "Security"
    link framework "CoreFoundation"
}
MAP
    if [ "$platform" = iphoneos ]; then
        cp target/aarch64-apple-ios/release/librs_proxy_cache.a "$framework/RsProxyCache"
    else
        xcrun lipo -create target/aarch64-apple-ios-sim/release/librs_proxy_cache.a \
            target/x86_64-apple-ios/release/librs_proxy_cache.a -output "$framework/RsProxyCache"
    fi
    cat > "$framework/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>cn.gavinliu.rsproxycache</string>
<key>CFBundleExecutable</key><string>RsProxyCache</string>
<key>CFBundlePackageType</key><string>FMWK</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>
PLIST
done
xcodebuild -create-xcframework \
    -framework "$work/iphoneos/RsProxyCache.framework" \
    -framework "$work/iphonesimulator/RsProxyCache.framework" \
    -output "$work/RsProxyCache.xcframework"
mkdir -p target/sdk
ditto "$work/RsProxyCache.xcframework" target/sdk/RsProxyCache.xcframework

#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
: "${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to the Android NDK directory}"
toolchain=$(find "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt" -mindepth 1 -maxdepth 1 -type d | head -n 1)/bin
export CARGO_TARGET_DIR="$PWD/target"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for abi in arm64-v8a armeabi-v7a x86_64 x86; do
    case "$abi" in
        arm64-v8a) target=aarch64-linux-android; clang=$target ;;
        armeabi-v7a) target=armv7-linux-androideabi; clang=armv7a-linux-androideabi ;;
        x86_64) target=x86_64-linux-android; clang=$target ;;
        x86) target=i686-linux-android; clang=$target ;;
    esac
    cargo_key=$(printf '%s' "$target" | tr '[:lower:]-' '[:upper:]_')
    cc="$toolchain/${clang}24-clang"
    env "CARGO_TARGET_${cargo_key}_LINKER=$cc" "CC_$target=$cc" \
        "AR_$target=$toolchain/llvm-ar" cargo build --locked --release --lib --target "$target"
    mkdir -p "$work/jni/$abi"
    "$cc" -shared -Wl,--exclude-libs,ALL -Wl,-z,max-page-size=16384 -Wl,--no-undefined \
        sdk/android/bridge.c "target/$target/release/librs_proxy_cache.a" \
        -ldl -llog -lm -o "$work/jni/$abi/librsproxycache_jni.so"
    "$toolchain/llvm-strip" --strip-unneeded "$work/jni/$abi/librsproxycache_jni.so"
done
mkdir -p "$work/classes" target/sdk
javac --release 8 -d "$work/classes" sdk/android/src/cn/gavinliu/rsproxycache/ProxyCache.java
jar cf "$work/classes.jar" -C "$work/classes" .
cp sdk/android/AndroidManifest.xml "$work/"
printf '%s\n' '-keepclasseswithmembernames class cn.gavinliu.rsproxycache.ProxyCache { native <methods>; }' > "$work/proguard.txt"
touch "$work/R.txt"
output="$PWD/target/sdk/rs-proxy-cache.aar"
(cd "$work" && zip -qr rs-proxy-cache.aar AndroidManifest.xml classes.jar proguard.txt R.txt jni)
mv "$work/rs-proxy-cache.aar" "$output"

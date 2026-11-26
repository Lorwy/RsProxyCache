#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
sdk/android/gradlew -p sdk/android :rs-proxy-cache:assembleRelease
mkdir -p target/sdk
cp sdk/android/rs-proxy-cache/build/outputs/aar/rs-proxy-cache-release.aar target/sdk/rs-proxy-cache.aar

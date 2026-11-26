#!/bin/sh
set -eu
cd "$(dirname "$0")/.."

case "${1:-}" in
    "")
        export RPC_TEST_SKIP_FFMPEG=1
        export RPC_TEST_FULL=0
        ;;
    --full)
        if ! command -v ffmpeg >/dev/null 2>&1; then
            echo "Full verification requires ffmpeg on PATH." >&2
            exit 1
        fi
        export RPC_TEST_SKIP_FFMPEG=0
        export RPC_TEST_FULL=1
        ;;
    *)
        echo "Usage: $0 [--full]" >&2
        exit 2
        ;;
esac

cargo test --locked
cargo build --locked
target_dir=${CARGO_TARGET_DIR:-target}
case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) binary=rs-proxy-cache.exe ;;
    *) binary=rs-proxy-cache ;;
esac
RPC_TEST_BINARY="$target_dir/debug/$binary" python3 tests/playback.py -v

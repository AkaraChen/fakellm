#!/bin/sh
# Checks out the git submodules the embedded txiki.js build needs: third_party/txiki.js and, inside it, libuv, quickjs,
# mbedtls (with its `framework`) and libwebsockets, all shallow. The same commands tjs-sys/build.rs prints when they are missing.
# Run it from anywhere in the repository; it is idempotent.
set -eu
cd "$(dirname "$0")/.."
git submodule update --init --depth 1 third_party/txiki.js
git -C third_party/txiki.js submodule update --init --depth 1 deps/libuv deps/quickjs deps/mbedtls deps/libwebsockets
git -C third_party/txiki.js/deps/mbedtls submodule update --init --depth 1 framework

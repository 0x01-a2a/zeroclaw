#!/bin/bash
set -e

NDK_ROOT="$HOME/Library/Android/sdk/ndk/27.1.12297006"
NDK_BIN="$NDK_ROOT/toolchains/llvm/prebuilt/darwin-x86_64/bin"
TARGET="aarch64-linux-android"
API=21

export PATH="$NDK_BIN:/usr/bin:/bin:/usr/local/bin:$PATH"
export ANDROID_NDK_HOME="$NDK_ROOT"
export ANDROID_NDK_ROOT="$NDK_ROOT"
export CC_aarch64_linux_android="$NDK_BIN/aarch64-linux-android${API}-clang"
export CXX_aarch64_linux_android="$NDK_BIN/aarch64-linux-android${API}-clang++"
export AR_aarch64_linux_android="$NDK_BIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK_BIN/aarch64-linux-android${API}-clang"

exec "$HOME/.cargo/bin/cargo" build --release --target "$TARGET" --features channel-zerox1 "$@"

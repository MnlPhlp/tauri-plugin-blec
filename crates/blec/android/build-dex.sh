#!/usr/bin/env bash
# Builds the kotlin side (lib/src) into the single `classes.dex` that the
# `embedded-dex` feature of `blec` embeds.
#
# Needs an android SDK (ANDROID_HOME) and a JDK 17+. Run it after changing
# anything under `lib/src` and commit the result.
set -euo pipefail
cd "$(dirname "$0")"

GRADLE=${GRADLE:-./gradlew}
OUT=../src/android/classes.dex
APK=dex/build/outputs/apk/release/dex-release-unsigned.apk

"$GRADLE" --no-daemon :dex:assembleRelease

# `InMemoryDexClassLoader(ByteBuffer, ClassLoader)` takes exactly one dex, so a
# build that spilled into a second one has to be shrunk further instead.
if unzip -l "$APK" | grep -q 'classes2\.dex'; then
    echo "error: the build produced more than one dex file:" >&2
    unzip -l "$APK" | grep 'classes.*\.dex' >&2
    exit 1
fi

unzip -o -j "$APK" classes.dex -d "$(dirname "$OUT")"
echo "wrote $OUT ($(stat -c%s "$OUT") bytes)"

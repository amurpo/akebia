#!/bin/bash
# Builds the APK. One optional argument, the ABI:
#
#   ./packaging/build-android.sh              # arm64-v8a, what a telephone is
#   ./packaging/build-android.sh x86_64       # what the emulator is
#
# There is no Gradle here, and that is not stubbornness: `NativeActivity` is a
# class the system already provides, so the package carries no Java, no `.dex`
# and no resources. What is left —a manifest, a shared object and a signature—
# the SDK's own tools do in four commands, and they are the four below.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ABI="${1:-arm64-v8a}"

# API level 26 (Android 8.0) is not a preference: `libaaudio.so` only appears in
# the NDK's sysroot from 26 onwards, and `cpal` links it to reach the sound card.
# Below that the build fails at the very last step with `unable to find library
# -laaudio`, having compiled everything else first.
API=26

# The NDK is looked for where the SDK leaves it, so that the usual install needs
# no environment variable set by hand.
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    SDK="${ANDROID_HOME:-$HOME/Android}"
    # The newest, which is the one `sdkmanager` just installed. `sort -V` so that
    # 28.2 wins over 28.10 instead of losing to it alphabetically.
    ANDROID_NDK_HOME=$(find "$SDK/ndk" -maxdepth 1 -mindepth 1 -type d 2>/dev/null |
        sort -V | tail -1)
fi
if [ ! -d "${ANDROID_NDK_HOME:-}" ]; then
    echo "error: the NDK was not found" >&2
    echo "       \$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager 'ndk;28.2.13676358'" >&2
    echo "       or point ANDROID_NDK_HOME at it" >&2
    exit 1
fi
export ANDROID_NDK_HOME

if ! command -v cargo-ndk >/dev/null; then
    echo "error: cargo-ndk is needed to pick the NDK's linker" >&2
    echo "       cargo install cargo-ndk" >&2
    exit 1
fi

# The layout Gradle expects to find the libraries in, ready for the day the APK
# gets built: one folder per ABI, named after it.
OUT="$ROOT/target/android/jniLibs"

echo "==> Building akebia for $ABI (API $API)..."
cargo ndk -t "$ABI" -P "$API" -o "$OUT" \
    build --release --manifest-path "$ROOT/Cargo.toml" -p akebia-android

SO="$OUT/$ABI/libakebia_android.so"
"$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-nm -D --defined-only "$SO" |
    grep -q android_main ||
    { echo "error: the library came out without an android_main to call" >&2; exit 1; }

# ---- The APK ----------------------------------------------------------------

SDK="${ANDROID_HOME:-$HOME/Android}"
# The newest of each, which is what `sdkmanager` installs by default. The
# platform only supplies the `android.jar` the manifest is checked against; it
# does not have to match the level the application targets.
TOOLS=$(find "$SDK/build-tools" -maxdepth 1 -mindepth 1 -type d | sort -V | tail -1)
PLATFORM=$(find "$SDK/platforms" -maxdepth 1 -mindepth 1 -type d | sort -V | tail -1)
if [ ! -x "${TOOLS:-}/aapt2" ] || [ ! -f "${PLATFORM:-}/android.jar" ]; then
    echo "error: the SDK's build-tools or platforms are missing from $SDK" >&2
    echo "       sdkmanager 'build-tools;35.0.0' 'platforms;android-35'" >&2
    exit 1
fi

WORK="$ROOT/target/android"
APK="$WORK/akebia-$ABI.apk"

# The signing key, and it is deliberately nowhere near `target/`. Android will
# only update a package whose signature it has seen before, so losing the key
# means uninstalling to get the new build on the telephone — and uninstalling
# takes the saved games with it. `target/` is a directory that exists to be
# thrown away; a key that dies with it is a key that dies about once a month.
# Out of the repository too: it is a private key, throwaway or not.
KEYS="${XDG_DATA_HOME:-$HOME/.local/share}/akebia"
KEYSTORE="$KEYS/debug.keystore"

# The version lives in the binary's crate, the same place the RPM reads it from:
# the root Cargo.toml only declares the workspace and has no `version` field.
VER=$(grep -m1 '^version' "$ROOT/crates/akebia-frontend/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')

# The icon, in the two shapes Android wants: the flat one for whoever still
# reads it, and the layer that goes inside an adaptive icon for everybody else.
# The sizes are derived from the 1024 master instead of being kept in the
# repository, the same as the RPM's are: they are ten PNGs that would always
# have to be regenerated together.
if ! command -v magick >/dev/null; then
    echo "error: ImageMagick is needed to generate the icon sizes" >&2
    echo "       sudo dnf install ImageMagick" >&2
    exit 1
fi
echo "==> Drawing the icon..."
rm -rf "$WORK/res"
cp -r "$ROOT/packaging/android/res" "$WORK/res"
# density, launcher icon side (48dp), adaptive layer side (108dp).
for sizes in "mdpi 48 108" "hdpi 72 162" "xhdpi 96 216" "xxhdpi 144 324" "xxxhdpi 192 432"; do
    set -- $sizes
    mkdir -p "$WORK/res/mipmap-$1" "$WORK/res/drawable-$1"
    magick "$ROOT/data/icons/akebia.png" -resize $(($2 * 88 / 100))x \
        -background none -gravity center -extent "$2x$2" \
        "$WORK/res/mipmap-$1/ic_launcher.png"
    # 62 % of the canvas: a launcher's mask eats the outer sixth on every side,
    # and it may animate the layer further than that when the icon is touched.
    # Anything beyond this circle is not guaranteed to be seen at all.
    magick "$ROOT/data/icons/akebia.png" -resize $(($3 * 62 / 100))x \
        -background none -gravity center -extent "$3x$3" \
        "$WORK/res/drawable-$1/ic_launcher_foreground.png"
done
"$TOOLS/aapt2" compile --dir "$WORK/res" -o "$WORK/res.zip"

echo "==> Packaging..."
# The version does not go in the manifest so that it does not have to be kept in
# step with the crate's by hand. The code is what Android compares to decide
# whether one package updates another; the name is only ever read by a person.
"$TOOLS/aapt2" link \
    -I "$PLATFORM/android.jar" \
    --manifest "$ROOT/packaging/android/AndroidManifest.xml" \
    --min-sdk-version "$API" \
    --version-code 1 \
    --version-name "$VER" \
    -o "$WORK/unaligned.apk" \
    "$WORK/res.zip"

# The one Java class, which only exists so that the system's file picker has
# somewhere to answer. `--release 17` and not the JDK's own version: what d8
# accepts is the ceiling here, and it is far below whatever `javac` happens to
# be installed.
echo "==> Compiling the activity..."
rm -rf "$WORK/classes" "$WORK/dex"
mkdir -p "$WORK/classes" "$WORK/dex"
javac --release 17 -Xlint:-options \
    -classpath "$PLATFORM/android.jar" \
    -d "$WORK/classes" \
    "$ROOT"/packaging/android/java/org/akebia/*.java
"$TOOLS/d8" --lib "$PLATFORM/android.jar" --min-api "$API" \
    --output "$WORK/dex" "$WORK/classes"/org/akebia/*.class

# The library goes in at the path the loader looks it up by, one folder per ABI.
# `zip` is told to work from `jniLibs` so that the ABI is the first component and
# the `target/` path above it does not end up inside the package.
rm -rf "$WORK/lib" && mkdir -p "$WORK/lib"
cp -r "$OUT/$ABI" "$WORK/lib/"
(cd "$WORK" && zip -qr unaligned.apk lib && zip -qj unaligned.apk dex/classes.dex)

# Android maps the libraries straight out of the package, so they have to sit on
# a page boundary. 16 KiB and not 4: that is the page size of the newer devices,
# and aligning to the larger one keeps the smaller happy too.
"$TOOLS/zipalign" -f -P 16 4 "$WORK/unaligned.apk" "$WORK/aligned.apk"

# A throwaway key: it only has to prove that every version of the package comes
# from the same place.
mkdir -p "$KEYS"

# Where it used to be kept. Carried across rather than left to be wiped with the
# next `target/`: it is the key the package already on the telephone was signed
# with, and minting another one instead would turn every future build into a
# different application as far as Android is concerned.
if [ ! -f "$KEYSTORE" ] && [ -f "$WORK/debug.keystore" ]; then
    echo "==> Moving the debug key to $KEYS..."
    mv "$WORK/debug.keystore" "$KEYSTORE"
fi

if [ ! -f "$KEYSTORE" ]; then
    echo "==> Minting a debug key..."
    keytool -genkeypair -keystore "$KEYSTORE" -alias akebia \
        -keyalg RSA -keysize 2048 -validity 10000 \
        -storepass android -keypass android \
        -dname "CN=Akebia Debug, O=Akebia, C=UY" 2>/dev/null
fi

# `-J-enable-native-access` is handed to the JVM, not to apksigner: from Java 24
# on, loading a native library out of an unnamed module warns four lines deep on
# every run, and the signing has nothing to say that those lines should bury.
JVM=-J-enable-native-access=ALL-UNNAMED
"$TOOLS/apksigner" "$JVM" sign \
    --ks "$KEYSTORE" --ks-pass pass:android --key-pass pass:android \
    --out "$APK" "$WORK/aligned.apk"
"$TOOLS/apksigner" "$JVM" verify "$APK" >/dev/null
rm -f "$WORK/unaligned.apk" "$WORK/aligned.apk" "$APK.idsig"

echo
echo "==> Done: $APK  ($(du -h "$APK" | cut -f1))"
echo
echo "   Install over USB:  adb install -r $APK"
echo "   Or copy it to the telephone and open it from the file manager."
echo "   What it says for itself:  adb logcat -s akebia RustStdoutStderr"

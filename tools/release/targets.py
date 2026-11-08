"""The release targets: one native library per row, shared by the release
workflow, pin_hashes.py and check_sync.py.

name = Dart target name (lib/src/hook/native_source.dart `targetName`),
triple = Rust target, rid = .NET runtime id (None = not in the NuGet package),
required = must be in every release (desktop); mobile rows are best effort.
"""

TARGETS = [
    # name,            triple,                       rid,           required
    ("windows-x64",   "x86_64-pc-windows-msvc",     "win-x64",     True),
    ("windows-arm64", "aarch64-pc-windows-msvc",    "win-arm64",   True),
    ("linux-x64",     "x86_64-unknown-linux-gnu",   "linux-x64",   True),
    ("linux-arm64",   "aarch64-unknown-linux-gnu",  "linux-arm64", True),
    ("macos-x64",     "x86_64-apple-darwin",        "osx-x64",     True),
    ("macos-arm64",   "aarch64-apple-darwin",       "osx-arm64",   True),
    ("android-arm64", "aarch64-linux-android",      None,          False),
    ("android-x64",   "x86_64-linux-android",       None,          False),
    ("android-arm",   "armv7-linux-androideabi",    None,          False),
    ("ios-arm64",     "aarch64-apple-ios",          None,          False),
    ("ios_sim-arm64", "aarch64-apple-ios-sim",      None,          False),
    ("ios_sim-x64",   "x86_64-apple-ios",           None,          False),
]


def ext(name):
    if name.startswith("windows-"):
        return "dll"
    if name.startswith("macos-") or name.startswith("ios"):
        return "dylib"
    return "so"


def release_file(name):
    """The release asset name (= Dart `releaseFileName`)."""
    return f"seforim_search_native-{name}.{ext(name)}"


def built_file(name):
    """The file cargo writes for this target."""
    e = ext(name)
    return "seforim_search_native.dll" if e == "dll" else f"libseforim_search_native.{e}"

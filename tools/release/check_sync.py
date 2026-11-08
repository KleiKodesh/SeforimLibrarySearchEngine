"""Fails (exit 1) when the package versions, the C ABI version or the pinned
native hashes are out of sync.

    python -I -X utf8 tools/release/check_sync.py              # CI on every push / PR
    python -I -X utf8 tools/release/check_sync.py --tag v0.2.0 # release workflow: tag = version
    python -I -X utf8 tools/release/check_sync.py --pinned     # after pinning: hashes = this version

Checks:
- engine-ffi Cargo.toml, dart pubspec.yaml and the C# csproj carry ONE version;
- engine-ffi SE_ABI_VERSION == Dart seforimAbiVersion == C# NativeAbi.Expected;
- the pinned hashes file names a release tag no newer than the version, lists
  only known targets with 64-hex hashes, and (when it is this version's tag)
  every required target.
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from targets import TARGETS, release_file  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def read(rel):
    with open(os.path.join(ROOT, rel), encoding="utf-8") as f:
        return f.read()


def grab(rel, pattern):
    m = re.search(pattern, read(rel), re.M)
    if not m:
        sys.exit(f"check_sync: pattern {pattern!r} not found in {rel}")
    return m.group(1)


def semver(v):
    core = re.match(r"(\d+)\.(\d+)\.(\d+)", v)
    return tuple(int(x) for x in core.groups()) if core else None


def main():
    args = sys.argv[1:]
    errors = []
    versions = {
        "engine-ffi/Cargo.toml": grab("engine-ffi/Cargo.toml", r'^version\s*=\s*"([^"]+)"'),
        "dart/seforim_search/pubspec.yaml": grab("dart/seforim_search/pubspec.yaml", r"^version:\s*([^\s#]+)"),
        "csharp/SeforimSearch.Engine/SeforimSearch.Engine.csproj": grab(
            "csharp/SeforimSearch.Engine/SeforimSearch.Engine.csproj", r"<Version>([^<]+)</Version>"),
    }
    version = versions["engine-ffi/Cargo.toml"]
    if len(set(versions.values())) != 1:
        errors.append("package versions differ: " + ", ".join(f"{k}={v}" for k, v in versions.items()))

    abis = {
        "engine-ffi SE_ABI_VERSION": grab("engine-ffi/src/lib.rs", r"pub const SE_ABI_VERSION: u32 = (\d+);"),
        "Dart seforimAbiVersion": grab("dart/seforim_search/lib/src/native.dart", r"const int seforimAbiVersion = (\d+);"),
        "C# NativeAbi.Expected": grab("csharp/SeforimSearch.Engine/NativeAbi.cs", r"public const int Expected = (\d+);"),
    }
    if len(set(abis.values())) != 1:
        errors.append("ABI versions differ: " + ", ".join(f"{k}={v}" for k, v in abis.items()))

    hashes_src = read("dart/seforim_search/lib/src/hook/asset_hashes.g.dart")
    m = re.search(r"const String\? releaseTag = (null|'([^']*)');", hashes_src)
    if not m:
        errors.append("asset_hashes.g.dart: no releaseTag line")
        pinned_tag = None
    else:
        pinned_tag = m.group(2)
    pinned = dict(re.findall(r"'([^']+)': '([^']*)'", hashes_src))
    known = {release_file(n) for n, *_ in TARGETS}
    for name, h in pinned.items():
        if name not in known:
            errors.append(f"asset_hashes.g.dart: unknown asset {name}")
        if not re.fullmatch(r"[0-9a-f]{64}", h):
            errors.append(f"asset_hashes.g.dart: bad sha256 for {name}")
    if pinned_tag is None and pinned:
        errors.append("asset_hashes.g.dart: hashes without a releaseTag")
    if pinned_tag is not None:
        pv = semver(pinned_tag.lstrip("v"))
        if pv is None or pv > semver(version):
            errors.append(f"asset_hashes.g.dart pins {pinned_tag}, newer than the package version {version}")
        if pinned_tag == f"v{version}":
            missing = [release_file(n) for n, _, _, req in TARGETS if req and release_file(n) not in pinned]
            if missing:
                errors.append(f"asset_hashes.g.dart ({pinned_tag}) lacks required targets: " + ", ".join(missing))

    if "--tag" in args:
        tag = args[args.index("--tag") + 1]
        if tag != f"v{version}":
            errors.append(f"tag {tag} does not match the package version v{version}")
    if "--pinned" in args and pinned_tag != f"v{version}":
        errors.append(f"asset_hashes.g.dart pins {pinned_tag}, expected v{version} (run pin_hashes.py)")

    if errors:
        print("check_sync: OUT OF SYNC")
        for e in errors:
            print("  - " + e)
        sys.exit(1)
    print(f"check_sync: ok (version {version}, ABI {abis['engine-ffi SE_ABI_VERSION']}, pinned {pinned_tag})")


if __name__ == "__main__":
    main()

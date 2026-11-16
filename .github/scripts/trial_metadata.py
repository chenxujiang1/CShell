#!/usr/bin/env python3
"""Version and package integrity checks; standard library, Python >= 3.10."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess

PROGRAMS = ("cshell-gui", "cshelld", "cshell-script-worker")
PLATFORMS = ("windows-x64", "macos-arm64", "macos-x64")
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?")


def version_fields(version, build_number):
    match = SEMVER.fullmatch(version)
    if not match or any(part.isdigit() and len(part) > 1 and part[0] == "0" for part in (match[4] or "").split(".")):
        raise ValueError("Invalid software semantic version")
    numbers = [int(match[i]) for i in range(1, 4)]
    if any(value > 65535 for value in numbers) or type(build_number) is not int or not 1 <= build_number <= 65535:
        raise ValueError("Numeric version/build fields must fit the Windows version resource")
    base = ".".join(map(str, numbers))
    return {
        "software_version": version,
        "build_number": build_number,
        "channel": "development" if match[4] else "stable",
        "windows_version": numbers + [build_number],
        "windows_prerelease": bool(match[4]),
        "mac_short_version": base,
        "mac_bundle_version": str(build_number),
    }


def workspace(repository):
    data = json.loads(subprocess.check_output([
        "cargo", "metadata", "--no-deps", "--format-version", "1", "--locked", "--offline",
        "--manifest-path", str(repository / "Cargo.toml"),
    ], cwd=repository, stderr=subprocess.PIPE))
    packages = {p["name"]: p["version"] for p in data["packages"]}
    version = packages["cshell-gui"]
    own = {n: v for n, v in packages.items() if n.startswith("cshell-") or n in ("cshelld", "xtask")}
    if any(v != version for v in own.values()):
        raise ValueError("First-party workspace package versions disagree")
    return version_fields(version, data["metadata"]["cshell"]["build-number"])


def git(repository, *args):
    return subprocess.check_output(["git", "-C", str(repository), *args])


def source_identity(repository):
    commit = git(repository, "rev-parse", "HEAD").decode().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Missing source commit")
    dirty = bool(git(repository, "status", "--porcelain", "--untracked-files=normal"))
    digest = hashlib.sha256()
    names = sorted(set(git(repository, "ls-files", "--cached", "--others", "--exclude-standard", "-z").split(b"\0")) - {b""})
    for name in names:
        path = repository / os.fsdecode(name)
        if not path.resolve().is_relative_to(repository):
            raise ValueError("Source path leaves repository")
        if path.is_file():
            digest.update(name + b"\0" + hashlib.sha256(path.read_bytes()).digest())
    return {"source_commit": commit, "source_dirty": dirty, "source_fingerprint": digest.hexdigest()}


def shipped_names(platform):
    if platform not in PLATFORMS:
        raise ValueError("Unsupported trial platform")
    return ("CShell.exe", "cshelld.exe", "cshell-script-worker.exe") if platform == "windows-x64" else ("CShell", "cshelld", "cshell-script-worker")


def inspect_binaries(directory, platform, version, source_names=False):
    result = {}
    for program, shipped in zip(PROGRAMS, shipped_names(platform)):
        name = program + (".exe" if platform == "windows-x64" else "") if source_names else shipped
        binary = directory / name
        if not binary.is_file() or binary.is_symlink():
            raise ValueError("Missing or linked package executable: " + name)
        options = {"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}
        reply = subprocess.run([str(binary), "--version"], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, timeout=15, check=False, **options)
        if reply.returncode != 0 or reply.stdout.decode("utf-8").strip() != f"{program} {version}":
            raise ValueError("Stale or unexpected component version: " + name)
        result[shipped] = {"program": program, "bytes": binary.stat().st_size,
                           "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
    return result


def make_metadata(repository, directory, platform, source_names=False, require_clean=False):
    fields = workspace(repository)
    identity = source_identity(repository)
    if require_clean and identity["source_dirty"]:
        raise ValueError("CI packaging requires a clean source checkout")
    return {"format": "cshell.trial-build", "format_version": 1, "platform": platform,
            **fields, **identity,
            "binaries": inspect_binaries(directory, platform, fields["software_version"], source_names)}


def verify_plist(path, data):
    with path.open("rb") as source:
        plist = plistlib.load(source)
    expected = {
        "CFBundleShortVersionString": data["mac_short_version"],
        "CFBundleVersion": data["mac_bundle_version"],
        "CShellVersion": data["software_version"],
        "CShellSourceCommit": data["source_commit"],
        "CShellSourceDirty": data["source_dirty"],
    }
    if any(plist.get(key) != value for key, value in expected.items()):
        raise ValueError("Mac bundle version/source fields differ from metadata")


def verify_metadata(data, repository, directory, platform, require_clean=False, source_names=False):
    if data.get("format") != "cshell.trial-build" or data.get("format_version") != 1 or data.get("platform") != platform:
        raise ValueError("Unexpected package metadata format or platform")
    for key, value in workspace(repository).items():
        if data.get(key) != value:
            raise ValueError("Package version metadata differs from source: " + key)
    for key, value in source_identity(repository).items():
        if data.get(key) != value:
            raise ValueError("Package source identity differs from checkout: " + key)
    if require_clean and data["source_dirty"]:
        raise ValueError("Modified workspace cannot be distributed as a CI build")
    binaries = inspect_binaries(directory, platform, data["software_version"], source_names)
    if data.get("binaries") != binaries:
        raise ValueError("Package executable hashes differ from metadata")
    if platform.startswith("macos-") and not source_names:
        verify_plist(directory.parent / "Info.plist", data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("version", "check-binaries", "create", "verify"))
    parser.add_argument("--repository", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--field")
    parser.add_argument("--platform", choices=PLATFORMS)
    parser.add_argument("--binary-directory", type=Path)
    parser.add_argument("--metadata", type=Path)
    parser.add_argument("--source-names", action="store_true")
    parser.add_argument("--require-clean", action="store_true")
    args = parser.parse_args()
    repository = args.repository.resolve()
    if args.action == "version":
        fields = workspace(repository)
        print(fields[args.field] if args.field else json.dumps(fields, sort_keys=True))
        return
    if args.action == "check-binaries":
        if args.binary_directory is None or args.platform is None:
            parser.error("check-binaries requires platform and binary directory")
        inspect_binaries(args.binary_directory.resolve(), args.platform, workspace(repository)["software_version"], args.source_names)
        print("Trial component versions passed")
        return
    if args.binary_directory is None or args.metadata is None or args.platform is None:
        parser.error("create/verify require platform, binary directory and metadata path")
    directory = args.binary_directory.resolve()
    if args.action == "create":
        output = args.metadata.resolve()
        if not directory.is_relative_to(repository) or not output.is_relative_to(repository):
            raise ValueError("Package source and output paths must stay inside repository")
        data = make_metadata(repository, directory, args.platform, args.source_names, args.require_clean)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    else:
        verify_metadata(json.loads(args.metadata.read_text(encoding="utf-8")), repository, directory, args.platform, args.require_clean, args.source_names)
    print("Trial metadata " + args.action + " passed")


if __name__ == "__main__":
    main()

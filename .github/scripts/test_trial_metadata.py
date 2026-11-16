#!/usr/bin/env python3
"""Portable package contract tests, with no GUI/daemon or user data access."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("trial_metadata", Path(__file__).with_name("trial_metadata.py"))
metadata = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(metadata)


class VersionTests(unittest.TestCase):
    def test_development_and_stable_versions_use_numeric_platform_fields(self):
        dev = metadata.version_fields("1.1.0-dev.1", 11001)
        self.assertEqual(dev["windows_version"], [1, 1, 0, 11001])
        self.assertTrue(dev["windows_prerelease"])
        self.assertEqual(dev["mac_short_version"], "1.1.0")
        self.assertEqual(dev["mac_bundle_version"], "11001")
        stable = metadata.version_fields("1.1.0", 11002)
        self.assertFalse(stable["windows_prerelease"])
        self.assertGreater(int(stable["mac_bundle_version"]), int(dev["mac_bundle_version"]))

    def test_invalid_or_truncated_versions_cannot_be_packaged(self):
        for version in ["1.1", "01.1.0", "1.1.0-dev.01", "1.1.0-", '1.1.0"', "65536.0.0", "1.1.0 trailing"]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                metadata.version_fields(version, 11001)
        for build in [0, -1, 65536, True, "11001"]:
            with self.subTest(build=build), self.assertRaises(ValueError):
                metadata.version_fields("1.1.0-dev.1", build)


class PackageTests(unittest.TestCase):
    def setUp(self):
        # Reuse the project's target; no per-test Cargo target is created.
        parent = Path(__file__).resolve().parents[2] / "target/ssh-audit-fixtures"
        parent.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="metadata-", dir=parent)
        self.root = Path(self.temp.name)
        self.fields = metadata.version_fields("1.1.0-dev.1", 11001)
        self.identity = {"source_commit": "a" * 40, "source_dirty": False, "source_fingerprint": "b" * 64}
        self.binaries = {name: {"program": program, "bytes": 10, "sha256": str(i) * 64}
                         for i, (program, name) in enumerate(zip(metadata.PROGRAMS, metadata.shipped_names("windows-x64")))}
        self.data = {"format": "cshell.trial-build", "format_version": 1, "platform": "windows-x64",
                     **self.fields, **self.identity, "binaries": self.binaries}
        self.patches = [patch.object(metadata, "workspace", return_value=self.fields),
                        patch.object(metadata, "source_identity", return_value=self.identity),
                        patch.object(metadata, "inspect_binaries", return_value=self.binaries)]
        for item in self.patches:
            item.start()

    def tearDown(self):
        for item in reversed(self.patches):
            item.stop()
        self.temp.cleanup()

    def verify(self, data):
        metadata.verify_metadata(data, self.root, self.root, "windows-x64", require_clean=True)

    def test_valid_package_passes_and_each_identity_or_hash_mismatch_is_rejected(self):
        self.verify(self.data)
        for key, value in [("software_version", "1.0.0"), ("build_number", 11000),
                           ("source_commit", "c" * 40), ("source_fingerprint", "d" * 64),
                           ("source_dirty", True), ("platform", "macos-arm64")]:
            invalid = copy.deepcopy(self.data)
            invalid[key] = value
            with self.subTest(field=key), self.assertRaises(ValueError):
                self.verify(invalid)
        invalid = copy.deepcopy(self.data)
        invalid["binaries"]["cshelld.exe"]["sha256"] = "e" * 64
        with self.assertRaises(ValueError):
            self.verify(invalid)

    def test_ci_refuses_modified_sources_before_reading_executables(self):
        self.identity["source_dirty"] = True
        with self.assertRaises(ValueError):
            metadata.make_metadata(self.root, self.root, "windows-x64", require_clean=True)
        metadata.inspect_binaries.assert_not_called()

    def test_mac_plist_preserves_full_version_with_numeric_bundle_fields(self):
        data = {**self.fields, **self.identity}
        path = self.root / "Info.plist"
        value = {"CFBundleShortVersionString": "1.1.0", "CFBundleVersion": "11001",
                 "CShellVersion": "1.1.0-dev.1", "CShellSourceCommit": "a" * 40, "CShellSourceDirty": False}
        path.write_bytes(plistlib.dumps(value))
        metadata.verify_plist(path, data)
        value["CFBundleShortVersionString"] = "1.1.0-dev.1"
        path.write_bytes(plistlib.dumps(value))
        with self.assertRaises(ValueError):
            metadata.verify_plist(path, data)


class ComponentTests(unittest.TestCase):
    def test_version_probe_rejects_a_stale_companion_and_never_reads_stdin(self):
        import subprocess
        parent = Path(__file__).resolve().parents[2] / "target/ssh-audit-fixtures"
        parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="component-", dir=parent) as root:
            directory = Path(root)
            for program in metadata.PROGRAMS:
                (directory / (program + ".exe")).write_bytes(program.encode())
            def reply(args, **kwargs):
                self.assertEqual(args[1:], ["--version"])
                self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
                program = Path(args[0]).stem
                version = "1.0.0" if program == "cshelld" else "1.1.0-dev.1"
                return subprocess.CompletedProcess(args, 0, f"{program} {version}\n".encode(), b"")
            with patch("subprocess.run", side_effect=reply), self.assertRaisesRegex(ValueError, "cshelld"):
                metadata.inspect_binaries(directory, "windows-x64", "1.1.0-dev.1", source_names=True)


if __name__ == "__main__":
    unittest.main()

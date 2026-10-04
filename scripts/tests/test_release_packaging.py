"""Synthetic release contracts; not native Windows/Linux GUI acceptance."""

import hashlib
import importlib.util
import json
from pathlib import Path
import plistlib
import re
from types import SimpleNamespace
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("package_release", ROOT / "scripts/package-release.py")
PACKAGING = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGING)
IDENTITY = {"version": "0.6.0", "built_at_utc": "2026-10-04T10:43:00Z", "timestamp_source": "build-clock"}
COMMIT = "a" * 40


class PackageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="markrust-package-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.6.0"\n')
        (self.root / "LICENSE-MPL-2.0").write_text("Synthetic license fixture")
        (self.root / "assets/fonts").mkdir(parents=True)
        (self.root / "assets/fonts/LICENSE-OFL-Inter.txt").write_text("Synthetic font license")
        (self.root / "assets/icon").mkdir()
        (self.root / "assets/icon/icon.png").write_bytes(b"synthetic icon")
        self.binary = self.root / "synthetic-binary"
        self.binary.write_bytes(b"synthetic executable")
        self.output = self.root / "packages"

    def build(self, target, **arguments):
        with patch.object(PACKAGING, "validate_native_target"), \
             patch.object(PACKAGING, "compiled_identity", return_value=IDENTITY), \
             patch.object(PACKAGING.subprocess, "run", return_value=SimpleNamespace(stdout="libc.so => /lib/libc.so\n")):
            return PACKAGING.package(self.root, self.binary, self.output, target, COMMIT, **arguments)

    def test_linux_archive_cli_identity_licenses_and_checksum(self):
        archive = self.build("x86_64-unknown-linux-gnu", tag="v0.6.0")
        with tarfile.open(archive) as package:
            for name in ["markrust", "markrust.desktop", "LICENSE-MPL-2.0", "LICENSE-OFL-Inter.txt", "RUNTIME-LIBRARIES.txt"]:
                self.assertIn(name, package.getnames())
            metadata = json.load(package.extractfile("BUILD-INFO.json"))
            self.assertEqual(metadata["source_commit"], COMMIT)
            self.assertEqual(metadata["source_tag"], "v0.6.0")
            self.assertEqual(metadata["built_at_utc"], IDENTITY["built_at_utc"])
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.assertEqual(Path(f"{archive}.sha256").read_text(), f"{digest}  {archive.name}\n")

    def test_windows_archive_is_explicitly_experimental(self):
        archive = self.build("x86_64-pc-windows-msvc")
        self.assertEqual(archive.name, "markrust-windows-x86_64-experimental.zip")
        with zipfile.ZipFile(archive) as package:
            self.assertIn("markrust.exe", package.namelist())
            self.assertIn("ACL/locking QA", package.read("README.txt").decode())

    def test_macos_archive_keeps_app_and_cli(self):
        def bundle(stage, _root, _identity):
            (stage / "MarkRust.app/Contents/MacOS").mkdir(parents=True)
            (stage / "MarkRust.app/Contents/MacOS/markrust").write_bytes(b"synthetic app")
        with patch.object(PACKAGING, "assemble_macos_bundle", side_effect=bundle):
            archive = self.build("aarch64-apple-darwin")
        with tarfile.open(archive) as package:
            self.assertIn("MarkRust.app/Contents/MacOS/markrust", package.getnames())
            self.assertIn("markrust", package.getnames())
            self.assertIn("NOT Developer-ID", package.extractfile("README.txt").read().decode())

    def test_macos_prerelease_plist_has_numeric_versions_and_full_identity(self):
        stage = self.root / "prerelease-bundle"
        stage.mkdir()
        (stage / "markrust").write_bytes(b"synthetic executable")
        identity = {**IDENTITY, "version": "0.7.0-beta.1"}
        # Only metadata is under test; native icon/signing tools are covered by
        # the separate real-host package smoke, never claimed by this mock.
        with patch.object(PACKAGING.subprocess, "run"):
            PACKAGING.assemble_macos_bundle(stage, self.root, identity)
        metadata = plistlib.loads((stage / "MarkRust.app/Contents/Info.plist").read_bytes())
        self.assertEqual(metadata["CFBundleShortVersionString"], "0.7.0")
        self.assertEqual(metadata["CFBundleVersion"], "0.7.0")
        self.assertEqual(metadata["MarkRustFullVersion"], "0.7.0-beta.1")
        self.assertEqual(metadata["MarkRustBuildDate"], IDENTITY["built_at_utc"])

    def test_existing_archive_is_not_overwritten(self):
        archive = self.build("x86_64-unknown-linux-gnu")
        contents = archive.read_bytes()
        with self.assertRaisesRegex(ValueError, "overwrite"):
            self.build("x86_64-unknown-linux-gnu")
        self.assertEqual(archive.read_bytes(), contents)

    def test_invalid_commit_or_tag_never_executes_binary(self):
        with patch.object(PACKAGING, "validate_native_target"), patch.object(PACKAGING, "compiled_identity") as smoke:
            with self.assertRaisesRegex(ValueError, "SHA-1"):
                PACKAGING.package(self.root, self.binary, self.output, "x86_64-pc-windows-msvc", "main")
            with self.assertRaisesRegex(ValueError, "workspace version"):
                PACKAGING.package(self.root, self.binary, self.output, "x86_64-pc-windows-msvc", COMMIT, "v99.0.0")
            smoke.assert_not_called()

    def test_missing_linux_shared_library_rejects_archive(self):
        with patch.object(PACKAGING, "validate_native_target"), \
             patch.object(PACKAGING, "compiled_identity", return_value=IDENTITY), \
             patch.object(PACKAGING.subprocess, "run", return_value=SimpleNamespace(stdout="libexample.so => not found\n")):
            with self.assertRaisesRegex(ValueError, "unresolved"):
                PACKAGING.package(self.root, self.binary, self.output, "x86_64-unknown-linux-gnu", COMMIT)
        self.assertFalse(list(self.output.glob("*.tar.gz")))


class IdentityTests(unittest.TestCase):
    def test_macos_numeric_version_core_is_validated(self):
        self.assertEqual(PACKAGING.macos_version_fields("0.6.0")["CFBundleVersion"], "0.6.0")
        with self.assertRaisesRegex(ValueError, "numeric"):
            PACKAGING.macos_version_fields("not-a-version")

    def test_release_tag_is_exact_and_safe(self):
        PACKAGING.validate_tag("v0.6.0", "0.6.0")
        PACKAGING.validate_tag("v0.7.0-beta.1", "0.7.0-beta.1")
        for tag in ["v0.6.0/branch", "v0.6.0;echo unsafe", "v0.6", "0.6.0", "v0.6.0+build"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                PACKAGING.validate_tag(tag, "0.6.0")

    def test_native_architecture_is_required(self):
        with patch.object(PACKAGING.platform, "system", return_value="Windows"), \
             patch.object(PACKAGING.platform, "machine", return_value="AMD64"):
            PACKAGING.validate_native_target("x86_64-pc-windows-msvc")
            with self.assertRaisesRegex(ValueError, "native runner"):
                PACKAGING.validate_native_target("aarch64-apple-darwin")

    def test_version_and_build_time_are_compiled_identity(self):
        with patch.object(PACKAGING.subprocess, "run", side_effect=[SimpleNamespace(stdout="markrust 0.6.0\n"), SimpleNamespace(stdout=json.dumps(IDENTITY))]):
            self.assertEqual(PACKAGING.compiled_identity(Path("synthetic"), "0.6.0"), IDENTITY)

    def test_stale_binary_is_rejected(self):
        with patch.object(PACKAGING.subprocess, "run", return_value=SimpleNamespace(stdout="markrust 0.5.0\n")):
            with self.assertRaisesRegex(ValueError, "binary version"):
                PACKAGING.compiled_identity(Path("synthetic"), "0.6.0")

    def test_missing_timestamp_is_rejected(self):
        with patch.object(PACKAGING.subprocess, "run", side_effect=[SimpleNamespace(stdout="markrust 0.6.0\n"), SimpleNamespace(stdout='{"version":"0.6.0"}')]):
            with self.assertRaisesRegex(ValueError, "timestamp"):
                PACKAGING.compiled_identity(Path("synthetic"), "0.6.0")


class WorkflowTests(unittest.TestCase):
    def workflow(self, name):
        return (ROOT / ".github/workflows" / name).read_text()

    def test_actions_are_pinned_without_privileged_untrusted_context(self):
        for name in ["ci.yml", "release.yml", "build-binaries.yml"]:
            content = self.workflow(name)
            for action in re.findall(r"uses:\s+([^\s#]+)", content):
                if not action.startswith("./"):
                    self.assertRegex(action, r"^[\w.-]+/[\w.-]+@[a-f0-9]{40}$", name)
            self.assertNotIn("pull_request_target", content)
            self.assertNotIn("secrets: inherit", content)

    def test_push_artifacts_and_tag_release_are_distinct(self):
        self.assertIn("branches: ['**']", self.workflow("ci.yml"))
        release = self.workflow("release.yml")
        self.assertIn("tags: ['v*']", release)
        self.assertIn("--check-tag", release)
        self.assertEqual(release.count("contents: write"), 1)
        publisher = release.split("  release:\n", 1)[1]
        self.assertNotIn("actions/checkout@", publisher)
        self.assertIn("sha256sum -c", publisher)
        self.assertIn("gh release create", publisher)
        self.assertIn("--verify-tag", publisher)
        self.assertNotIn("gh release upload", publisher)
        self.assertIn("markrust-windows-x86_64-experimental.zip", publisher)

    def test_native_matrix_has_real_platform_prerequisites(self):
        build = self.workflow("build-binaries.yml")
        for target in PACKAGING.TARGETS:
            self.assertIn(f"target: {target}", build)
        for prerequisite in ["macos-15-intel", "windows-2022", "GPUI_FXC_PATH", "xcrun --find metal", "if-no-files-found: error"]:
            self.assertIn(prerequisite, build)
        self.assertNotIn("continue-on-error", build)


if __name__ == "__main__":
    unittest.main()

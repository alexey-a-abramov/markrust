#!/usr/bin/env python3
"""Package an already-built native MarkRust executable; never install or publish."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile


TARGETS = {
    "aarch64-apple-darwin": ("Darwin", "arm64", "markrust-macos-aarch64", ".tar.gz"),
    "x86_64-apple-darwin": ("Darwin", "x86_64", "markrust-macos-x86_64", ".tar.gz"),
    "x86_64-unknown-linux-gnu": ("Linux", "x86_64", "markrust-linux-x86_64", ".tar.gz"),
    "x86_64-pc-windows-msvc": ("Windows", "x86_64", "markrust-windows-x86_64-experimental", ".zip"),
}


def workspace_version(root):
    with (root / "Cargo.toml").open("rb") as source:
        return tomllib.load(source)["workspace"]["package"]["version"]


def validate_tag(tag, version):
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", tag):
        raise ValueError("release tag must be vMAJOR.MINOR.PATCH, optionally with a prerelease")
    if tag != f"v{version}":
        raise ValueError(f"tag {tag!r} does not match workspace version v{version}")


def validate_native_target(target):
    system, architecture, _, _ = TARGETS[target]
    machine = platform.machine().lower()
    normalized = {"amd64": "x86_64", "aarch64": "arm64"}.get(machine, machine)
    if (platform.system(), normalized) != (system, architecture):
        raise ValueError(f"{target} must be packaged and smoke-tested on its native runner")


def compiled_identity(binary, expected_version):
    version = subprocess.run(
        [str(binary), "--version"], check=True, capture_output=True, text=True, timeout=30
    ).stdout.strip()
    if version != f"markrust {expected_version}":
        raise ValueError(f"binary version does not match workspace: {version!r}")
    identity = json.loads(subprocess.run(
        [str(binary), "--build-info"], check=True, capture_output=True, text=True, timeout=30
    ).stdout)
    if identity.get("version") != expected_version:
        raise ValueError("compiled build identity does not match workspace version")
    if not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", identity.get("built_at_utc", "")):
        raise ValueError("compiled build timestamp must be seconds-precision UTC")
    return identity


def macos_version_fields(version):
    # Apple's version keys are numeric, even when Cargo/GitHub use a prerelease.
    numeric = version.split("-", 1)[0]
    if not re.fullmatch(r"\d+\.\d+\.\d+", numeric):
        raise ValueError("macOS bundle version must have a numeric MAJOR.MINOR.PATCH core")
    return {
        "CFBundleShortVersionString": numeric,
        "CFBundleVersion": numeric,
        "MarkRustFullVersion": version,
    }


def assemble_macos_bundle(stage, root, identity):
    app = stage / "MarkRust.app"
    resources = app / "Contents" / "Resources"
    executable = app / "Contents" / "MacOS" / "markrust"
    executable.parent.mkdir(parents=True)
    resources.mkdir(parents=True)
    shutil.copy2(stage / "markrust", executable)
    plist = {
        "CFBundleName": "MarkRust", "CFBundleDisplayName": "MarkRust",
        "CFBundleIdentifier": "com.alexeyabramov.markrust",
        "CFBundleExecutable": "markrust", "CFBundleIconFile": "MarkRust",
        **macos_version_fields(identity["version"]),
        "MarkRustBuildDate": identity["built_at_utc"],
        "CFBundlePackageType": "APPL", "CFBundleInfoDictionaryVersion": "6.0",
        "LSMinimumSystemVersion": "11.0",
        "LSApplicationCategoryType": "public.app-category.productivity",
        "NSHighResolutionCapable": True, "NSSupportsAutomaticGraphicsSwitching": True,
        "NSHumanReadableCopyright": "Alexey Abramov — MPL-2.0",
        "CFBundleDocumentTypes": [{
            "CFBundleTypeName": "Markdown document", "CFBundleTypeRole": "Editor",
            "LSItemContentTypes": ["net.daringfireball.markdown"],
        }],
    }
    with (app / "Contents" / "Info.plist").open("wb") as output:
        plistlib.dump(plist, output)
    (app / "Contents" / "PkgInfo").write_bytes(b"APPL????")
    iconset = stage / "MarkRust.iconset"
    iconset.mkdir()
    icon = root / "assets" / "icon" / "icon.png"
    for pixels, label in [(16, "16x16"), (32, "16x16@2x"), (32, "32x32"),
                          (64, "32x32@2x"), (128, "128x128"), (256, "128x128@2x"),
                          (256, "256x256"), (512, "256x256@2x"), (512, "512x512"),
                          (1024, "512x512@2x")]:
        subprocess.run(["sips", "-z", str(pixels), str(pixels), str(icon), "--out",
                        str(iconset / f"icon_{label}.png")], check=True, capture_output=True)
    subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o",
                    str(resources / "MarkRust.icns")], check=True)
    shutil.rmtree(iconset)  # Only the task-owned temporary iconset, never an installation.
    subprocess.run(["codesign", "--force", "--sign", "-", "--timestamp=none", str(app)], check=True)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
    subprocess.run(["plutil", "-lint", str(app / "Contents" / "Info.plist")], check=True)


def write_archive(stage, archive):
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED) as output:
            for path in sorted(stage.rglob("*")):
                if path.is_file():
                    output.write(path, path.relative_to(stage).as_posix())
    else:
        with tarfile.open(archive, "x:gz") as output:
            for path in sorted(stage.iterdir()):
                output.add(path, arcname=path.name)


def package(root, binary, output, target, commit, tag=None):
    validate_native_target(target)
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("source commit must be a full lowercase Git SHA-1")
    version = workspace_version(root)
    if tag:
        validate_tag(tag, version)
    if binary.is_symlink() or not binary.is_file():
        raise ValueError("binary must be a regular file, not a symbolic link")
    _, _, name, extension = TARGETS[target]
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"{name}{extension}"
    checksum = output / f"{archive.name}.sha256"
    if archive.exists() or checksum.exists():
        raise ValueError("refusing to overwrite an existing release package")
    with tempfile.TemporaryDirectory(prefix="markrust-package-") as temporary:
        stage = Path(temporary)
        staged_binary = stage / ("markrust.exe" if target.endswith("windows-msvc") else "markrust")
        shutil.copy2(binary, staged_binary)
        identity = compiled_identity(staged_binary, version)
        manifest = {
            **identity, "target": target, "source_commit": commit,
            "source_tag": tag, "signing": "ad-hoc; not notarized" if "apple" in target else "unsigned",
            "gui_qa": "macOS regression contracts; Windows/Linux native GUI manual QA pending",
        }
        (stage / "BUILD-INFO.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        shutil.copy2(root / "LICENSE-MPL-2.0", stage)
        shutil.copy2(root / "assets/fonts/LICENSE-OFL-Inter.txt", stage)
        if "apple" in target:
            assemble_macos_bundle(stage, root, identity)
            instructions = "Copy MarkRust.app to Applications. The top-level markrust is the CLI executable.\nThis build is ad-hoc signed, NOT Developer-ID signed or notarized; Gatekeeper may block it.\n"
        elif "windows" in target:
            instructions = "Run markrust.exe. This experimental build is not Authenticode signed.\nWindows 10/11 x64 and Microsoft Visual C++ 2015–2022 x64 runtime may be required.\nNative editor, clipboard, IME and recovery directory ACL/locking QA are still pending.\n"
        else:
            shutil.copy2(root / "assets/icon/icon.png", stage / "markrust.png")
            (stage / "markrust.desktop").write_text(
                "[Desktop Entry]\nType=Application\nName=MarkRust\nExec=markrust %F\nIcon=markrust\n"
                "Terminal=false\nCategories=Office;TextEditor;\nMimeType=text/markdown;text/plain;\n",
                encoding="utf-8",
            )
            dependencies = subprocess.run(["ldd", str(staged_binary)], check=True, capture_output=True, text=True).stdout
            if "not found" in dependencies:
                raise ValueError("Linux package has unresolved shared-library dependencies")
            (stage / "RUNTIME-LIBRARIES.txt").write_text(dependencies, encoding="utf-8")
            instructions = "Run ./markrust on Linux x86_64 with an X11/Wayland desktop and Vulkan-capable graphics.\nBuilt on Ubuntu 22.04; see RUNTIME-LIBRARIES.txt for dynamically linked system libraries.\nThis is not a self-contained AppImage; native GUI/clipboard/IME QA is still pending.\n"
        (stage / "README.txt").write_text(
            f"MarkRust {version}\nSource commit: {commit}\nBuild: {identity['built_at_utc']}\n\n"
            + instructions + "\nDevelopment artifacts are not a stable release or an automatic update.\n"
            "Documentation: https://github.com/alexey-a-abramov/markrust/blob/main/docs/deployment.md\n",
            encoding="utf-8",
        )
        write_archive(stage, archive)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    print(f"Packaged {archive.name}: {digest}")
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path, default=Path("dist"))
    parser.add_argument("--commit", default=os.environ.get("GITHUB_SHA"))
    parser.add_argument("--tag", default=os.environ.get("MARKRUST_RELEASE_TAG"))
    parser.add_argument("--check-tag", action="store_true", help="validate release tag without building or packaging")
    arguments = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    try:
        if arguments.check_tag:
            validate_tag(arguments.tag or "", workspace_version(root))
            print(f"Validated {arguments.tag}")
        else:
            if not arguments.target or not arguments.binary or not arguments.commit:
                parser.error("--target, --binary and --commit are required for packaging")
            package(root, arguments.binary.absolute(), arguments.output.absolute(),
                    arguments.target, arguments.commit, arguments.tag)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    main()

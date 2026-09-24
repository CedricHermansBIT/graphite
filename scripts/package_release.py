#!/usr/bin/env python3
"""Package a binary with notices, or matching sources with offline dependencies."""
import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def run(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True)


def dependency_notices(destination):
    metadata = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1"))
    inventory = []
    for package in metadata["packages"]:
        if package["source"] is None:
            continue
        name = f"{package['name']}-{package['version']}"
        source = Path(package["manifest_path"]).parent
        licenses = []
        for file in source.rglob("*"):
            if file.is_file() and (any(word in file.name.lower() for word in ("license", "licence", "copying", "copyright", "notice")) or file.name in {"OFL.txt", "UFL.txt", "Hack-Regular.txt"}):
                relative = file.relative_to(source)
                target = destination / "third-party-licenses" / name / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(file, target)
                licenses.append(str(relative))
        inventory.append({"name": package["name"], "version": package["version"],
                          "license": package["license"], "repository": package["repository"],
                          "license_files": licenses})
    (destination / "dependency-licenses.json").write_text(
        json.dumps({"scope": "Cargo dependency graph across supported targets", "packages": inventory}, indent=2) + "\n",
        encoding="utf-8",
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--platform", default=f"{platform.system().lower()}-{platform.machine()}")
    parser.add_argument("--source", action="store_true", help="include locked dependency sources for offline builds")
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    if not args.source and not args.binary:
        parser.error("choose --binary PATH or --source")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    ref = os.environ.get("GITHUB_REF", "")
    if ref.startswith("refs/tags/v") and ref.removeprefix("refs/tags/v") != version:
        parser.error(f"release tag {ref} does not match Cargo version {version}")
    label = "source" if args.source else args.platform
    name = f"graphite-{version}-{label}"
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="graphite-package-") as temporary:
        staging = Path(temporary) / name
        staging.mkdir()
        if args.source:
            # Include the release checkout's exact sources, without build outputs
            # or locally downloaded compatibility datasets.
            files = run("git", "ls-files", "--cached", "-z").split("\0")
            for relative in files:
                source = ROOT / relative
                if relative and source.is_file():
                    target = staging / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(source, target)
            # A Git archive does not contain gitlink contents. Copy the pinned
            # Bandage tree explicitly so the optional OGDF feature also builds
            # from this standalone source package.
            bandage = ROOT / "Bandage"
            expected = run("git", "rev-parse", "HEAD:Bandage").strip()
            try:
                actual = subprocess.check_output(
                    ["git", "-C", str(bandage), "rev-parse", "HEAD"], text=True
                ).strip()
            except subprocess.CalledProcessError as error:
                parser.error(f"initialize the Bandage submodule before packaging: {error}")
            if actual != expected:
                parser.error(f"Bandage is at {actual}, expected pinned commit {expected}")
            if subprocess.check_output(["git", "-C", str(bandage), "status", "--porcelain"]):
                parser.error("Bandage has local changes; source package must match its pinned commit")
            submodule_files = subprocess.check_output(
                ["git", "-C", str(bandage), "ls-files", "--cached", "-z"]
            ).split(b"\0")
            for encoded in submodule_files:
                if encoded:
                    relative = Path(os.fsdecode(encoded))
                    source = bandage / relative
                    if source.is_file():
                        target = staging / "Bandage" / relative
                        target.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copyfile(source, target)
            config = run("cargo", "vendor", "--locked", "--versioned-dirs", str(staging / "vendor"))
            (staging / ".cargo").mkdir(exist_ok=True)
            config = config.replace(str(staging / "vendor"), "vendor")
            # TOML uses escaped backslashes for Windows absolute paths.
            config = config.replace(str(staging / "vendor").replace("\\", "\\\\"), "vendor")
            (staging / ".cargo" / "config.toml").write_text(config, encoding="utf-8")
        else:
            binary = args.binary.resolve()
            if not binary.is_file():
                parser.error(f"binary does not exist: {binary}")
            shutil.copy2(binary, staging / binary.name)
            for filename in ("README.md", "LICENSE", "THIRD_PARTY.md", "CHANGELOG.md"):
                shutil.copyfile(ROOT / filename, staging / filename)
            shutil.copytree(ROOT / "examples", staging / "examples")
            dependency_notices(staging)
        manifest = {
            "version": version, "commit": run("git", "rev-parse", "HEAD").strip(),
            "working_tree_modified": bool(run("git", "status", "--porcelain").strip()),
            "rustc": run("rustc", "--version").strip(), "platform": label,
            "layout_backend": "rust", "source_archive": f"graphite-{version}-source.tar.gz",
        }
        (staging / "build-info.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        archive_format = "zip" if "windows" in label.lower() else "gztar"
        archive = Path(shutil.make_archive(str(args.output.resolve() / name), archive_format, temporary, name))
        with archive.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        archive.with_name(archive.name + ".sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
        print(archive)


if __name__ == "__main__":
    main()

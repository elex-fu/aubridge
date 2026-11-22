#!/usr/bin/env python3
"""Derive private, explicitly test-only native updater fixtures without Cargo.

Never install or launch an app. A is an unchanged private baseline copy; B adds
one signed resource marker; C substitutes a copied native /usr/bin/false and is
then signed, archived and verified normally. C tests launch failure recovery,
not a product-code regression or a public release.
"""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

from release_common import (archive_app, check_app, copy_app, digest, executable,
                            manifest, run, unpack_archive, write_json)


def read_receipt(path):
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 4 * 1024 * 1024:
        raise RuntimeError("Source receipt must be a regular schema2 receipt <=4MiB")
    with path.open("rb") as source:
        return json.load(source)


def distinct_copy(source, destination):
    copy_app(source, destination)
    for path in source.rglob("*"):
        if path.is_file() and not path.is_symlink():
            copied = destination / path.relative_to(source)
            if os.path.samestat(path.stat(), copied.stat()):
                raise RuntimeError("Copied bundle shares a regular-file inode with its source")


def sign_private(app):
    run(["/usr/bin/codesign", "--force", "--deep", "--sign", "-",
         "--timestamp=none", str(app)])
    return check_app(app)


def fixture(output, label, source, receipt, provenance):
    stage = output / label
    stage.mkdir(mode=0o700)
    app = stage / "aubridge.app"
    distinct_copy(source, app)
    details = {"test_only": True, "label": label, "source_base": provenance,
               "same_product_version": True, "new_product_source": False}
    if label != "A":
        details["kind"] = ("same-code-signed-resource-marker" if label == "B"
                           else "controlled-native-launch-failure")
        if label == "C":
            original = executable(app)
            details["substituted_executable"] = {
                "from": "/usr/bin/false", "original_sha256": digest(original),
                "copied_system_bytes_sha256": digest(Path("/usr/bin/false")),
            }
            # Write bytes into the already independent private file. No hardlink
            # or symlink to the system executable or the source app is introduced.
            with Path("/usr/bin/false").open("rb") as source_bytes, original.open("wb") as copied:
                shutil.copyfileobj(source_bytes, copied)
                copied.flush()
                os.fsync(copied.fileno())
            original.chmod(0o755)
        marker = app / "Contents/Resources/aubridge-update-fixture.json"
        if marker.exists() or marker.is_symlink():
            raise RuntimeError("Source already contains an updater fixture marker")
        write_json(marker, details)
        sign_private(app)
    else:
        details["kind"] = "unchanged-private-baseline"
    files = manifest(app)
    if label == "A" and files != receipt["files"]:
        raise RuntimeError("Baseline clone differs from the original receipt")
    if label != "A" and files == receipt["files"]:
        raise RuntimeError("Derived fixture must have a different complete manifest")
    verification = check_app(app, files, revision=receipt["git_revision"])
    if label == "C":
        details["substituted_executable"]["signed_copy_sha256"] = digest(executable(app))
    archive = stage / f"aubridge-fixture-{label}.zip"
    archive_app(app, archive)
    derived = {**receipt, "files": files, "verification": verification,
               "fixture": details,
               "archive": {"name": archive.name, "size": archive.stat().st_size,
                           "sha256": digest(archive)}}
    if label != "A":
        derived["checks"] = "BASELINE_SOURCE_ONLY_NOT_A_NEW_PRODUCT_GATE"
    # Reuse the existing schema2 extractor, with all temporary writes private.
    with tempfile.TemporaryDirectory(prefix="roundtrip-", dir=stage) as temporary:
        unpacked = unpack_archive(archive, derived, Path(temporary) / "unpacked")
        check_app(unpacked, files, revision=receipt["git_revision"])
    derived["fixture_validation"] = {"codesign": "PASS", "archive_round_trip": "PASS",
                                     "runtime": "NOT_RUN"}
    write_json(stage / "receipt.json", derived)
    write_json(stage / "fixture-provenance.json", details)
    (stage / "SHA256SUMS").write_text(f"{derived['archive']['sha256']}  {archive.name}\n")
    readback_sha = digest(archive)
    if readback_sha != derived["archive"]["sha256"]:
        raise RuntimeError("Fixture archive changed after its round-trip verification")
    return {"app": str(app), "receipt": str(stage / "receipt.json"),
            "archive": str(archive), "independent_zip_sha256": readback_sha,
            "executable_sha256": digest(executable(app)), "test_only": True}


def prepare(source, receipt_path, output):
    if sys.platform != "darwin":
        raise RuntimeError("These fixtures require native macOS ad-hoc signing")
    if not output.is_absolute() or output.exists() or output.is_symlink():
        raise RuntimeError("Provide a new absolute private output directory")
    parent = output.parent.resolve(strict=True)
    output = parent / output.name
    protected = [Path("/Applications"), Path("/System"), Path("/Library"),
                 Path.home() / "Applications"]
    if any(output == root or root in output.parents for root in protected):
        raise RuntimeError("Fixture output cannot be in a normal application/system directory")
    if any(part.endswith(".app") for part in output.parts):
        raise RuntimeError("Fixture output cannot be inside an application bundle")
    if source.is_symlink():
        raise RuntimeError("Source app must not be a symlink")
    source = source.resolve(strict=True)
    receipt_path = receipt_path.resolve(strict=True)
    receipt = read_receipt(receipt_path)
    if (receipt.get("schema") != 2 or receipt.get("app") != "aubridge.app"
            or receipt.get("mode") != "development"
            or receipt.get("signing") != "local-ad-hoc"):
        raise RuntimeError("Only an explicit final development schema2 baseline is accepted")
    source_files = manifest(source)
    check_app(source, receipt["files"], revision=receipt["git_revision"])
    original_archive = receipt_path.parent / receipt["archive"]["name"]
    if (not original_archive.is_file() or original_archive.is_symlink()
            or original_archive.stat().st_size != receipt["archive"]["size"]
            or digest(original_archive) != receipt["archive"]["sha256"]):
        raise RuntimeError("Baseline archive does not match its independent receipt")
    provenance = {"app": str(source), "receipt_sha256": digest(receipt_path),
                  "archive_sha256": receipt["archive"]["sha256"],
                  "executable_sha256": digest(executable(source)),
                  "git_revision": receipt["git_revision"], "git_tree": receipt["git_tree"],
                  "version": receipt["version"]}
    output.mkdir(mode=0o700)
    # release_common.unpack_archive creates its own temporary trusted copy.
    # Route those copies under this run's explicit private output as well.
    previous_tempdir = tempfile.tempdir
    tempfile.tempdir = str(output)
    try:
        artifacts = {label: fixture(output, label, source, receipt, provenance)
                     for label in ("A", "B", "C")}
    finally:
        tempfile.tempdir = previous_tempdir
    if manifest(source) != source_files:
        raise RuntimeError("Source bundle changed while preparing fixtures")
    cases = {}
    for name in ("busy-idle", "rollback", "handshake"):
        root = output / "cases" / name
        for child in ("home", "data", "Applications", "evidence"):
            (root / child).mkdir(mode=0o700, parents=True)
        cases[name] = {"home": str(root / "home"), "data_dir": str(root / "data"),
                       "install_app": str(root / "Applications/aubridge.app"),
                       "evidence_dir": str(root / "evidence")}
    plan = {"test_only": True, "created_utc": datetime.now(timezone.utc).isoformat(),
            "source_base": provenance, "artifacts": artifacts, "cases": cases,
            "installed_or_started": False,
            "scope": "Private same-version dev swap and valid-package launch-failure recovery only; no public update acceptance"}
    write_json(output / "fixture-plan.json", plan)
    print(f"Prepared test-only fixtures; no app installed or started: {output / 'fixture-plan.json'}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-app", required=True, type=Path)
    parser.add_argument("--source-receipt", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    args = parser.parse_args()
    prepare(args.source_app, args.source_receipt, args.output_dir)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"prepare-update-fixtures: {error}", file=sys.stderr)
        sys.exit(1)

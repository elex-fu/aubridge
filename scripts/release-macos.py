#!/usr/bin/env python3
"""Build/verify a commit-bound macOS package; install with automatic recovery.

Development builds are ad-hoc signed. Distribution builds require Developer ID
and notarization credentials and never silently fall back to a development app.
"""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile

from clean_source import check, checkout
from release_common import (ROOT, archive_app, check_app, copy_app, digest, distribution_preflight,
                            manifest, run, unpack_archive, write_json)
from release_install import Installer


def build(output_dir, *, mode="development", profile="release"):
    if run(["git", "status", "--porcelain", "--untracked-files=normal"]):
        raise RuntimeError("Commit the source baseline before packaging (working tree is dirty)")
    distribution = mode == "distribution"
    if distribution:
        distribution_preflight(os.environ)
        if profile != "release":
            raise RuntimeError("Distribution requires the release profile")
    revision = run(["git", "rev-parse", "HEAD"])
    created = datetime.now(timezone.utc)
    stage = output_dir / f"{revision[:12]}-{created.strftime('%Y%m%dT%H%M%S.%fZ')}"
    stage.mkdir(parents=True, mode=0o700)
    with checkout(stage, revision, signing=distribution) as (source, env, identity):
        checked = check(source, env, stage, identity)
        config = json.loads((source / "crates/smoke/tauri.conf.json").read_text())
        build_info = {**checked, "created_utc": created.isoformat(), "profile": profile,
                      "version": config["version"], "mode": mode,
                      "signing_requested": "developer-id" if distribution else "local-ad-hoc",
                      "target": run(["rustc", "-vV"], cwd=source, env=env).split("host: ")[1].splitlines()[0],
                      "tauri_cli": run(["cargo", "tauri", "--version"], cwd=source, env=env)}
        write_json(stage / "aubridge-build.json", build_info)
        (stage / "Info.plist").write_bytes(plistlib.dumps({"AubridgeGitRevision": revision}))
        override = {"bundle": {"resources": {str(stage / "aubridge-build.json"): "aubridge-build.json"},
                    "macOS": {"infoPlist": str(stage / "Info.plist"), "hardenedRuntime": True}}}
        if not distribution:
            # Strip all Apple secrets from dev builds; Tauri signs before packaging.
            env["APPLE_SIGNING_IDENTITY"] = "-"
            override["bundle"]["macOS"]["signingIdentity"] = "-"
        write_json(stage / "tauri.release.json", override)
        env.update(AUBRIDGE_BUILD_REVISION=revision, AUBRIDGE_BUILD_TREE=identity["git_tree"],
                   AUBRIDGE_BUILD_MODE=mode, AUBRIDGE_SOURCE_VERIFIED="1")
        command = ["cargo", "tauri", "build", "--bundles", "app", "--ci",
                   "--config", str(stage / "tauri.release.json")]
        if profile == "debug":
            command.append("--debug")
        command.extend(("--", "--locked"))
        # The package output must be private, even when the check gate shares a cache.
        env["CARGO_TARGET_DIR"] = str(stage / "target")
        run(command, cwd=source / "crates/smoke", env=env, log=stage / "bundle.log")
        app = stage / "aubridge.app"
        copy_app(Path(env["CARGO_TARGET_DIR"]) / profile / "bundle/macos/aubridge.app", app)
        verification = check_app(app, distribution=distribution,
                                team_id=env.get("APPLE_TEAM_ID"), revision=revision)
        archive = stage / f"aubridge-{config['version']}-{build_info['target']}-{mode}.zip"
        archive_app(app, archive)
        receipt = {**build_info, "schema": 2, "app": "aubridge.app", "files": manifest(app),
                   "signing": "developer-id-notarized" if distribution else "local-ad-hoc",
                   "runtime_contract": ["request_limits", "value_summary", "build_identity"],
                   "team_id": env.get("APPLE_TEAM_ID") if distribution else None,
                   "verification": verification,
                   "archive": {"name": archive.name, "size": archive.stat().st_size,
                               "sha256": digest(archive)}}
        # Verify the distributed bytes, including notarization, after an archive round trip.
        with tempfile.TemporaryDirectory(prefix="aubridge-archive-", dir=stage) as temporary:
            unpacked = unpack_archive(archive, receipt, Path(temporary) / "unpacked")
            check_app(unpacked, receipt["files"], distribution=distribution,
                      team_id=receipt["team_id"], revision=revision)
        shutil.rmtree(stage / "target")
    write_json(stage / "receipt.json", receipt)
    (stage / "SHA256SUMS").write_text(f"{digest(archive)}  {archive.name}\n")
    print(f"Built and verified {mode} package: {stage / 'receipt.json'}")
    return stage


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    build_parser = subparsers.add_parser("build")
    build_parser.add_argument("--output-dir", type=Path, default=ROOT / ".local/releases")
    build_parser.add_argument("--mode", choices=("development", "distribution"), default="development")
    build_parser.add_argument("--profile", choices=("debug", "release"), default="release")
    subparsers.add_parser("preflight", help="Validate the formal distribution environment")
    for name in ("install", "verify", "recover"):
        child = subparsers.add_parser(name)
        if name != "recover":
            child.add_argument("receipt", type=Path)
            child.add_argument("--archive", type=Path)
            child.add_argument("--team-id", help="Trusted Developer ID Team, outside the receipt")
        child.add_argument("--app", type=Path, default=None)
        if name in ("install", "recover"):
            child.add_argument("--home", type=Path, help="Private application HOME for acceptance")
            child.add_argument("--data-dir", type=Path)
            child.add_argument("--timeout", type=float, default=30)
        if name == "install":
            child.add_argument("--allow-development", action="store_true")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("This package workflow requires macOS")
    if args.command == "build":
        build(args.output_dir.resolve(), mode=args.mode, profile=args.profile)
        return
    if args.command == "preflight":
        distribution_preflight(os.environ)
        print("Distribution environment present; signing/notarization still require a successful build")
        return
    if args.command == "recover":
        if args.timeout <= 0:
            parser.error("--timeout must be positive")
        installer = Installer(args.app or Path.home() / "Applications/aubridge.app",
                              home=args.home, data_dir=args.data_dir, timeout=args.timeout)
        result = installer.recover()
        print(f"Recovery state: {result['phase']}; journal: {installer.journal_path}")
        return
    path = args.receipt.resolve()
    receipt = json.loads(path.read_text())
    distribution = receipt.get("signing") == "developer-id-notarized"
    if distribution and args.command == "verify" and not args.team_id:
        parser.error("--team-id is required to verify a distribution package")
    with tempfile.TemporaryDirectory(prefix="aubridge-package-") as temporary:
        if receipt.get("schema") == 2:
            archive = args.archive or path.parent / receipt["archive"]["name"]
            source = unpack_archive(archive, receipt, Path(temporary) / "unpacked")
        else:
            if args.command != "verify":
                parser.error("Historical receipts support verify only; rebuild before installing")
            source = Path(receipt["app"])
        if args.command == "verify":
            check_app(args.app or source, receipt["files"], distribution=distribution,
                      team_id=args.team_id, revision=receipt.get("git_revision"))
            print("Archive, complete manifest, source revision and signature verified")
        else:
            if args.timeout <= 0:
                parser.error("--timeout must be positive")
            installer = Installer(args.app or Path.home() / "Applications/aubridge.app",
                                  home=args.home, data_dir=args.data_dir, timeout=args.timeout)
            result = installer.install(source, receipt, allow_development=args.allow_development,
                                       team_id=args.team_id)
            write_json(path.parent / "installation.json", result)
            print(f"Installed {result['git_revision']}; listener PID verified; journal: {installer.journal_path}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        # Print no captured command output: it may include signing-service details.
        print(f"release-macos: {error}", file=sys.stderr)
        sys.exit(1)

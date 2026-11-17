#!/usr/bin/env python3
"""Build a signed local development bundle and install it after the app quits.

Receipts bind the complete bundle to a clean Git commit. Installation preserves
the previous app and leaves credentials, configuration and the ledger in place.
This ad-hoc signed development package is not a notarized public release.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
import urllib.parse
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
BUNDLE_ID = "dev.aubridge.smoke"
INSTALL_PATH = Path.home() / "Applications/aubridge.app"


def run(args, *, cwd=ROOT, log=None):
    if log is None:
        return subprocess.check_output(args, cwd=cwd, text=True).strip()
    print(f"Running {args[0]} {args[1] if len(args) > 1 else ''}: {log}", flush=True)
    with log.open("w") as output:
        subprocess.run(args, cwd=cwd, stdout=output, stderr=subprocess.STDOUT, check=True)
    return None


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def manifest(app):
    files = {}
    for path in sorted(app.rglob("*")):
        name = str(path.relative_to(app))
        if path.is_symlink():
            files[name] = "symlink:" + os.readlink(path)
        elif path.is_file():
            files[name] = digest(path)
    return files


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def check_app(app, expected=None):
    info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    if info.get("CFBundleIdentifier") != BUNDLE_ID:
        raise RuntimeError("Unexpected application identity")
    run(["codesign", "--verify", "--deep", "--strict", str(app)])
    if expected is not None and manifest(app) != expected:
        raise RuntimeError("Bundle manifest does not match the build receipt")


def build(output_dir):
    if run(["git", "status", "--porcelain", "--untracked-files=normal"]):
        raise RuntimeError("Commit the source baseline before packaging (working tree is dirty)")
    revision = run(["git", "rev-parse", "HEAD"])
    created = datetime.now(timezone.utc)
    stage = output_dir / f"{revision[:12]}-{created.strftime('%Y%m%dT%H%M%SZ')}"
    stage.mkdir(parents=True)
    run(["zsh", str(ROOT / "scripts/check.zsh")], log=stage / "checks.log")
    run(
        ["cargo", "tauri", "build", "--debug", "--bundles", "app", "--ci", "--", "--locked"],
        cwd=ROOT / "crates/smoke", log=stage / "bundle.log",
    )
    metadata = json.loads(run(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"]))
    source = Path(metadata["target_directory"]) / "debug/bundle/macos/aubridge.app"
    app = stage / "aubridge.app"
    shutil.copytree(source, app, symlinks=True)
    build_info = {
        "git_revision": revision,
        "git_tree": run(["git", "rev-parse", "HEAD^{tree}"]),
        "created_utc": created.isoformat(),
        "profile": "debug",
        "signing": "local-ad-hoc",
        "rustc": run(["rustc", "--version"]),
        "node": run(["node", "--version"]),
        "tauri_cli": run(["cargo", "tauri", "--version"]),
    }
    resources = app / "Contents/Resources"
    resources.mkdir(exist_ok=True)
    write_json(resources / "aubridge-build.json", build_info)
    plist_path = app / "Contents/Info.plist"
    plist = plistlib.loads(plist_path.read_bytes())
    plist["AubridgeGitRevision"] = revision
    plist_path.write_bytes(plistlib.dumps(plist))
    run(
        ["codesign", "--force", "--sign", "-", "--identifier", BUNDLE_ID,
         "--timestamp=none", str(app)], log=stage / "codesign.log",
    )
    check_app(app)
    if run(["git", "status", "--porcelain", "--untracked-files=normal"]):
        raise RuntimeError("Source changed during the build; this candidate cannot be released")
    receipt = {**build_info, "app": str(app.resolve()), "files": manifest(app)}
    write_json(stage / "receipt.json", receipt)
    print(f"Built and verified: {stage / 'receipt.json'}")


def running_pids():
    executable = str(INSTALL_PATH / "Contents/MacOS/smoke")
    rows = run(["ps", "-axo", "pid=,comm="]).splitlines()
    return [int(row.split(None, 1)[0]) for row in rows
            if len(row.split(None, 1)) == 2 and row.split(None, 1)[1] == executable]


def verify_runtime():
    session_path = Path.home() / ".aubridge/gateway-session.json"
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    for _ in range(30):
        try:
            session = json.loads(session_path.read_text())
            endpoint = session["endpoint"]
            target = urllib.parse.urlsplit(endpoint)
            if (target.scheme != "http" or target.hostname != "127.0.0.1"
                    or target.username or target.password or not target.port
                    or target.path not in ("", "/") or target.query or target.fragment):
                raise RuntimeError("Unexpected gateway session endpoint")
            statuses = {}
            for route in ("/healthz", "/admin/quota-history", "/admin/import/cc-switch"):
                request = urllib.request.Request(
                    endpoint + route,
                    headers={"Authorization": "Bearer " + session["admin_token"]},
                )
                with opener.open(request, timeout=5) as response:
                    statuses[route] = response.status
            pids = running_pids()
            if pids and all(value == 200 for value in statuses.values()):
                return {"endpoint": endpoint, "pids": pids, "http_status": statuses}
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(1)
    raise RuntimeError("Installed app did not become healthy; the previous bundle is preserved")


def install(receipt_path, receipt):
    source = Path(receipt["app"])
    check_app(source, receipt["files"])
    if running_pids():
        raise RuntimeError("Quit aubridge from its application menu before installing; in-flight work must drain")
    backup = receipt_path.parent / "installed-before.app"
    pending = INSTALL_PATH.with_name(f".aubridge-install-{os.getpid()}.app")
    if pending.exists() or backup.exists():
        raise RuntimeError("Installation staging or backup already exists; inspect it before retrying")
    if INSTALL_PATH.is_symlink():
        raise RuntimeError("Refusing to replace a symlink at the application install path")
    if INSTALL_PATH.exists():
        info = plistlib.loads((INSTALL_PATH / "Contents/Info.plist").read_bytes())
        if info.get("CFBundleIdentifier") != BUNDLE_ID:
            raise RuntimeError("Refusing to replace a different application")
    INSTALL_PATH.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, pending, symlinks=True)
    check_app(pending, receipt["files"])
    if INSTALL_PATH.exists():
        INSTALL_PATH.rename(backup)
    try:
        pending.rename(INSTALL_PATH)
        check_app(INSTALL_PATH, receipt["files"])
    except Exception:
        if backup.exists():
            if INSTALL_PATH.exists():
                INSTALL_PATH.rename(pending)
            backup.rename(INSTALL_PATH)
        raise
    run(["open", "-g", str(INSTALL_PATH)])
    observed = verify_runtime()
    result = {
        "git_revision": receipt["git_revision"], "installed_app": str(INSTALL_PATH),
        "previous_app": str(backup) if backup.exists() else None,
        "executable_sha256": digest(INSTALL_PATH / "Contents/MacOS/smoke"),
        **observed,
    }
    write_json(receipt_path.parent / "installation.json", result)
    print(json.dumps(result, ensure_ascii=False, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    build_parser = subparsers.add_parser("build")
    build_parser.add_argument("--output-dir", type=Path, default=ROOT / ".local/releases")
    for name in ("install", "verify"):
        child = subparsers.add_parser(name)
        child.add_argument("receipt", type=Path)
        if name == "verify":
            child.add_argument("--app", type=Path)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("This local package workflow requires macOS")
    if args.command == "build":
        build(args.output_dir.resolve())
    else:
        path = args.receipt.resolve()
        receipt = json.loads(path.read_text())
        if args.command == "install":
            install(path, receipt)
        else:
            check_app(args.app or Path(receipt["app"]), receipt["files"])
            print("Bundle identity, strict signature and complete manifest verified")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"release-macos: {error}", file=sys.stderr)
        sys.exit(1)

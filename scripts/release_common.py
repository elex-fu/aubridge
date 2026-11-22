"""Release evidence and package checks. No credential values belong in receipts."""

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import plistlib
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parent.parent
BUNDLE_ID = "dev.aubridge.smoke"
MAX_ARCHIVE_BYTES = 1024 * 1024 * 1024


def run(args, *, cwd=ROOT, env=None, log=None):
    if log is None:
        return subprocess.check_output(args, cwd=cwd, env=env, text=True,
                                       stderr=subprocess.STDOUT).strip()
    print(f"Running {args[0]} {args[1] if len(args) > 1 else ''}: {log}", flush=True)
    with Path(log).open("w") as output:
        subprocess.run(args, cwd=cwd, env=env, stdout=output,
                       stderr=subprocess.STDOUT, check=True)


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def write_json(path, value):
    path = Path(path)
    temp = path.with_name(path.name + ".tmp")
    with temp.open("w") as stream:
        os.chmod(temp, 0o600)
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temp.replace(path)


def manifest(app):
    files = {}
    for path in sorted(Path(app).rglob("*")):
        name = path.relative_to(app).as_posix()
        mode = stat.S_IMODE(path.lstat().st_mode)
        if path.is_symlink():
            if not path.resolve().is_relative_to(Path(app).resolve()):
                raise RuntimeError("Bundle symlink escapes the application")
            files[name] = {"symlink": os.readlink(path), "mode": mode}
        elif path.is_file():
            files[name] = {"sha256": digest(path), "mode": mode}
        elif path.is_dir():
            files[name] = {"directory": True, "mode": mode}
        else:
            raise RuntimeError("Bundle contains an unsupported file type")
    return files


def executable(app):
    info = plistlib.loads((Path(app) / "Contents/Info.plist").read_bytes())
    if info.get("CFBundleIdentifier") != BUNDLE_ID:
        raise RuntimeError("Unexpected application identity")
    name = info.get("CFBundleExecutable")
    if not name or Path(name).name != name:
        raise RuntimeError("Unexpected application executable")
    path = Path(app) / "Contents/MacOS" / name
    if path.is_symlink() or not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError("Application executable is missing or not executable")
    return path


def signing_details(app):
    output = run(["codesign", "--display", "--verbose=4", str(app)])
    team = re.search(r"^TeamIdentifier=(.+)$", output, re.MULTILINE)
    return {"team_id": team.group(1) if team else None,
            "developer_id": "Authority=Developer ID Application:" in output,
            "hardened_runtime": bool(re.search(r"flags=.*\bruntime\b", output))}


def check_app(app, expected=None, *, distribution=False, team_id=None, revision=None):
    app = Path(app)
    executable(app)
    actual = manifest(app)
    if expected is not None:
        # Historical local receipts stored only regular files and link targets.
        if expected and isinstance(next(iter(expected.values())), str):
            actual = {name: ("symlink:" + item["symlink"] if "symlink" in item
                             else item["sha256"]) for name, item in actual.items()
                      if "directory" not in item}
        if actual != expected:
            raise RuntimeError("Bundle manifest does not match the build receipt")
    run(["codesign", "--verify", "--deep", "--strict", str(app)])
    if revision:
        info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
        build = json.loads((app / "Contents/Resources/aubridge-build.json").read_text())
        if info.get("AubridgeGitRevision") != revision or build.get("git_revision") != revision:
            raise RuntimeError("Bundle source revision does not match the receipt")
    if distribution:
        details = signing_details(app)
        if (not team_id or details["team_id"] != team_id
                or not details["developer_id"] or not details["hardened_runtime"]):
            raise RuntimeError("Expected Developer ID Team and hardened runtime are required")
        run(["xcrun", "stapler", "validate", str(app)])
        run(["spctl", "--assess", "--type", "execute", "--verbose=2", str(app)])
        return {**details, "stapled_ticket": True, "gatekeeper": True}
    return {"signing": "local-ad-hoc", "notarized": False}


def distribution_preflight(env):
    required = ("APPLE_SIGNING_IDENTITY", "APPLE_TEAM_ID", "APPLE_API_ISSUER",
                "APPLE_API_KEY", "APPLE_API_KEY_PATH")
    missing = [name for name in required if not env.get(name)]
    if missing:
        raise RuntimeError("Distribution environment missing: " + ", ".join(missing))
    if not env["APPLE_SIGNING_IDENTITY"].startswith("Developer ID Application:"):
        raise RuntimeError("Distribution requires a Developer ID Application identity")
    if not re.fullmatch(r"[A-Z0-9]{10}", env["APPLE_TEAM_ID"]):
        raise RuntimeError("APPLE_TEAM_ID must be the expected 10-character Team ID")
    if not Path(env["APPLE_API_KEY_PATH"]).is_file():
        raise RuntimeError("APPLE_API_KEY_PATH must point to a local private key file")
    if env.get("APPLE_CERTIFICATE"):
        if "APPLE_CERTIFICATE_PASSWORD" not in env:
            raise RuntimeError("APPLE_CERTIFICATE_PASSWORD is required with APPLE_CERTIFICATE")
    elif env["APPLE_SIGNING_IDENTITY"] not in run(["security", "find-identity", "-v", "-p", "codesigning"]):
        raise RuntimeError("Developer ID Application identity is not installed")


def copy_app(source, destination):
    if Path(destination).exists():
        raise RuntimeError("Bundle copy destination already exists")
    if sys.platform == "darwin":
        # Preserve native metadata; generic copytree does not promise resource forks.
        run(["/usr/bin/ditto", str(source), str(destination)])
    else:
        shutil.copytree(source, destination, symlinks=True)


def archive_app(app, archive):
    """Store the verified app with explicit Unix modes and link entries."""
    if Path(archive).exists():
        raise RuntimeError("Archive output already exists")
    if sys.platform == "darwin":
        # Apple specifies ditto for macOS distribution archives, including metadata.
        run(["/usr/bin/ditto", "-c", "-k", "--keepParent", str(app), str(archive)])
        return
    with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED) as output:
        for path in [Path(app), *sorted(Path(app).rglob("*"))]:
            name = path.relative_to(Path(app).parent).as_posix()
            info = (zipfile.ZipInfo(name) if path.is_symlink()
                    else zipfile.ZipInfo.from_file(path, arcname=name))
            info.create_system = 3
            if path.is_symlink():
                info.external_attr = path.lstat().st_mode << 16
                output.writestr(info, os.readlink(path).encode())
            elif path.is_dir():
                output.writestr(info, b"")
            else:
                output.write(path, arcname=name)


def unpack_archive(archive, receipt, destination):
    """Validate all entries before extracting; no traversal or link-parent writes."""
    with tempfile.TemporaryDirectory(prefix="aubridge-verified-archive-") as temporary:
        trusted = Path(temporary) / "package.zip"
        with Path(archive).open("rb") as source, trusted.open("xb") as output:
            copied = 0
            while block := source.read(1024 * 1024):
                copied += len(block)
                if copied > MAX_ARCHIVE_BYTES:
                    raise RuntimeError("Archive exceeds size limit")
                output.write(block)
        return _unpack_archive(trusted, receipt, destination)


def _unpack_archive(archive, receipt, destination):
    archive = Path(archive)
    expected = receipt.get("archive", {})
    if (archive.stat().st_size != expected.get("size") or archive.stat().st_size > MAX_ARCHIVE_BYTES
            or digest(archive) != expected.get("sha256")):
        raise RuntimeError("Archive checksum or size does not match the receipt")
    destination = Path(destination)
    if destination.exists():
        raise RuntimeError("Archive extraction destination already exists")
    with zipfile.ZipFile(archive) as source:
        entries = source.infolist()
        if len(entries) > 10000 or sum(item.file_size for item in entries) > MAX_ARCHIVE_BYTES:
            raise RuntimeError("Archive exceeds extraction limits")
        names, links = set(), {}
        for item in entries:
            name = PurePosixPath(item.filename)
            metadata = name.parts and name.parts[0] == "__MACOSX"
            if (name.is_absolute() or ".." in name.parts or "\\" in item.filename
                    or not name.parts or (name.parts[0] != "aubridge.app" and not metadata)
                    or (metadata and len(name.parts) > 1 and name.parts[1] not in ("aubridge.app", "._aubridge.app"))
                    or str(name) in names or item.flag_bits & 1):
                raise RuntimeError("Unsafe or duplicate archive entry")
            names.add(str(name))
            kind = stat.S_IFMT(item.external_attr >> 16)
            if kind not in (0, stat.S_IFREG, stat.S_IFDIR, stat.S_IFLNK):
                raise RuntimeError("Unsupported archive entry type")
            if kind == stat.S_IFLNK:
                if metadata:
                    raise RuntimeError("Archive metadata cannot contain symlinks")
                if item.file_size > 4096:
                    raise RuntimeError("Archive symlink target is too long")
                target = source.read(item).decode("utf-8")
                normalized = os.path.normpath(str(name.parent / target))
                if (os.path.isabs(target) or "\\" in target or normalized == ".."
                        or not normalized.startswith("aubridge.app/")):
                    raise RuntimeError("Archive symlink escapes the application")
                links[str(name)] = target
        for name in names:
            if any(str(parent) in links for parent in PurePosixPath(name).parents):
                raise RuntimeError("Archive writes through a symlink")
        destination.mkdir(mode=0o700)
        if sys.platform == "darwin":
            run(["/usr/bin/ditto", "-x", "-k", str(archive), str(destination)])
            app = destination / "aubridge.app"
            manifest(app)  # Check resolved symlink chains after native extraction, too.
            return app
        directories = []
        for item in entries:
            if PurePosixPath(item.filename).parts[0] == "__MACOSX":
                continue
            path = destination / item.filename
            path.parent.mkdir(parents=True, exist_ok=True)
            if item.filename.rstrip("/") in links:
                continue
            mode = stat.S_IMODE(item.external_attr >> 16)
            if item.is_dir():
                path.mkdir(exist_ok=True)
                directories.append((path, mode))
            else:
                with source.open(item) as input_file, path.open("xb") as output:
                    shutil.copyfileobj(input_file, output, length=1024 * 1024)
                path.chmod(mode)
        for name, target in links.items():
            (destination / name).symlink_to(target)
        for name in links:
            if not (destination / name).resolve().is_relative_to(destination / "aubridge.app"):
                raise RuntimeError("Archive symlink chain escapes the application")
        for path, mode in reversed(directories):
            path.chmod(mode)
    return destination / "aubridge.app"

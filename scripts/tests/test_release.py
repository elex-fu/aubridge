"""Failure-oriented package/recovery tests; Apple service success is not mocked acceptance."""

import json
import http.server
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import clean_source
import release_common as common
import release_install as install


def bundle(path, version):
    (path / "Contents/MacOS").mkdir(parents=True)
    (path / "Contents/Info.plist").write_bytes(plistlib.dumps({
        "CFBundleIdentifier": common.BUNDLE_ID, "CFBundleExecutable": "smoke"}))
    binary = path / "Contents/MacOS/smoke"
    binary.write_text(version)
    binary.chmod(0o755)
    return path


def check_files(app, expected=None, **_):
    common.executable(app)
    if expected is not None and common.manifest(app) != expected:
        raise RuntimeError("fixture manifest mismatch")


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="aubridge-release-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()

    def archived(self):
        app = bundle(self.root / "aubridge.app", "candidate")
        archive = self.root / "aubridge.zip"
        common.archive_app(app, archive)
        receipt = {"files": common.manifest(app), "archive": {
            "size": archive.stat().st_size, "sha256": common.digest(archive)}}
        return app, archive, receipt

    @unittest.skipUnless(sys.platform == "darwin", "macOS runtime verifier")
    def test_runtime_requires_executable_identity_not_just_healthy_routes(self):
        app = bundle(self.root / "aubridge.app", "candidate")
        data = self.root / "data"
        data.mkdir()
        identity = {"version": "0.1.0", "git_revision": "expected", "git_tree": "tree",
                    "mode": "development", "profile": "release"}
        build = {**identity, "git_revision": "wrong", "source_verified": True, "pid": 123}
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(json.dumps(build if self.path == "/admin/build" else {}).encode())
            def log_message(self, *_):
                pass
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            (data / "gateway-session.json").write_text(json.dumps({
                "endpoint": f"http://127.0.0.1:{server.server_port}", "admin_token": "fixture"}))
            with patch.object(install, "process_executable", return_value=str(common.executable(app))), \
                    patch.object(install, "listener_pids", return_value={123}):
                with self.assertRaisesRegex(RuntimeError, "runtime contract"):
                    install.verify_runtime(app,data,pid=123,timeout=0.05,contract=["build_identity"],identity=identity)
                build.update(git_revision="expected",source_verified=False)
                with self.assertRaisesRegex(RuntimeError, "runtime contract"):
                    install.verify_runtime(app,data,pid=123,timeout=0.05,contract=["build_identity"],identity=identity)
                build["source_verified"] = True
                for key in ("version", "git_tree", "mode", "profile"):
                    expected = build[key]
                    build[key] = "mismatch"
                    with self.assertRaisesRegex(RuntimeError, "runtime contract"):
                        install.verify_runtime(app, data, pid=123, timeout=0.05,
                                               contract=["build_identity"], identity=identity)
                    build[key] = expected
                result = install.verify_runtime(app,data,pid=123,timeout=1,contract=["build_identity"],identity=identity)
                self.assertEqual(result["build_identity"]["git_revision"],"expected")
            server.shutdown()

    def test_archive_round_trip_and_tamper_rejection(self):
        app, archive, receipt = self.archived()
        extracted = common.unpack_archive(archive, receipt, self.root / "unpacked")
        self.assertEqual(common.manifest(extracted), common.manifest(app))
        with archive.open("ab") as stream:
            stream.write(b"corrupt")
        with self.assertRaisesRegex(RuntimeError, "checksum"):
            common.unpack_archive(archive, receipt, self.root / "tampered")
        self.assertFalse((self.root / "tampered").exists())

    @unittest.skipUnless(sys.platform == "darwin", "Native macOS archive metadata")
    def test_native_metadata_survives_copy_and_archive(self):
        source = bundle(self.root / "source.app", "candidate")
        binary = common.executable(source)
        subprocess.run(["/usr/bin/xattr", "-w", "com.aubridge.release-test", "metadata-fixture", str(binary)], check=True)
        app = self.root / "aubridge.app"
        common.copy_app(source, app)
        self.assertEqual(subprocess.check_output(["/usr/bin/xattr", "-p", "com.aubridge.release-test", str(common.executable(app))]).strip(), b"metadata-fixture")
        archive = self.root / "aubridge.zip"
        common.archive_app(app, archive)
        receipt = {"archive": {"size": archive.stat().st_size, "sha256": common.digest(archive)}}
        unpacked = common.unpack_archive(archive, receipt, self.root / "unpacked")
        self.assertEqual(subprocess.check_output(["/usr/bin/xattr", "-p", "com.aubridge.release-test", str(common.executable(unpacked))]).strip(), b"metadata-fixture")

    @unittest.skipIf(os.name == "nt", "Unix link/mode semantics")
    def test_link_round_trip_and_mode_is_part_of_manifest(self):
        app = bundle(self.root / "aubridge.app", "candidate")
        (app / "Contents/alias").symlink_to("MacOS/smoke")
        archive = self.root / "aubridge.zip"
        common.archive_app(app, archive)
        receipt = {"files": common.manifest(app), "archive": {
            "size": archive.stat().st_size, "sha256": common.digest(archive)}}
        unpacked = common.unpack_archive(archive, receipt, self.root / "unpacked")
        self.assertEqual(common.manifest(unpacked), receipt["files"])
        (unpacked / "Contents/Info.plist").chmod(0o777)
        with self.assertRaisesRegex(RuntimeError, "manifest"):
            common.check_app(unpacked, receipt["files"])

    def test_traversal_and_link_parent_rejected_before_any_extraction(self):
        for index, entries in enumerate(([("../outside", b"bad", stat.S_IFREG | 0o644)],
                [("aubridge.app/link", b"../../outside", stat.S_IFLNK | 0o777)],
                [("aubridge.app/link", b"Contents", stat.S_IFLNK | 0o777),
                 ("aubridge.app/link/file", b"bad", stat.S_IFREG | 0o644)])):
            with self.subTest(index=index):
                archive = self.root / f"bad-{index}.zip"
                with zipfile.ZipFile(archive, "w") as output:
                    for name, content, mode in entries:
                        info = zipfile.ZipInfo(name)
                        info.external_attr = mode << 16
                        output.writestr(info, content)
                receipt = {"archive": {"size": archive.stat().st_size, "sha256": common.digest(archive)}}
                destination = self.root / f"bad-{index}"
                with self.assertRaises(RuntimeError):
                    common.unpack_archive(archive, receipt, destination)
                self.assertFalse(destination.exists())

    @unittest.skipIf(os.name == "nt", "macOS signature/executable fixture")
    def test_distribution_preflight_and_team_fail_closed(self):
        with self.assertRaisesRegex(RuntimeError, "APPLE_SIGNING_IDENTITY"):
            common.distribution_preflight({})
        env = {name: "fixture" for name in ("APPLE_SIGNING_IDENTITY", "APPLE_TEAM_ID",
                "APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_KEY_PATH")}
        env["APPLE_SIGNING_IDENTITY"] = "-"
        with self.assertRaisesRegex(RuntimeError, "Developer ID"):
            common.distribution_preflight(env)
        app = bundle(self.root / "aubridge.app", "candidate")
        with patch.object(common, "run", return_value="Authority=Developer ID Application: Fixture\nTeamIdentifier=WRONGTEAM1\nflags=0x10000(runtime)") as call:
            with self.assertRaisesRegex(RuntimeError, "Team"):
                common.check_app(app, distribution=True, team_id="RIGHTTEAM1")
            self.assertFalse(any("stapler" in args[0] for args, _ in call.call_args_list))


@unittest.skipIf(os.name == "nt", "macOS executable and installer fixtures")
class RecoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="aubridge-upgrade-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.home.mkdir()
        self.app = bundle(self.root / "aubridge.app", "previous")
        self.source = bundle(self.root / "candidate.app", "candidate")
        self.previous_files = common.manifest(self.app)
        self.receipt = {"git_revision": "fixture", "files": common.manifest(self.source),
                        "signing": "local-ad-hoc", "runtime_contract": ["request_limits"]}
        self.installer = install.Installer(self.app, home=self.home)
        self.config = self.home / ".aubridge/config.json"
        self.config.parent.mkdir()
        self.config.write_text('{"unrelated":"preserved"}\n')
        self.config_sha = common.digest(self.config)
        for name, replacement in (("check_app", check_files), ("running_pids", lambda _: [])):
            patcher = patch.object(install, name, side_effect=replacement)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_failed_start_restores_and_restarts_previous_without_touching_config(self):
        with patch.object(self.installer, "_start", side_effect=[RuntimeError("boot failure"), {"pid": 42}]) as launch:
            with self.assertRaisesRegex(RuntimeError, "previous installation restored"):
                self.installer.install(self.source, self.receipt, allow_development=True)
        self.assertEqual(common.manifest(self.app), self.previous_files)
        self.assertEqual(launch.call_count, 2)
        self.assertTrue(launch.call_args.kwargs["rollback"])
        journal = json.loads(self.installer.journal_path.read_text())
        self.assertEqual(journal["phase"], "rolled-back")
        self.assertEqual(common.manifest(Path(journal["transaction"]) / "failed.app"), self.receipt["files"])
        self.assertEqual(common.digest(self.config), self.config_sha)

    def test_integrity_failure_and_development_opt_in_never_swap_old_app(self):
        with self.assertRaisesRegex(RuntimeError, "allow-development"):
            self.installer.install(self.source, self.receipt)
        (self.source / "Contents/MacOS/smoke").write_text("tampered")
        with self.assertRaisesRegex(RuntimeError, "manifest"):
            self.installer.install(self.source, self.receipt, allow_development=True)
        self.assertEqual(common.manifest(self.app), self.previous_files)
        self.assertFalse(self.installer.journal_path.exists())

    def journal(self, phase):
        transaction = self.root / ".aubridge-upgrade-fixture"
        transaction.mkdir()
        shutil.copytree(self.source, transaction / "pending.app")
        journal = {"app": str(self.app), "data_dir": str(self.installer.data_dir),
                   "transaction": str(transaction), "candidate_files": self.receipt["files"],
                   "previous_files": self.previous_files, "pid": None}
        self.installer._save(journal, phase)
        return transaction

    def test_recover_crash_between_old_rename_and_checkpoint(self):
        transaction = self.journal("prepared")
        self.app.rename(transaction / "previous.app")
        with patch.object(self.installer, "_start", return_value={"pid": 42}):
            result = self.installer.recover()
        self.assertEqual(result["phase"], "rolled-back")
        self.assertEqual(common.manifest(self.app), self.previous_files)
        self.assertEqual(common.digest(self.config), self.config_sha)

    def test_recover_before_swap_leaves_previous_and_does_not_start_candidate(self):
        self.journal("prepared")
        with patch.object(self.installer, "_start") as launch:
            result = self.installer.recover()
        self.assertEqual(result["phase"], "aborted")
        launch.assert_not_called()
        self.assertEqual(common.manifest(self.app), self.previous_files)

    def test_failed_rollback_start_keeps_previous_available_for_recover(self):
        with patch.object(self.installer, "_start", side_effect=RuntimeError("boot failure")):
            with self.assertRaisesRegex(RuntimeError, "boot failure"):
                self.installer.install(self.source, self.receipt, allow_development=True)
        self.assertEqual(common.manifest(self.app), self.previous_files)
        with patch.object(self.installer, "_start", return_value={"pid": 42}):
            result = self.installer.recover()
        self.assertEqual(result["phase"], "rolled-back")

    def test_recover_crash_after_candidate_rename_and_is_idempotent(self):
        transaction = self.journal("old-saved")
        self.app.rename(transaction / "previous.app")
        (transaction / "pending.app").rename(self.app)
        with patch.object(self.installer, "_start", return_value={"pid": 42}) as launch:
            first = self.installer.recover()
            second = self.installer.recover()
        self.assertEqual(first["phase"], "rolled-back")
        self.assertEqual(first, second)
        self.assertEqual(launch.call_count, 1)
        self.assertEqual(common.manifest(self.app), self.previous_files)

    def test_recovery_preserves_external_edits(self):
        transaction = self.journal("old-saved")
        self.app.rename(transaction / "previous.app")
        (transaction / "pending.app").rename(self.app)
        (self.app / "Contents/MacOS/smoke").write_text("external")
        with self.assertRaisesRegex(RuntimeError, "outside this transaction"):
            self.installer.recover()
        self.assertEqual((self.app / "Contents/MacOS/smoke").read_text(), "external")
        self.assertEqual(common.manifest(transaction / "previous.app"), self.previous_files)

    def test_stale_healthy_session_cannot_validate_another_process(self):
        (self.config.parent / "gateway-session.json").write_text(json.dumps({
            "endpoint": "http://127.0.0.1:12345", "admin_token": "fixture"}))
        with patch.object(install, "process_executable", return_value=str(common.executable(self.app))), \
                patch.object(install, "listener_pids", return_value={999}), \
                patch.object(install.urllib.request, "build_opener") as opener:
            with self.assertRaisesRegex(RuntimeError, "runtime contract"):
                install.verify_runtime(self.app, self.config.parent, pid=123, timeout=0.01)
            opener.return_value.open.assert_not_called()


class CleanCheckoutTests(unittest.TestCase):
    def test_private_environment_drops_credentials_and_source_overrides(self):
        with patch.dict(os.environ, {"KEY_KIMI_CODE": "secret", "OPENAI_API_KEY": "secret",
                "CARGO_INCREMENTAL": "1", "CARGO_PROFILE_RELEASE_LTO": "off",
                "RUSTFLAGS": "override", "PLUGINS_DIR": "/user", "APPLE_PASSWORD": "secret",
                "npm_config_userconfig": "/secret/npmrc", "npm_config_fetch_header_token": "secret"}):
            env = clean_source.isolated_env(Path("/private-fixture"))
        for name in ("KEY_KIMI_CODE", "OPENAI_API_KEY", "RUSTFLAGS", "PLUGINS_DIR", "CARGO_PROFILE_RELEASE_LTO",
                     "APPLE_PASSWORD", "npm_config_userconfig", "npm_config_fetch_header_token"):
            self.assertNotIn(name, env)
        self.assertEqual(env["HOME"], "/private-fixture")
        self.assertEqual(env["CARGO_INCREMENTAL"], "0")
        self.assertIn("localhost", env["NO_PROXY"])

    def test_fixed_commit_excludes_ignored_and_untracked_user_inputs(self):
        with tempfile.TemporaryDirectory(prefix="aubridge-checkout-test-") as temp:
            root = Path(temp).resolve()
            repository = root / "repository"
            repository.mkdir()
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=repository,
                    stderr=subprocess.STDOUT, text=True).strip()
            git("init")
            (repository / "tracked").write_text("committed")
            (repository / ".gitignore").write_text(".env\n")
            git("add", ".")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "fixture")
            revision = git("rev-parse", "HEAD")
            (repository / ".env").write_text("not-to-copy")
            (repository / "untracked").write_text("not-to-copy")
            stage = root / "stage"
            stage.mkdir()
            with clean_source.checkout(stage, revision, root=repository) as (source, env, identity):
                self.assertEqual((source / "tracked").read_text(), "committed")
                self.assertFalse((source / ".env").exists())
                self.assertFalse((source / "untracked").exists())
                self.assertEqual(identity["git_revision"], revision)
                self.assertNotEqual(env["HOME"], str(Path.home()))
            self.assertFalse((stage / "source").exists())
            self.assertFalse((stage / "home").exists())


if __name__ == "__main__":
    unittest.main()

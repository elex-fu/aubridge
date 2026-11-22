"""macOS same-volume app replacement with a recoverable journal."""

from contextlib import contextmanager
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import urllib.parse
import urllib.request

from clean_source import isolated_env
from release_common import check_app, copy_app, digest, executable, manifest, run, write_json


@contextmanager
def install_lock(path):
    with path.open("a+b") as lock:
        os.chmod(path, 0o600)
        if os.name == "nt":
            import msvcrt
            lock.write(b"\0")
            lock.flush()
            lock.seek(0)
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            if os.name == "nt":
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def process_executable(pid):
    try:
        return run(["ps", "-ww", "-p", str(pid), "-o", "comm="])
    except subprocess.CalledProcessError:
        return None


def running_pids(app):
    path = str(executable(app))
    rows = run(["ps", "-ww", "-axo", "pid=,comm="]).splitlines()
    return [int(parts[0]) for row in rows if len(parts := row.split(None, 1)) == 2
            and parts[1] == path]


def listener_pids(port):
    try:
        rows = run(["/usr/sbin/lsof", "-nP", f"-iTCP:{port}", "-sTCP:LISTEN", "-Fp"])
        return {int(row[1:]) for row in rows.splitlines() if row.startswith("p")}
    except subprocess.CalledProcessError:
        return set()


def verify_runtime(app, data_dir, *, pid, timeout=30, contract=(), identity=None):
    """Both the session's socket and the process must belong to this exact app."""
    expected_executable = str(executable(app))
    deadline = time.monotonic() + timeout
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while time.monotonic() < deadline:
        if process_executable(pid) != expected_executable:
            raise RuntimeError("Started application exited or its process identity changed")
        try:
            session = json.loads((data_dir / "gateway-session.json").read_text())
            endpoint = session["endpoint"]
            target = urllib.parse.urlsplit(endpoint)
            if (target.scheme != "http" or target.hostname != "127.0.0.1"
                    or target.username or target.password or not target.port
                    or target.path not in ("", "/") or target.query or target.fragment):
                raise ValueError("Unexpected gateway session endpoint")
            if listener_pids(target.port) != {pid}:
                raise ValueError("Session socket is not owned by the started application")
            statuses, documents = {}, {}
            routes = ["/healthz", "/admin/quota-history", "/admin/import/cc-switch"]
            if contract:
                routes.extend(("/admin/gateway", "/admin/dashboard"))
            if "build_identity" in contract:
                routes.append("/admin/build")
            for route in routes:
                request = urllib.request.Request(endpoint.rstrip("/") + route,
                    headers={"Authorization": "Bearer " + session["admin_token"]})
                remaining = max(0.05, min(2, deadline - time.monotonic()))
                with opener.open(request, timeout=remaining) as response:
                    statuses[route] = response.status
                    if route in ("/admin/gateway", "/admin/dashboard", "/admin/build"):
                        documents[route] = json.loads(response.read(2 * 1024 * 1024))
            if "request_limits" in contract and "request_limits" not in documents["/admin/gateway"]:
                raise ValueError("New gateway contract is missing")
            if "value_summary" in contract and "value_summary" not in documents["/admin/dashboard"]:
                raise ValueError("New ledger contract is missing")
            if "build_identity" in contract:
                build = documents["/admin/build"]
                if (not identity or any(build.get(key) != expected for key, expected in identity.items())
                        or build.get("pid") != pid or build.get("source_verified") is not True):
                    raise ValueError("Running executable build identity does not match the package")
            if (all(value == 200 for value in statuses.values())
                    and listener_pids(target.port) == {pid}
                    and process_executable(pid) == expected_executable):
                result = {"endpoint": endpoint, "pid": pid, "listener_pid_verified": True,
                          "http_status": statuses}
                if "build_identity" in contract:
                    result["build_identity"] = documents["/admin/build"]
                return result
        except (OSError, ValueError, KeyError, subprocess.CalledProcessError):
            pass
        time.sleep(min(0.2, max(0, deadline - time.monotonic())))
    raise RuntimeError("Started application did not meet its runtime contract before the deadline")


class Installer:
    TERMINAL = {"installed", "rolled-back", "rolled-back-empty", "aborted"}

    def __init__(self, app, *, home=None, data_dir=None, timeout=30):
        # Do not resolve the final path: that would hide a symlink replacement.
        self.app = Path(app).expanduser().absolute()
        self.home = Path(home).resolve() if home else Path.home()
        self.data_dir = Path(data_dir).resolve() if data_dir else (
            Path(os.environ["PLUGINS_DIR"]).parent.resolve()
            if home is None and os.environ.get("PLUGINS_DIR") else self.home / ".aubridge")
        self.env = isolated_env(self.home) if home else dict(os.environ)
        self.env["PLUGINS_DIR"] = str(self.data_dir / "plugins")
        self.timeout = timeout
        self.journal_path = self.app.with_name("." + self.app.name + ".upgrade.json")
        self.lock_path = self.app.with_name("." + self.app.name + ".upgrade.lock")
        self.child = None

    def _save(self, journal, phase):
        journal["phase"] = phase
        journal["updated_utc"] = datetime.now(timezone.utc).isoformat()
        write_json(Path(journal["transaction"]) / "journal.json", journal)
        write_json(self.journal_path, journal)

    def _load(self):
        value = json.loads(self.journal_path.read_text())
        transaction = Path(value["transaction"])
        if (value.get("app") != str(self.app) or value.get("data_dir") != str(self.data_dir)
                or transaction.parent != self.app.parent
                or not transaction.name.startswith(".aubridge-upgrade-") or transaction.is_symlink()):
            raise RuntimeError("Upgrade journal does not match this application/data directory")
        return value

    def _start(self, journal, *, rollback=False):
        log = Path(journal["transaction"]) / ("rollback-launch.log" if rollback else "launch.log")
        with log.open("ab") as output:
            os.chmod(log, 0o600)
            self.child = subprocess.Popen([str(executable(self.app))], cwd=self.home,
                env=self.env, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                start_new_session=True)
        journal["pid"] = self.child.pid
        self._save(journal, "rollback-starting" if rollback else "starting")
        return verify_runtime(self.app, self.data_dir, pid=self.child.pid, timeout=self.timeout,
                              contract=() if rollback else journal.get("runtime_contract", ()),
                              identity=None if rollback else journal.get("build_identity"))

    def _stop(self, journal):
        pids = running_pids(self.app)
        owned = journal.get("pid")
        if self.child and self.child.pid == owned and self.child.poll() is None and owned not in pids:
            # A failed executable may have become an interpreter; Popen still owns it.
            pids.append(owned)
        if any(pid != owned for pid in pids):
            raise RuntimeError("Quit the application before recovery; an unowned process is running")
        if owned in pids:
            os.kill(owned, signal.SIGTERM)
            deadline = time.monotonic() + 8
            while ((self.child and self.child.pid == owned and self.child.poll() is None)
                   or process_executable(owned) == str(executable(self.app))):
                if time.monotonic() >= deadline:
                    raise RuntimeError("Application has not exited; backup preserved, retry recover after quitting")
                time.sleep(0.1)

    def install(self, source, receipt, *, allow_development=False, team_id=None):
        distribution = receipt.get("signing") == "developer-id-notarized"
        if not distribution and not allow_development:
            raise RuntimeError("Development package requires explicit --allow-development")
        self.app.parent.mkdir(parents=True, exist_ok=True)
        with install_lock(self.lock_path):
            if self.journal_path.exists() and self._load()["phase"] not in self.TERMINAL:
                raise RuntimeError("An interrupted upgrade exists; run recover first")
            if self.app.is_symlink():
                raise RuntimeError("Refusing to replace a symlink at the install path")
            old_files = None
            if self.app.exists():
                check_app(self.app)
                if running_pids(self.app):
                    raise RuntimeError("Quit aubridge before installing; in-flight work must drain")
                old_files = manifest(self.app)
                if distribution:
                    from release_common import signing_details
                    installed_team = signing_details(self.app)["team_id"]
                    if installed_team and installed_team != "not set":
                        if team_id and team_id != installed_team:
                            raise RuntimeError("Configured Team differs from the installed application's Team")
                        team_id = installed_team
            if distribution and (not team_id or team_id != receipt.get("team_id")):
                raise RuntimeError("Pin the trusted Developer ID Team using --team-id")
            check_app(source, receipt["files"], distribution=distribution, team_id=team_id,
                      revision=receipt.get("git_revision"))
            stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
            transaction = self.app.parent / f".aubridge-upgrade-{stamp}-{os.getpid()}"
            transaction.mkdir(mode=0o700)
            pending = transaction / "pending.app"
            copy_app(source, pending)
            check_app(pending, receipt["files"], distribution=distribution, team_id=team_id,
                      revision=receipt.get("git_revision"))
            journal = {"schema": 1, "app": str(self.app), "data_dir": str(self.data_dir),
                       "transaction": str(transaction), "git_revision": receipt.get("git_revision"),
                       "candidate_files": receipt["files"], "previous_files": old_files,
                       "build_identity": {key: receipt.get(key) for key in ("version", "git_revision", "git_tree", "mode", "profile")},
                       "runtime_contract": receipt.get("runtime_contract", ()), "pid": None}
            self._save(journal, "prepared")
            try:
                if self.app.exists():
                    self.app.rename(transaction / "previous.app")
                self._save(journal, "old-saved")
                pending.rename(self.app)
                self._save(journal, "candidate-installed")
                observed = self._start(journal)
                check_app(self.app, receipt["files"], distribution=distribution, team_id=team_id,
                          revision=receipt.get("git_revision"))
                journal["runtime"] = observed
                journal["executable_sha256"] = digest(executable(self.app))
                self._save(journal, "installed")
                return journal
            except Exception as error:
                journal["failure"] = type(error).__name__
                self._save(journal, "failed")
                self._recover(journal)
                raise RuntimeError("Candidate failed; previous installation restored (see upgrade journal)") from error

    def recover(self):
        with install_lock(self.lock_path):
            journal = self._load()
            if journal["phase"] in self.TERMINAL:
                return journal
            return self._recover(journal)

    def _recover(self, journal):
        transaction = Path(journal["transaction"])
        previous = transaction / "previous.app"
        if self.app.is_symlink():
            raise RuntimeError("Refusing recovery over a symlink")
        current = manifest(self.app) if self.app.exists() else None
        if current not in (None, journal["candidate_files"], journal["previous_files"]):
            raise RuntimeError("Installed files changed outside this transaction; backup preserved")
        if previous.exists():
            check_app(previous, journal["previous_files"])
        if current == journal["previous_files"] and current is not None and not previous.exists():
            # Before the first rename, or after rollback's rename but before its checkpoint.
            if journal.get("phase") == "prepared":
                self._save(journal, "aborted")
                return journal
        elif current == journal["candidate_files"]:
            self._stop(journal)
            failed = transaction / "failed.app"
            if failed.exists():
                raise RuntimeError("Failed candidate already exists; preserve and inspect the transaction")
            self.app.rename(failed)
        if previous.exists():
            previous.rename(self.app)
        if journal["previous_files"] is None:
            self._save(journal, "rolled-back-empty")
            return journal
        if not self.app.exists() or manifest(self.app) != journal["previous_files"]:
            raise RuntimeError("Previous application is unavailable; transaction preserved")
        check_app(self.app, journal["previous_files"])
        pids = running_pids(self.app)
        if pids:
            if pids != [journal.get("pid")] or journal.get("phase") != "rollback-starting":
                raise RuntimeError("Quit the restored application before completing recovery")
            observed = verify_runtime(self.app, self.data_dir, pid=pids[0], timeout=self.timeout)
        else:
            observed = self._start(journal, rollback=True)
        journal["rollback_runtime"] = observed
        self._save(journal, "rolled-back")
        return journal

"""A detached commit checkout with isolated application configuration."""

from contextlib import contextmanager
import os
from pathlib import Path
import shutil

from release_common import ROOT, run, write_json


def isolated_env(home, *, signing=False):
    # An explicit allowlist keeps user credentials and source-affecting overrides out.
    keep = {"PATH", "TMPDIR", "TEMP", "TMP", "SYSTEMROOT", "WINDIR", "COMSPEC",
            "PATHEXT", "LANG", "LC_ALL", "USER", "LOGNAME", "CI", "GITHUB_ACTIONS",
            "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "CARGO_NET_OFFLINE",
            "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"}
    npm_options = {"proxy", "https_proxy", "fetch_retries", "fetch_timeout", "fetch_retry_mintimeout",
                   "fetch_retry_maxtimeout", "audit", "fund", "cache"}
    keep.update(name for name in os.environ if name.lower().removeprefix("npm_config_") in npm_options)
    if signing:
        keep.update(("APPLE_SIGNING_IDENTITY", "APPLE_TEAM_ID", "APPLE_CERTIFICATE",
                     "APPLE_CERTIFICATE_PASSWORD", "APPLE_API_ISSUER", "APPLE_API_KEY",
                     "APPLE_API_KEY_PATH"))
    env = {name: value for name, value in os.environ.items() if name in keep}
    original_home = Path.home()
    env.update({"HOME": str(home), "USERPROFILE": str(home),
                "CARGO_HOME": os.environ.get("CARGO_HOME", str(original_home / ".cargo")),
                "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(original_home / ".rustup")),
                "CODEX_HOME": str(home / ".codex"), "CLAUDE_CONFIG_DIR": str(home / ".claude"),
                "OPENCODE_CONFIG": str(home / ".config/opencode/opencode.json"),
                "XDG_CONFIG_HOME": str(home / ".config"), "XDG_DATA_HOME": str(home / ".local/share"),
                "XDG_STATE_HOME": str(home / ".local/state"), "XDG_CACHE_HOME": str(home / ".cache"),
                "NO_PROXY": "127.0.0.1,localhost,::1", "no_proxy": "127.0.0.1,localhost,::1",
                "PYTHONDONTWRITEBYTECODE": "1", "CARGO_INCREMENTAL": "0"})
    return env


def require_unchanged(source, revision):
    if (run(["git", "rev-parse", "HEAD"], cwd=source) != revision
            or run(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=source)):
        raise RuntimeError("Source changed during checks/build; candidate is invalid")


@contextmanager
def checkout(stage, revision="HEAD", *, signing=False, root=ROOT):
    revision = run(["git", "rev-parse", "--verify", f"{revision}^{{commit}}"], cwd=root)
    tree = run(["git", "rev-parse", f"{revision}^{{tree}}"], cwd=root)
    source, home = stage / "source", stage / "home"
    home.mkdir(mode=0o700)
    run(["git", "worktree", "add", "--detach", str(source), revision], cwd=root)
    env = isolated_env(home, signing=signing)
    # Shared compiler cache is permitted; source, node_modules and user data are fresh.
    env["CARGO_TARGET_DIR"] = str(root / "target")
    env["SOURCE_DATE_EPOCH"] = run(["git", "show", "-s", "--format=%ct", revision], cwd=root)
    try:
        yield source, env, {"git_revision": revision, "git_tree": tree,
                            "checkout": "detached-clean", "user_data": "isolated"}
        require_unchanged(source, revision)
    finally:
        # Preserve unexpected edits rather than force-remove a shared/user mutation.
        if source.exists():
            require_unchanged(source, revision)
            run(["git", "worktree", "remove", "--force", str(source)], cwd=root)
        shutil.rmtree(home)


def check(source, env, stage, identity):
    run(["python3" if os.name != "nt" else "python", "scripts/check.py"],
        cwd=source, env=env, log=stage / "checks.log")
    require_unchanged(source, identity["git_revision"])
    result = {**identity, "checks": "PASS", "checks_log": "checks.log",
              "rustc": run(["rustc", "--version"], cwd=source, env=env),
              "node": run(["node", "--version"], cwd=source, env=env)}
    write_json(stage / "checks.json", result)
    return result

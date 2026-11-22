#!/usr/bin/env python3
"""The same local/CI gate, using lockfiles and without provider calls."""

import os
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    root = Path(__file__).resolve().parent.parent
    npm = shutil.which("npm.cmd" if os.name == "nt" else "npm")
    if not npm:
        raise RuntimeError("npm is required")
    commands = [[sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests", "-v"],
                [npm, "--prefix", "crates/smoke/web", "ci"],
                [npm, "--prefix", "crates/smoke/web", "run", "build"],
                ["cargo", "fmt", "--all", "--", "--check"],
                ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"],
                ["cargo", "test", "--locked", "--workspace", "--all-targets"]]
    for command in commands:
        print("+ " + " ".join(command), flush=True)
        prefix = ["rtk", "proxy"] if shutil.which("rtk") else []
        subprocess.run(prefix + command, cwd=root, check=True)


if __name__ == "__main__":
    main()

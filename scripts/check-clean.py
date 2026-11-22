#!/usr/bin/env python3
"""Check a fixed commit in a clean detached checkout, without user credentials."""

import argparse
from datetime import datetime, timezone
from pathlib import Path

from clean_source import check, checkout
from release_common import ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", default="HEAD")
    parser.add_argument("--output-dir", type=Path, default=ROOT / ".local/checks")
    args = parser.parse_args()
    stage = args.output_dir.resolve() / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    stage.mkdir(parents=True)
    with checkout(stage, args.revision) as (source, env, identity):
        check(source, env, stage, identity)
    print(f"Clean checkout PASS: {stage / 'checks.json'}")


if __name__ == "__main__":
    main()

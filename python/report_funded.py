#!/usr/bin/env python3
"""Create a compact distribution report for one immutable execution lane."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from ivshock_validation.funded_report import write_report


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--steps", type=Path, required=True)
    parser.add_argument("--summary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = write_report(args.steps, args.summary, args.output)
    print(json.dumps(result["combined"], indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (FileExistsError, OSError, ValueError, json.JSONDecodeError) as error:
        print(json.dumps({"status": "FAILED", "error": str(error)}), file=sys.stderr)
        raise SystemExit(2) from None

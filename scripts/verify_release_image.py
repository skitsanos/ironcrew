#!/usr/bin/env python3
"""Stable compatibility entry point for the release-image receipt verifier."""

import sys

from release_image_receipt import main


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] != "verify":
        sys.argv.insert(1, "verify")
    elif len(sys.argv) == 1:
        sys.argv.append("verify")
    raise SystemExit(main())

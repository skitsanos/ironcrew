"""Allocate the pre-push gate's private, configuration-free temporary root."""

import os
from pathlib import Path
import shutil
import sys
import tempfile


class IsolationError(Exception):
    pass


def validate_directory(directory: Path, repository: Path) -> None:
    if directory == repository or repository in directory.parents:
        raise IsolationError("temporary directories must be outside the checkout")
    if not directory.is_dir():
        raise IsolationError("the temporary base must be an existing directory")
    # lstat also rejects dangling symlinks and non-file .env entries. Only an
    # absent entry is safe; permission and other inspection errors must stop us.
    for ancestor in (directory, *directory.parents):
        try:
            (ancestor / ".env").lstat()
        except FileNotFoundError:
            continue
        raise IsolationError("a .env entry exists in the temporary ancestry")


def create_temporary_root(repository: Path, configured_base: str) -> Path:
    base = Path(configured_base)
    if not base.is_absolute() or any(c in configured_base for c in "\r\n"):
        raise IsolationError("the temporary base must be an absolute, single-line path")
    repository = repository.resolve(strict=True)
    base = base.resolve(strict=True)
    if any(c in str(base) for c in "\r\n"):
        raise IsolationError("the resolved temporary base must be a single-line path")
    validate_directory(base, repository)
    # Passing dir explicitly prevents tempfile's fallback to TEMP, TMP, or CWD
    # when a configured base is unusable. mkdtemp creates an owned 0700 root.
    scratch = Path(tempfile.mkdtemp(prefix="ironcrew-pre-push.", dir=base))
    try:
        validate_directory(scratch, repository)
    except BaseException:
        shutil.rmtree(scratch)
        raise
    return scratch


def main() -> int:
    try:
        scratch = create_temporary_root(Path(sys.argv[1]), os.environ.get("TMPDIR") or "/tmp")
    except IsolationError as error:
        reason = str(error)
    except (OSError, RuntimeError, ValueError):
        # Filesystem errors can contain caller-controlled paths. Never print
        # their details or inspect .env contents to explain an admission error.
        reason = "cannot inspect or create a private temporary directory"
    else:
        print(scratch)
        return 0
    print(f"pre-push: temporary-directory isolation failed: {reason}", file=sys.stderr)
    print(
        "pre-push: set TMPDIR to an existing writable directory outside the checkout "
        "with no .env in its ancestry; no fallback was attempted.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())

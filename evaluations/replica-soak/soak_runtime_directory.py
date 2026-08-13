"""Lifecycle for an external, dotenv-free replica working directory."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path


def _has_dotenv(directory: Path) -> bool:
    """Detect files, directories, and broken symlinks named `.env`."""
    try:
        os.lstat(directory / ".env")
    except FileNotFoundError:
        return False
    except OSError as error:
        raise RuntimeError(
            "replica soak could not verify the runtime directory ancestor chain"
        ) from error
    return True


def verify_runtime_directory(runtime_dir: Path, source_root: Path) -> Path:
    """Require an existing directory outside source with no ancestor `.env`."""
    try:
        runtime = runtime_dir.resolve(strict=True)
        source = source_root.resolve(strict=True)
    except OSError as error:
        raise RuntimeError("replica soak runtime directory must exist") from error
    if not runtime.is_dir():
        raise RuntimeError("replica soak runtime path is not a directory")
    try:
        runtime.relative_to(source)
    except ValueError:
        pass
    else:
        raise RuntimeError("replica soak runtime directory must be outside source")
    for directory in (runtime, *runtime.parents):
        if _has_dotenv(directory):
            raise RuntimeError(
                "replica soak refuses a runtime directory with `.env` in its ancestor chain"
            )
    return runtime


class ExternalRuntimeDirectory:
    """Lazily allocate and clean one verified process working directory."""

    def __init__(self, source_root: Path) -> None:
        self.source_root = source_root
        self._temporary: tempfile.TemporaryDirectory[str] | None = None
        self.current: Path | None = None

    def ensure(self) -> Path:
        if self.current is not None:
            return verify_runtime_directory(self.current, self.source_root)
        temporary = tempfile.TemporaryDirectory(prefix="ironcrew-ic018-soak-")
        try:
            runtime = verify_runtime_directory(Path(temporary.name), self.source_root)
        except BaseException:
            temporary.cleanup()
            raise
        self._temporary = temporary
        self.current = runtime
        return runtime

    def cleanup(self) -> None:
        temporary, self._temporary = self._temporary, None
        self.current = None
        if temporary is not None:
            temporary.cleanup()

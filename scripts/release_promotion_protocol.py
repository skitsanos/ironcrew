"""Pure state transitions for immutable version and latest promotion."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

TAG_RE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


class PromotionError(RuntimeError):
    """A promotion precondition or postcondition failed."""


@dataclass(frozen=True)
class ReleaseImage:
    tag: str
    archive: Path
    digest: str


class PromotionBackend(Protocol):
    def inspect(self, tag: str) -> str | None: ...
    def copy(self, image: ReleaseImage, tag: str) -> bool: ...
    def latest_release_tag(self) -> str: ...
    def release_image(self, tag: str) -> ReleaseImage: ...


def stable_version(tag: str) -> str:
    if not TAG_RE.fullmatch(tag):
        raise PromotionError("release tag must match stable form vX.Y.Z")
    return tag[1:]


def promote_version(backend: PromotionBackend, image: ReleaseImage) -> str:
    version = stable_version(image.tag)
    existing = backend.inspect(version)
    if existing == image.digest:
        return "no-op"
    if existing is not None:
        raise PromotionError("immutable version tag points to a different digest")
    copied = backend.copy(image, version)
    observed = backend.inspect(version)
    if observed != image.digest:
        if not copied and observed is None:
            raise PromotionError("version copy failed and no registry tag appeared")
        raise PromotionError("version tag digest did not match the signed receipt")
    return "published" if copied else "concurrent-no-op"


def reconcile_latest(
    backend: PromotionBackend, *, max_attempts: int
) -> tuple[str, int]:
    if not 1 <= max_attempts <= 10:
        raise PromotionError("latest reconciliation attempts must be between 1 and 10")
    for attempt in range(1, max_attempts + 1):
        before = backend.latest_release_tag()
        stable_version(before)
        image = backend.release_image(before)
        if image.tag != before:
            raise PromotionError("verified release image tag did not match GitHub latest")
        if backend.inspect("latest") != image.digest:
            if not backend.copy(image, "latest"):
                raise PromotionError("latest copy failed")
        if backend.inspect("latest") != image.digest:
            raise PromotionError("latest digest did not match the signed receipt")
        after = backend.latest_release_tag()
        stable_version(after)
        if after == before:
            return before, attempt
    raise PromotionError("GitHub latest changed throughout the bounded reconciliation loop")

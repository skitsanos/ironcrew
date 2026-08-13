#!/usr/bin/env python3
"""Run IC-015 against one explicitly disposable Docker Hub repository."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable

from dockerhub_acceptance_api import AcceptanceError, DockerHubApi, fingerprint
from dockerhub_immutability import (
    ImmutabilityPolicyError,
    SEMVER_IMMUTABILITY_RULE,
    require_semver_immutability,
)
from promote_release_image import CommandBackend
from release_promotion_protocol import (
    PromotionError,
    ReleaseImage,
    promote_version,
    reconcile_latest,
)

SOURCE_RE = re.compile(
    r"^docker://[a-z0-9][a-z0-9./:_-]*@(?P<digest>sha256:[0-9a-f]{64})$"
)
VERSION_A = "v0.0.1"
VERSION_B = "v0.0.2"

class LatestSequenceBackend:
    def __init__(self, registry: CommandBackend, images: dict[str, ReleaseImage]):
        self.registry = registry
        self.images = images
        self.sequence = iter((VERSION_A, VERSION_B, VERSION_B, VERSION_B))

    def inspect(self, tag: str) -> str | None:
        return self.registry.inspect(tag)

    def copy(self, image: ReleaseImage, tag: str) -> bool:
        return self.registry.copy(image, tag)

    def latest_release_tag(self) -> str:
        return next(self.sequence)

    def release_image(self, tag: str) -> ReleaseImage:
        return self.images[tag]


def stage_image(registry: CommandBackend, source: str, destination: Path,
                tag: str) -> ReleaseImage:
    matched = SOURCE_RE.fullmatch(source)
    if not matched:
        raise AcceptanceError("source must be a docker:// reference pinned by sha256")
    registry._run([
        "skopeo", "copy", "--quiet", "--all", "--preserve-digests",
        source, f"oci-archive:{destination}",
    ])
    result = registry._run([
        "skopeo", "inspect", "--raw", f"oci-archive:{destination}"
    ])
    digest = matched.group("digest")
    observed = "sha256:" + hashlib.sha256(result.stdout.encode()).hexdigest()
    if observed != digest:
        raise AcceptanceError("staged OCI archive digest did not match its pinned source")
    return ReleaseImage(tag=tag, archive=destination, digest=digest)


def run_acceptance(registry: CommandBackend, first: ReleaseImage,
                   second: ReleaseImage, *, verify_policy: Callable[[], None]) \
        -> dict[str, object]:
    if first.digest == second.digest:
        raise AcceptanceError("acceptance images must have different digests")
    initial = promote_version(registry, first)
    replay = promote_version(registry, first)
    conflict = ReleaseImage(first.tag, second.archive, second.digest)
    try:
        promote_version(registry, conflict)
    except PromotionError as error:
        if str(error) != "immutable version tag points to a different digest":
            raise
    else:
        raise AcceptanceError("protocol accepted a conflicting version replay")
    verify_policy()
    overwrite_succeeded = registry.copy(conflict, "0.0.1")
    if overwrite_succeeded or registry.inspect("0.0.1") != first.digest:
        raise AcceptanceError("registry did not reject the immutable-tag overwrite")
    if not registry.copy(conflict, "latest") \
            or registry.inspect("latest") != second.digest:
        raise AcceptanceError(
            "mutable-tag control failed after immutable-tag rejection"
        )
    if not registry.copy(first, "latest") \
            or registry.inspect("latest") != first.digest:
        raise AcceptanceError("mutable-tag control could not restore the older digest")
    verify_policy()
    newer = promote_version(registry, second)
    source = LatestSequenceBackend(registry, {first.tag: first, second.tag: second})
    latest, attempts = reconcile_latest(source, max_attempts=3)
    if (initial, replay, newer, latest, attempts, registry.inspect("latest")) != (
        "published", "no-op", "published", VERSION_B, 2, second.digest
    ):
        raise AcceptanceError("promotion acceptance postconditions did not match")
    return {
        "initial_version_promotion": initial,
        "identical_replay": replay,
        "conflicting_replay": "protocol-refused",
        "registry_overwrite": "semver-only-rejected-with-mutable-control",
        "newer_version_promotion": newer,
        "latest_reconciled_to": latest,
        "latest_attempts": attempts,
        "version_digest": first.digest,
        "latest_digest": second.digest,
    }


def write_evidence(path: Path, document: dict[str, object]) -> None:
    with path.open("x", encoding="utf-8") as output:
        os.chmod(path, 0o600)
        json.dump(document, output, indent=2, sort_keys=True)
        output.write("\n")


def require_output_target(path: Path) -> None:
    path.parent.resolve(strict=True)
    if path.exists() or path.is_symlink():
        raise AcceptanceError("evidence output already exists")


def read_bound_evidence(path: Path | None, *, phase: str,
                        api: DockerHubApi, identity: str | None = None) -> dict[str, object]:
    if path is None or path.is_symlink() or not path.is_file():
        raise AcceptanceError("bound input evidence is required")
    if path.stat().st_size > 1024 * 1024:
        raise AcceptanceError("bound input evidence exceeded its byte limit")
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        raise AcceptanceError("bound input evidence was invalid") from None
    expected = {
        "schema": "ironcrew.ic015.dockerhub-acceptance.v1",
        "phase": phase,
        "run_id": api.run_id,
        "repository": api.image,
    }
    if not isinstance(document, dict) \
            or any(document.get(key) != value for key, value in expected.items()):
        raise AcceptanceError("input evidence did not match the requested run")
    observed_identity = document.get("repository_fingerprint")
    if not isinstance(observed_identity, str) \
            or not re.fullmatch(r"[0-9a-f]{64}", observed_identity):
        raise AcceptanceError("input evidence repository identity was invalid")
    if identity is not None and observed_identity != identity:
        raise AcceptanceError("repository identity changed after preparation")
    if document.get("semver_rule") != SEMVER_IMMUTABILITY_RULE:
        raise AcceptanceError("input evidence immutable-tag rule was invalid")
    if phase == "acceptance-passed" and (
        document.get("final_tags") != ["0.0.1", "0.0.2", "latest"]
        or document.get("immutable_policy_revalidated") is not True
        or document.get("cleanup_required") is not True
    ):
        raise AcceptanceError("acceptance evidence postconditions were invalid")
    return document


def common_evidence(api: DockerHubApi, state: dict[str, object]) -> dict[str, object]:
    return {
        "schema": "ironcrew.ic015.dockerhub-acceptance.v1",
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "run_id": api.run_id,
        "repository": api.image,
        "repository_fingerprint": fingerprint(state),
        "semver_rule": SEMVER_IMMUTABILITY_RULE,
    }


def require_registry_tags_absent(registry: CommandBackend, tags: list[str]) -> None:
    if any(registry.inspect(tag) is not None for tag in tags):
        raise AcceptanceError("disposable registry tags still exist")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("prepare", "run", "verify-cleanup"))
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--input-evidence", type=Path)
    parser.add_argument("--source-a")
    parser.add_argument("--source-b")
    parser.add_argument("--docker-hub-api", default="https://hub.docker.com")
    parser.add_argument("--authorize-create", action="store_true")
    parser.add_argument("--authorize-promotion", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        require_output_target(args.evidence)
        if args.input_evidence is not None \
                and args.input_evidence.resolve() == args.evidence.resolve():
            raise AcceptanceError("input and output evidence paths must differ")
        api = DockerHubApi(
            namespace=args.namespace,
            run_id=args.run_id,
            username=os.environ.get("DOCKERHUB_USERNAME", ""),
            secret=os.environ.get("DOCKERHUB_TOKEN", ""),
            api_base=args.docker_hub_api,
        )
        if args.phase == "prepare":
            if not args.authorize_create:
                raise AcceptanceError("explicit disposable-repository creation is required")
            state = api.create()
            evidence = common_evidence(api, state) | {"phase": "prepared"}
        elif args.phase == "run":
            if not args.authorize_promotion:
                raise AcceptanceError("explicit non-production promotion is required")
            if not args.source_a or not args.source_b:
                raise AcceptanceError("two pinned source images are required")
            state = api.require_identity()
            read_bound_evidence(
                args.input_evidence, phase="prepared", api=api,
                identity=fingerprint(state),
            )
            require_semver_immutability(
                image=api.image,
                username=os.environ.get("DOCKERHUB_USERNAME", ""),
                secret=os.environ.get("DOCKERHUB_TOKEN", ""),
                api_base=args.docker_hub_api,
            )
            if api.tags():
                raise AcceptanceError("disposable repository was not empty before acceptance")
            with tempfile.TemporaryDirectory(prefix="ironcrew-ic015-acceptance-") as work:
                registry = CommandBackend(
                    repository="unused/unused", image=api.image, work=Path(work),
                    validator=Path("scripts/verify_release_image.py"),
                )
                first = stage_image(registry, args.source_a, Path(work) / "a.tar", VERSION_A)
                second = stage_image(registry, args.source_b, Path(work) / "b.tar", VERSION_B)
                result = run_acceptance(
                    registry, first, second,
                    verify_policy=lambda: require_semver_immutability(
                        image=api.image,
                        username=os.environ.get("DOCKERHUB_USERNAME", ""),
                        secret=os.environ.get("DOCKERHUB_TOKEN", ""),
                        api_base=args.docker_hub_api,
                    ),
                )
            require_semver_immutability(
                image=api.image,
                username=os.environ.get("DOCKERHUB_USERNAME", ""),
                secret=os.environ.get("DOCKERHUB_TOKEN", ""),
                api_base=args.docker_hub_api,
            )
            final_tags = ["0.0.1", "0.0.2", "latest"]
            if api.tags() != final_tags:
                raise AcceptanceError("final disposable tag inventory was unexpected")
            evidence = common_evidence(api, state) | {
                "phase": "acceptance-passed", "result": result,
                "final_tags": final_tags, "immutable_policy_revalidated": True,
                "cleanup_required": True,
            }
        else:
            prior = read_bound_evidence(
                args.input_evidence, phase="acceptance-passed", api=api
            )
            if api.repository(allow_missing=True) is not None:
                raise AcceptanceError("disposable repository still exists")
            cleanup_tags = ["0.0.1", "0.0.2", "latest"]
            with tempfile.TemporaryDirectory(prefix="ironcrew-ic015-cleanup-") as work:
                registry = CommandBackend(
                    repository="unused/unused", image=api.image, work=Path(work),
                    validator=Path("scripts/verify_release_image.py"),
                )
                require_registry_tags_absent(registry, cleanup_tags)
            evidence = {
                "schema": "ironcrew.ic015.dockerhub-cleanup.v1",
                "recorded_at": datetime.now(timezone.utc).isoformat(),
                "run_id": api.run_id,
                "repository": api.image,
                "repository_fingerprint": prior.get("repository_fingerprint"),
                "repository_absent": True,
                "registry_tags_absent": cleanup_tags,
            }
        write_evidence(args.evidence, evidence)
        print(f"{args.phase}: passed for {api.image}")
        return 0
    except (AcceptanceError, ImmutabilityPolicyError, PromotionError, OSError) as error:
        print(f"error: {error}", file=os.sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

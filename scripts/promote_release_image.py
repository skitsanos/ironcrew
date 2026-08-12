#!/usr/bin/env python3
"""Promote signed, release-built OCI images without rebuilding them."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

from dockerhub_immutability import ImmutabilityPolicyError, require_semver_immutability
from release_asset_verification import (
    ASSET_SIZE_LIMITS,
    check_checksum,
    release_asset_sizes,
)
from release_promotion_protocol import (
    PromotionBackend,
    PromotionError,
    ReleaseImage,
    promote_version,
    reconcile_latest,
    stable_version,
)

DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
NOT_FOUND_MARKERS = ("manifest unknown", "name unknown")
COMMAND_OUTPUT_MAX_BYTES = 1024 * 1024


class CommandBackend:
    def __init__(self, *, repository: str, image: str, work: Path, validator: Path):
        self.repository = repository
        self.image = image
        self.work = work
        self.validator = validator
        self.cache: dict[str, ReleaseImage] = {}

    @staticmethod
    def _run(command: list[str], *, allow_failure: bool = False) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryFile() as stdout_file, tempfile.TemporaryFile() as stderr_file:
            result = subprocess.run(
                command, stdout=stdout_file, stderr=stderr_file, check=False
            )
            streams = []
            for stream in (stdout_file, stderr_file):
                size = stream.tell()
                if size > COMMAND_OUTPUT_MAX_BYTES:
                    raise PromotionError(
                        f"{Path(command[0]).name} command output exceeded its byte limit"
                    )
                stream.seek(0)
                try:
                    streams.append(stream.read().decode("utf-8"))
                except UnicodeDecodeError:
                    raise PromotionError(
                        f"{Path(command[0]).name} command output was not UTF-8"
                    ) from None
        completed = subprocess.CompletedProcess(command, result.returncode, *streams)
        if completed.returncode and not allow_failure:
            raise PromotionError(f"{Path(command[0]).name} command failed")
        return completed

    def inspect(self, tag: str) -> str | None:
        result = self._run(
            ["skopeo", "inspect", "--format", "{{.Digest}}", f"docker://{self.image}:{tag}"],
            allow_failure=True,
        )
        if result.returncode:
            if any(marker in result.stderr.lower() for marker in NOT_FOUND_MARKERS):
                return None
            raise PromotionError("registry digest inspection failed")
        digest = result.stdout.strip()
        if not DIGEST_RE.fullmatch(digest):
            raise PromotionError("registry returned an invalid digest")
        return digest

    def copy(self, image: ReleaseImage, tag: str) -> bool:
        result = self._run(
            [
                "skopeo",
                "copy",
                "--quiet",
                "--all",
                "--preserve-digests",
                f"oci-archive:{image.archive}",
                f"docker://{self.image}:{tag}",
            ],
            allow_failure=True,
        )
        return result.returncode == 0

    def latest_release_tag(self) -> str:
        result = self._run(
            ["gh", "api", f"repos/{self.repository}/releases/latest", "--jq", ".tag_name"]
        )
        tag = result.stdout.strip()
        stable_version(tag)
        return tag

    def tag_commit(self, tag: str) -> str:
        reference = self._json_command(
            ["gh", "api", f"repos/{self.repository}/git/ref/tags/{tag}"],
            "GitHub tag reference",
        )
        tag_object = reference.get("object") if isinstance(reference, dict) else None
        if (
            not isinstance(tag_object, dict)
            or tag_object.get("type") != "tag"
            or not isinstance(tag_object.get("sha"), str)
            or not COMMIT_RE.fullmatch(tag_object["sha"])
        ):
            raise PromotionError("release ref must point to one annotated tag object")
        annotation = self._json_command(
            ["gh", "api", f"repos/{self.repository}/git/tags/{tag_object['sha']}"],
            "GitHub annotated tag",
        )
        commit_object = annotation.get("object") if isinstance(annotation, dict) else None
        commit = commit_object.get("sha") if isinstance(commit_object, dict) else None
        if not (
            isinstance(annotation, dict)
            and annotation.get("tag") == tag
            and isinstance(commit_object, dict)
            and commit_object.get("type") == "commit"
            and isinstance(commit, str)
            and COMMIT_RE.fullmatch(commit)
        ):
            raise PromotionError("annotated release tag must point directly to one commit")
        return commit

    def _json_command(self, command: list[str], label: str) -> object:
        result = self._run(command)
        try:
            return json.loads(result.stdout)
        except json.JSONDecodeError:
            raise PromotionError(f"{label} metadata was invalid") from None

    def _validate_release(self, tag: str, expected: set[str]) -> dict[str, int]:
        result = self._run(
            [
                "gh",
                "release",
                "view",
                tag,
                "--repo",
                self.repository,
                "--json",
                "tagName,isDraft,isPrerelease,assets",
            ]
        )
        try:
            state = json.loads(result.stdout)
        except json.JSONDecodeError:
            raise PromotionError("GitHub release metadata was invalid") from None
        return release_asset_sizes(state, tag, expected, PromotionError)

    def release_image(self, tag: str) -> ReleaseImage:
        if tag in self.cache:
            return self.cache[tag]
        stable_version(tag)
        directory = self.work / tag
        directory.mkdir(mode=0o700)
        archive_name = f"ironcrew-{tag}-linux-oci.tar"
        receipt_name = f"ironcrew-{tag}-image-receipt.v1.json"
        expected = {
            archive_name, f"{archive_name}.sha256", f"{archive_name}.bundle",
            receipt_name, f"{receipt_name}.sha256", f"{receipt_name}.bundle",
        }
        expected_sizes = self._validate_release(tag, expected)
        command = [
            "gh", "release", "download", tag, "--repo", self.repository,
            "--dir", str(directory),
        ]
        for name in sorted(expected):
            command.extend(["--pattern", name])
        self._run(command)
        downloaded = {path.name: path for path in directory.iterdir()}
        invalid = any(
            path.is_symlink() or not path.is_file() for path in downloaded.values()
        )
        if set(downloaded) != expected or invalid:
            raise PromotionError("release image asset set was incomplete or unexpected")
        observed_sizes = {
            name: path.stat().st_size for name, path in downloaded.items()
        }
        if observed_sizes != expected_sizes:
            raise PromotionError("downloaded release asset size did not match GitHub metadata")
        archive, receipt = directory / archive_name, directory / receipt_name
        check_checksum(archive, directory / f"{archive_name}.sha256", PromotionError)
        check_checksum(receipt, directory / f"{receipt_name}.sha256", PromotionError)
        identity = (
            f"https://github.com/{self.repository}/.github/workflows/"
            f"release.yml@refs/tags/{tag}"
        )
        for artifact in (archive, receipt):
            self._run([
                "cosign", "verify-blob", "--bundle", f"{artifact}.bundle",
                "--certificate-identity", identity,
                "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com",
                str(artifact),
            ])
        self._run(
            [
                sys.executable, "-B", str(self.validator), "--receipt", str(receipt),
                "--archive", str(archive), "--tag", tag,
            ]
        )
        try:
            document = json.loads(receipt.read_text(encoding="utf-8"))
            digest = document["oci_archive"]["index_digest"]
            commit = document["commit_sha"]
        except (OSError, json.JSONDecodeError, KeyError, TypeError):
            raise PromotionError("validated receipt could not be read") from None
        if not DIGEST_RE.fullmatch(digest):
            raise PromotionError("validated receipt contained an invalid image digest")
        if commit != self.tag_commit(tag):
            raise PromotionError("validated receipt commit did not match the release tag")
        image = ReleaseImage(tag=tag, archive=archive, digest=digest)
        self.cache[tag] = image
        return image


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--authorize-latest-reconciliation", action="store_true")
    parser.add_argument("--max-latest-attempts", type=int, default=3)
    parser.add_argument("--validator", type=Path, default=Path("scripts/verify_release_image.py"))
    parser.add_argument("--docker-hub-api", default="https://hub.docker.com")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        stable_version(args.tag)
        if not args.authorize_latest_reconciliation:
            raise PromotionError("explicit latest-reconciliation authorization is required")
        require_semver_immutability(
            image=args.image,
            username=os.environ.get("DOCKERHUB_USERNAME", ""),
            secret=os.environ.get("DOCKERHUB_TOKEN", ""),
            api_base=args.docker_hub_api,
        )
        with tempfile.TemporaryDirectory(prefix="ironcrew-image-promotion-") as temporary:
            backend = CommandBackend(
                repository=args.repository,
                image=args.image,
                work=Path(temporary),
                validator=args.validator,
            )
            release_image = backend.release_image(args.tag)
            version_result = promote_version(backend, release_image)
            latest_tag, attempts = reconcile_latest(
                backend, max_attempts=args.max_latest_attempts
            )
        print(
            f"Version {stable_version(args.tag)}: {version_result}; "
            f"latest: {latest_tag} ({attempts} attempt(s))"
        )
        return 0
    except (PromotionError, ImmutabilityPolicyError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

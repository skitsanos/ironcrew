#!/usr/bin/env python3
"""Package/verify an unpublished, exact-commit Linux CI executable; never publish it."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tarfile

ARCHIVE = "ironcrew-linux-amd64.tar.gz"
SCHEMA = "ironcrew.history-reader.ci.v1"
TARGET = "x86_64-unknown-linux-gnu"
MAX_BYTES = 256 * 1024 * 1024
TAR_OVERHEAD = 20 * 1024


def digest(data):
    return hashlib.sha256(data).hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def native_binary(data):
    require(64 <= len(data) <= MAX_BYTES, "Invalid executable size")
    require(data[:6] == b"\x7fELF\x02\x01" and data[18:20] == b"\x3e\x00",
            "Expected a native Linux AMD64 ELF executable")


def read_file(path, limit):
    require(path.is_file() and not path.is_symlink(), "Missing or linked artifact file")
    require(path.stat().st_size <= limit, "Artifact file exceeds size limit")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0)
    with os.fdopen(os.open(path, flags), "rb") as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_size <= limit, "Artifact file exceeds size limit")
        data = stream.read(limit + 1)
    require(len(data) <= limit, "Artifact file exceeds size limit")
    return data


def identity(commit, repository, run_id, attempt):
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "Invalid source commit")
    require(re.fullmatch(r"[\w.-]+/[\w.-]+", repository), "Invalid repository")
    require(re.fullmatch(r"[1-9][0-9]*", run_id), "Invalid run ID")
    require(re.fullmatch(r"[1-9][0-9]*", attempt), "Invalid run attempt")
    return dict(schema=SCHEMA, commit=commit, repository=repository,
                run_id=run_id, attempt=attempt, target=TARGET)


def package(binary, output, binding, epoch):
    data = read_file(binary, MAX_BYTES)
    native_binary(data)
    output.mkdir(parents=True, exist_ok=False)
    header = tarfile.TarInfo("ironcrew")
    header.size, header.mode, header.mtime = len(data), 0o755, epoch
    with (output / ARCHIVE).open("xb") as stream:
        with gzip.GzipFile(fileobj=stream, mode="wb", filename="", mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.USTAR_FORMAT) as archive:
                archive.addfile(header, io.BytesIO(data))
    receipt = dict(binding, archive=ARCHIVE,
                   archive_sha256=digest(read_file(output / ARCHIVE, MAX_BYTES)),
                   binary_sha256=digest(data), binary_bytes=len(data))
    raw = (json.dumps(receipt, sort_keys=True, indent=2) + "\n").encode()
    (output / "artifact.json").write_bytes(raw)
    (output / "checksums.txt").write_text(
        f"{receipt['archive_sha256']}  {ARCHIVE}\n{digest(raw)}  artifact.json\n")
    return receipt, digest(raw)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "Duplicate receipt field")
        result[key] = value
    return result


def verify(directory, output, binding, receipt_sha256):
    require(re.fullmatch(r"[0-9a-f]{64}", receipt_sha256), "Invalid receipt digest")
    raw = read_file(directory / "artifact.json", 4096)
    require(digest(raw) == receipt_sha256, "Receipt digest mismatch")
    receipt = json.loads(raw, object_pairs_hook=unique_object)
    require(set(receipt) == set(binding) | {
        "archive", "archive_sha256", "binary_sha256", "binary_bytes"}, "Invalid receipt fields")
    require(all(receipt[key] == value for key, value in binding.items()), "Artifact identity mismatch")
    require(receipt["archive"] == ARCHIVE, "Unexpected archive name")
    require(type(receipt["binary_bytes"]) is int and 64 <= receipt["binary_bytes"] <= MAX_BYTES,
            "Invalid declared executable size")
    for key in ["archive_sha256", "binary_sha256"]:
        require(isinstance(receipt[key], str) and re.fullmatch(r"[0-9a-f]{64}", receipt[key]),
                "Invalid declared digest")
    archive_path = directory / ARCHIVE
    archive_bytes = read_file(archive_path, MAX_BYTES)
    require(digest(archive_bytes) == receipt["archive_sha256"], "Archive digest mismatch")
    expected = f"{receipt['archive_sha256']}  {ARCHIVE}\n{receipt_sha256}  artifact.json\n"
    require(read_file(directory / "checksums.txt", 512).decode() == expected, "Checksum manifest mismatch")
    # Bound decompression before a tar parser can process expanded GNU/PAX metadata.
    with gzip.GzipFile(fileobj=io.BytesIO(archive_bytes), mode="rb") as compressed:
        expanded = compressed.read(MAX_BYTES + TAR_OVERHEAD + 1)
    require(1024 <= len(expanded) <= MAX_BYTES + TAR_OVERHEAD, "Expanded archive exceeds size limit")
    # Our producer emits one USTAR regular file. Parse one fixed 512-byte header only;
    # never ask tarfile to follow extended headers or their attacker-declared lengths.
    member = tarfile.TarInfo.frombuf(expanded[:512], "utf-8", "strict")
    require(member.name == "ironcrew" and member.type in [tarfile.REGTYPE, tarfile.AREGTYPE]
            and not member.linkname, "Archive must contain only a regular ironcrew executable")
    require(member.size == receipt["binary_bytes"] and member.mode == 0o755,
            "Archive executable metadata mismatch")
    end = 512 + member.size
    require(len(expanded) >= end + 1024 and not any(expanded[end:]),
            "Unexpected extra archive member or nonzero padding")
    data = expanded[512:end]
    require(len(data) == receipt["binary_bytes"] and digest(data) == receipt["binary_sha256"],
            "Executable digest mismatch")
    native_binary(data)
    output.mkdir(parents=True, exist_ok=False)
    binary = output / "ironcrew"
    with binary.open("xb") as stream:
        stream.write(data)
    binary.chmod(0o700)
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["package", "verify"])
    parser.add_argument("--commit", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--attempt", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--directory", type=Path)
    parser.add_argument("--receipt-sha256")
    args = parser.parse_args()
    binding = identity(args.commit, args.repository, args.run_id, args.attempt)
    if args.command == "package":
        require(args.binary is not None, "Executable is required")
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
        require(head == args.commit, "Build checkout does not match selected source")
        require(not subprocess.check_output(["git", "status", "--porcelain=v1", "--untracked-files=all"]),
                "Artifact source checkout is dirty")
        epoch = int(subprocess.check_output(["git", "show", "-s", "--format=%ct", "HEAD"]))
        receipt, sha = package(args.binary, args.output, binding, epoch)
        require(not subprocess.check_output(["git", "status", "--porcelain=v1", "--untracked-files=all"]),
                "Packaging changed the source checkout")
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
                stream.write(f"receipt_sha256={sha}\n")
        print(json.dumps(dict(receipt, receipt_sha256=sha), sort_keys=True))
    else:
        require(args.directory is not None, "Artifact directory is required")
        print(json.dumps(verify(args.directory, args.output, binding, args.receipt_sha256),
                         sort_keys=True))


if __name__ == "__main__":
    main()

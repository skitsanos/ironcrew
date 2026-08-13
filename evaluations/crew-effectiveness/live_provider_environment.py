"""Fail-closed, secret-minimal environment for paid OpenAI evaluation children."""

from __future__ import annotations

import os
import re
import stat
import subprocess
from collections.abc import Mapping
from pathlib import Path
from urllib.parse import urlsplit


MAX_DOTENV_BYTES = 64 * 1024
MAX_API_KEY_BYTES = 4 * 1024
MAX_SYSTEM_VALUE_BYTES = 16 * 1024
_PROVIDER_NAMES = ("OPENAI_API_KEY", "OPENAI_BASE_URL")
_MODEL_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,127}")
_ASSIGNMENT = re.compile(r"(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)")
_SYSTEM_ENVIRONMENT_ALLOWLIST = (
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "WINDIR",
)
_SENSITIVE_SUFFIXES = ("_KEY", "_TOKEN", "_PASSWORD", "_SECRET")


def _read_ignored_dotenv(root: Path) -> str | None:
    dotenv = root / ".env"
    if dotenv.is_symlink():
        raise ValueError("root .env must not be a symlink")
    try:
        before = dotenv.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(before.st_mode):
        raise ValueError("root .env must be a regular file")
    if before.st_size > MAX_DOTENV_BYTES:
        raise ValueError("root .env exceeds the live-provider size limit")
    ignored = subprocess.run(
        ["git", "check-ignore", "--quiet", "--", ".env"],
        cwd=root,
        check=False,
        capture_output=True,
        timeout=10,
    )
    if ignored.returncode != 0:
        raise ValueError("root .env must be ignored by repository policy")

    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(dotenv, flags)
    except OSError as error:
        raise ValueError("root .env could not be opened safely") from error
    try:
        opened = os.fstat(descriptor)
        if not stat.S_ISREG(opened.st_mode) or not os.path.samestat(before, opened):
            raise ValueError("root .env changed while opening")
        raw = os.read(descriptor, MAX_DOTENV_BYTES + 1)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    if len(raw) > MAX_DOTENV_BYTES:
        raise ValueError("root .env exceeds the live-provider size limit")
    if (opened.st_size, opened.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
        raise ValueError("root .env changed while reading")
    try:
        return raw.decode("utf-8", "strict")
    except UnicodeDecodeError as error:
        raise ValueError("root .env must be valid UTF-8") from error


def _quoted_value(raw: str, quote: str) -> str:
    value: list[str] = []
    escaped = False
    escapes = {"n": "\n", "r": "\r", "t": "\t", '"': '"', "\\": "\\"}
    for index, character in enumerate(raw[1:], 1):
        if escaped:
            if quote != '"' or character not in escapes:
                raise ValueError("unsupported escape in provider dotenv value")
            value.append(escapes[character])
            escaped = False
        elif character == "\\" and quote == '"':
            escaped = True
        elif character == quote:
            remainder = raw[index + 1 :].strip()
            if remainder and not remainder.startswith("#"):
                raise ValueError("unexpected content after quoted provider dotenv value")
            return "".join(value)
        else:
            value.append(character)
    raise ValueError("unterminated quoted provider dotenv value")


def _dotenv_value(raw: str) -> str:
    raw = raw.strip()
    if not raw:
        return ""
    if raw[0] in ("'", '"'):
        return _quoted_value(raw, raw[0])
    for index, character in enumerate(raw):
        if character == "#" and (index == 0 or raw[index - 1].isspace()):
            raw = raw[:index]
            break
    value = raw.rstrip()
    if any(character in value for character in ("'", '"', "`", "\x00", "\n", "\r")):
        raise ValueError("unsupported syntax in unquoted provider dotenv value")
    return value


def _provider_values(text: str | None) -> dict[str, str]:
    values: dict[str, str] = {}
    if text is None:
        return values
    for line_number, raw_line in enumerate(text.splitlines(), 1):
        stripped = raw_line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        match = _ASSIGNMENT.fullmatch(raw_line.strip())
        if match is None:
            raise ValueError(f"root .env line {line_number} is not a basic assignment")
        name, raw_value = match.groups()
        if name not in _PROVIDER_NAMES:
            continue
        if name in values:
            raise ValueError(f"root .env contains duplicate {name}")
        values[name] = _dotenv_value(raw_value)
    return values


def _validate_key(value: str) -> None:
    encoded = value.encode("utf-8")
    if not (20 <= len(encoded) <= MAX_API_KEY_BYTES):
        raise ValueError("OPENAI_API_KEY is malformed")
    if any(byte < 0x21 or byte > 0x7E for byte in encoded):
        raise ValueError("OPENAI_API_KEY is malformed")
    if value.casefold() in {"your-api-key-here", "replace-me", "changeme"}:
        raise ValueError("OPENAI_API_KEY is malformed")


def _validate_base_url(value: str) -> None:
    if not value or len(value.encode("utf-8")) > 2_048 or any(char.isspace() for char in value):
        raise ValueError("OPENAI_BASE_URL is malformed")
    parsed = urlsplit(value)
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or ".." in Path(parsed.path).parts
    ):
        raise ValueError("OPENAI_BASE_URL is malformed")


def _system_environment(source: Mapping[str, str]) -> dict[str, str]:
    selected: dict[str, str] = {}
    for name in _SYSTEM_ENVIRONMENT_ALLOWLIST:
        value = source.get(name)
        if value is None:
            continue
        if name.upper().endswith(_SENSITIVE_SUFFIXES):
            continue
        if "\x00" in value or len(value.encode("utf-8")) > MAX_SYSTEM_VALUE_BYTES:
            raise ValueError(f"allowed system environment value is malformed: {name}")
        if name.lower().endswith("proxy"):
            parsed = urlsplit(value)
            if parsed.username is not None or parsed.password is not None:
                raise ValueError(f"credential-bearing proxy environment is not allowed: {name}")
        selected[name] = value
    return selected


def live_provider_environment(
    root: Path,
    model: str,
    process_environment: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """Build a minimal paid-provider child environment without reporting values."""
    if not _MODEL_PATTERN.fullmatch(model):
        raise ValueError("evaluator model identifier is malformed")
    root = root.resolve(strict=True)
    process = os.environ if process_environment is None else process_environment
    dotenv_values = _provider_values(_read_ignored_dotenv(root))

    selected: dict[str, str] = {}
    for name in _PROVIDER_NAMES:
        from_process = process.get(name)
        from_dotenv = dotenv_values.get(name)
        if from_process is not None and from_dotenv is not None:
            raise ValueError(f"duplicate {name} across process environment and root .env")
        if from_process is not None:
            selected[name] = from_process
        elif from_dotenv is not None:
            selected[name] = from_dotenv

    key = selected.get("OPENAI_API_KEY")
    if key is None:
        raise ValueError("OPENAI_API_KEY is required for live evaluation")
    _validate_key(key)
    if "OPENAI_BASE_URL" in selected:
        _validate_base_url(selected["OPENAI_BASE_URL"])

    child = _system_environment(process)
    child.update(selected)
    child["OPENAI_MODEL"] = model
    return child


def redaction_canaries(environment: Mapping[str, str]) -> tuple[str, ...]:
    """Return unique secret values for exact-value log redaction, never a receipt."""
    values = {
        value
        for name, value in environment.items()
        if value and name.upper().endswith(_SENSITIVE_SUFFIXES)
    }
    return tuple(sorted(values))

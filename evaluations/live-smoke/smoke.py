#!/usr/bin/env python3
"""Opt-in current-build compatibility smoke. Default invocation is provider-free."""
import argparse
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import time

from contracts import HumanDriver, marker, verify
from http_case import run_http
from mock_provider import MockProvider
from plan import CASES, MODEL, RUN_TOKENS, TIMEOUT_SECONDS, plan
from process import Deadline, Process
from support import (HERE, ROOT, ExternalRuntimeDirectory, SmokeError, live_provider_environment,
                     redaction_canaries, reject_secrets, require_unchanged_provenance,
                     safe_binary_path, worktree_provenance)


def runtime_environment(live, origin=None):
    if live:
        environment = live_provider_environment(ROOT, MODEL)
        configured = environment.get("OPENAI_BASE_URL", "https://api.openai.com").rstrip("/")
        if configured not in ("https://api.openai.com", "https://api.openai.com/v1"):
            raise SmokeError("native_origin_required")
        environment["OPENAI_BASE_URL"] = "https://api.openai.com"
        environment["IC048_BASE_URL"] = "https://api.openai.com"
        environment["IRONCREW_MAX_RUN_TOKENS"] = str(RUN_TOKENS)
    else:
        # No dotenv, provider credentials, database URLs or operator overrides.
        environment = {name: os.environ[name] for name in ("PATH", "LANG", "TMPDIR") if name in os.environ}
        environment.update(OPENAI_API_KEY="ic048-contract-not-a-secret", OPENAI_BASE_URL=origin,
                           IRONCREW_ALLOW_PRIVATE_IPS="1", IC048_BASE_URL=origin,
                           IC048_FIXTURE_KEY="ic048-contract-not-a-secret")
    environment.update({
        "IRONCREW_STORE": "json", "IRONCREW_LOG": "warn",
        "IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS": "0",
        "IRONCREW_SHUTDOWN_TIMEOUT_SECS": "1", "IRONCREW_SHUTDOWN_DRAIN_MS": "50",
        "IRONCREW_ENV_ALLOWLIST": "IC048_BASE_URL,IC048_FIXTURE_KEY",
        "IRONCREW_MAX_RUN_LIFETIME": "90", "IRONCREW_APPROVAL_TIMEOUT": "15",
        "IRONCREW_ASK_HUMAN_TIMEOUT": "15", "IRONCREW_RATE_LIMIT_MS": "100",
        "IRONCREW_PROVIDER_CONNECT_TIMEOUT_SECS": "5",
        "IRONCREW_PROVIDER_REQUEST_TIMEOUT_SECS": "30",
        "IRONCREW_PROVIDER_MAX_REQUEST_BYTES": "32768",
        "IRONCREW_PROVIDER_MAX_RESPONSE_BYTES": "65536",
        "IRONCREW_PROVIDER_MAX_STREAM_BYTES": "65536",
        "IRONCREW_PROVIDER_MAX_OUTPUT_BYTES": "16384",
        "IRONCREW_PROVIDER_MAX_ERROR_BYTES": "8192",
        "IRONCREW_FILE_WRITE_MAX_BYTES": "1024",
    })
    return environment


def binary_identity(binary):
    label, scope = safe_binary_path(ROOT, binary)
    with binary.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    return {"path": label, "path_scope": scope, "sha256": digest}


def cli_case(binary, directory, environment, canaries, deadline, driver=None):
    child = Process([str(binary), "run", str(directory)], directory, environment, canaries, driver)
    try:
        return marker(child.wait(deadline))
    finally:
        child.close()
        child.check()
        if child.forced:
            raise SmokeError("cli_forced_shutdown")


def execute(binary, reviewed, mode):
    live = mode == "live"
    report = {"plan": reviewed, "mode": mode, "cases": [], "status": "failed",
              "started_at": datetime.now(timezone.utc).isoformat(),
              "raw_prompts_answers_outputs_retained": False,
              "runtime_budget_enforced": live}
    if live and os.environ.get("IRONCREW_LIVE_SMOKE") != "1":
        return {**report, "status": "skipped", "reason": "disabled"}, 0
    if live and not reviewed["pricing"]["fresh"]:
        return {**report, "reason": "pricing_review_expired"}, 1
    runtime = ExternalRuntimeDirectory(ROOT)
    start = time.monotonic()
    deadline = Deadline(TIMEOUT_SECONDS)
    canaries = ()
    initial = None
    try:
        with ExitStack() as resources:
            mock = MockProvider() if not live else None
            origin = resources.enter_context(mock) if mock else None
            environment = runtime_environment(live, origin)
            canaries = redaction_canaries(environment) if live else ()
            initial = worktree_provenance(ROOT)
            report["source"] = initial
            report["binary"] = binary_identity(binary)
            temporary = runtime.ensure()
            version = Process([str(binary), "--version"], temporary, environment, canaries)
            try:
                raw_version = version.wait(deadline).decode("utf-8").strip()
                if len(raw_version) > 100 or not raw_version.startswith("ironcrew "):
                    raise SmokeError("invalid_binary_identity")
                report["binary"]["version"] = raw_version
            finally:
                version.close()
            for case in CASES:
                require_unchanged_provenance(initial, worktree_provenance(ROOT))
                if binary_identity(binary) != {k: v for k, v in report["binary"].items() if k != "version"}:
                    raise SmokeError("binary_changed")
                deadline.remaining()
                directory = temporary / case["id"]
                directory.mkdir()
                shutil.copyfile(HERE / "fixtures" / case["fixture"], directory / "crew.lua")
                driver = HumanDriver() if "hitl" in case["id"] else None
                began = time.monotonic()
                outcome = {"id": case["id"], "transport": case["transport"],
                           "status": "failed", "allocated_token_capacity": RUN_TOKENS}
                report["cases"].append(outcome)
                try:
                    if case["transport"] == "http":
                        payload = run_http(binary, directory, environment, canaries, deadline, driver)
                    else:
                        payload = cli_case(binary, directory, environment, canaries, deadline, driver)
                    outcome.update(verify(case, payload, directory, live, driver))
                except SmokeError as error:
                    outcome["failure"] = error.code
                    raise
                finally:
                    outcome["elapsed_seconds"] = round(time.monotonic() - began, 3)
                print(f"{case['id']}: passed", flush=True)
            if mock and mock.requests != sum(case["generation_requests"] for case in report["cases"]):
                raise SmokeError("mock_receipt_mismatch")
            report["status"] = "passed"
    except SmokeError as error:
        report["failure"] = error.code
    except KeyboardInterrupt:
        report["failure"] = "interrupted"
    except ValueError as error:
        # Only recognize the fixed missing-credential message; never echo errors.
        if str(error) == "OPENAI_API_KEY is required for live evaluation":
            report.update(status="skipped", reason="credential_unconfigured")
        else:
            report["failure"] = "configuration_or_provenance"
    except (OSError, KeyError, TypeError):
        report["failure"] = "harness_runtime"
    finally:
        runtime.cleanup()
        report["disposable_runtime_removed"] = runtime.current is None
        report["elapsed_seconds"] = round(time.monotonic() - start, 3)
        report["finished_at"] = datetime.now(timezone.utc).isoformat()
        if initial is not None:
            try:
                require_unchanged_provenance(initial, worktree_provenance(ROOT))
                report["source_stable"] = True
            except (ValueError, OSError):
                report.update(status="failed", failure="source_changed", source_stable=False)
    report["observed_generation_requests"] = sum(case.get("generation_requests", 0) for case in report["cases"])
    report["conservative_cost_estimate_usd"] = round(sum(
        case.get("conservative_cost_estimate_usd", 0) for case in report["cases"]), 8)
    report["unreceipted_case_capacity_tokens"] = sum(
        RUN_TOKENS for case in report["cases"] if case["status"] != "passed")
    reject_secrets(json.dumps(report).encode(), canaries)
    return report, 0 if report["status"] in ("passed", "skipped") else 1


def main():
    def interrupt(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("plan", "contract", "live"), default="plan")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/ironcrew")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--max-cost-usd", default="0.15")
    args = parser.parse_args()
    try:
        reviewed = plan(args.max_cost_usd)
        if args.report is not None:
            target = args.report.absolute()
            if target.exists() or target.is_symlink() or target.resolve().is_relative_to(ROOT):
                raise ValueError("report must be a new file outside the source tree")
        if args.mode == "plan":
            report, code = {"status": "planned", "plan": reviewed}, 0
        else:
            if args.report is None:
                raise ValueError("execution requires an external --report path")
            report, code = execute(args.binary.absolute(), reviewed, args.mode)
        encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if len(encoded.encode()) > 4 * 1024 * 1024:
            raise ValueError("report exceeds size limit")
        if args.report:
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open("x", encoding="utf-8") as output:
                output.write(encoded)
            print(f"Live smoke: {report['status']}; report: {target}")
        else:
            print(encoded)
        return code
    except (ValueError, OSError, SmokeError, KeyboardInterrupt):
        print("Live smoke: invalid configuration or unsafe report destination")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

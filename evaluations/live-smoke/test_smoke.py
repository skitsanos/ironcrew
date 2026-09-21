import copy
from datetime import date
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import contracts
from contracts import HumanDriver, marker, verify
from mock_provider import MockProvider
from plan import CASES, PRICING_DATE, plan
from process import Deadline, Process
import smoke
from support import ROOT, SmokeError, classify_failure, reject_secrets


def usage(live=False):
    values = {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110,
              "cached_tokens": 0, "reasoning_tokens": 0, "cache_write_tokens": 0}
    settled = {key: {"known": str(value), "complete": True} for key, value in values.items()}
    settled.update(requests="1", coverage="complete")
    budget = dict(state="active" if live else "disabled", limit="10000" if live else None,
                  charged="110" if live else "0", retained="0", reserved="0", in_flight="0")
    return dict(settled=settled, coverage="complete", in_flight="0", budget=budget)


class PlanTests(unittest.TestCase):
    def test_fixed_envelope_and_latest_alias(self):
        result = plan(today=PRICING_DATE)
        self.assertEqual(result["model"], "gpt-5.6-luna")
        self.assertEqual(result["limits"]["total_tokens"], 40000)
        self.assertEqual(result["limits"]["generation_requests"], 47)
        self.assertEqual(result["pricing"]["estimated_upper_bound_usd"], "0.12")
        self.assertEqual(result["limits"]["concurrency"], 1)

    def test_cost_ceiling_rejects_malformed_or_expanded_envelopes(self):
        for value in ("nan", "Infinity", "oops", "0", "0.119", "0.151", "-1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                plan(value)

    def test_pricing_expiry_is_explicit(self):
        self.assertFalse(plan(today=date(2026, 10, 22))["pricing"]["fresh"])
        self.assertFalse(plan(today=date(2026, 9, 20))["pricing"]["fresh"])
        self.assertTrue(plan(today=date(2026, 10, 21))["pricing"]["fresh"])

    def test_request_bound_tracks_default_tool_round_contract(self):
        source = (ROOT / "src/engine/crew.rs").read_text()
        self.assertIn("max_tool_rounds: 10,", source)
        hitl = (smoke.HERE / "fixtures/hitl.lua").read_text()
        self.assertIn("max_retries = 0", hitl)
        self.assertIn('task.depends_on = {"writer"}', hitl)
        for case in CASES:
            fixture = (smoke.HERE / "fixtures" / case["fixture"]).read_text()
            self.assertIn('assert(env("IC048_BASE_URL")', fixture)
            self.assertIn("max_tokens = 1024", fixture)


class PrivacyAndConfigTests(unittest.TestCase):
    def test_disabled_does_not_read_credentials_or_start_processes(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(smoke, "runtime_environment") as env:
            result, code = smoke.execute(Path("missing"), plan(), "live")
            self.assertEqual((code, result["status"], result["reason"]), (0, "skipped", "disabled"))
            env.assert_not_called()

    def test_missing_credentials_skip_distinctly(self):
        with patch.dict(os.environ, {"IRONCREW_LIVE_SMOKE": "1"}, clear=True), \
                patch.object(smoke, "runtime_environment", side_effect=ValueError(
                    "OPENAI_API_KEY is required for live evaluation")):
            result, code = smoke.execute(Path("missing"), plan(today=PRICING_DATE), "live")
            self.assertEqual((code, result["reason"]), (0, "credential_unconfigured"))

    def test_expired_prices_block_before_credentials(self):
        with patch.dict(os.environ, {"IRONCREW_LIVE_SMOKE": "1"}, clear=True), \
                patch.object(smoke, "runtime_environment") as env:
            result, code = smoke.execute(Path("missing"), plan(today=date(2027, 1, 1)), "live")
            self.assertEqual((code, result["reason"]), (1, "pricing_review_expired"))
            env.assert_not_called()

    def test_contract_environment_drops_operator_secrets(self):
        with patch.dict(os.environ, {"OPENAI_API_KEY": "secret", "DATABASE_URL": "secret",
                                     "IRONCREW_MAX_RUN_TOKENS": "1", "IRONCREW_ALLOW_SHELL": "1"}):
            env = smoke.runtime_environment(False, "http://127.0.0.1:12345")
        self.assertNotIn("DATABASE_URL", env)
        self.assertNotIn("IRONCREW_ALLOW_SHELL", env)
        self.assertNotIn("IRONCREW_MAX_RUN_TOKENS", env)
        self.assertEqual(env["IC048_FIXTURE_KEY"], "ic048-contract-not-a-secret")

    def test_live_environment_rejects_proxy_origins(self):
        with patch.object(smoke, "live_provider_environment", return_value={
                "OPENAI_API_KEY": "secret", "OPENAI_BASE_URL": "https://example.org"}):
            with self.assertRaisesRegex(SmokeError, "native_origin_required"):
                smoke.runtime_environment(True)

    def test_live_key_never_exposed_to_lua(self):
        with patch.object(smoke, "live_provider_environment", return_value={"OPENAI_API_KEY": "secret"}):
            env = smoke.runtime_environment(True)
        self.assertNotIn("IC048_FIXTURE_KEY", env)
        self.assertNotIn("OPENAI_API_KEY", env["IRONCREW_ENV_ALLOWLIST"])
        self.assertEqual(env["IRONCREW_MAX_RUN_TOKENS"], "10000")

    def test_native_v1_spelling_is_normalized_without_changing_origin(self):
        with patch.object(smoke, "live_provider_environment", return_value={
                "OPENAI_API_KEY": "secret", "OPENAI_BASE_URL": "https://api.openai.com/v1/"}):
            env = smoke.runtime_environment(True)
        self.assertEqual(env["IC048_BASE_URL"], "https://api.openai.com")

    def test_raw_secrets_never_enter_reports(self):
        with self.assertRaisesRegex(SmokeError, "secret_output_rejected"):
            reject_secrets(b"provider echoed sensitive-value", ("sensitive-value",))

    def test_failure_taxonomy_does_not_echo_raw_errors(self):
        for raw, expected in ((b"HTTP 503 secret", "provider_unavailable"),
                              (b"HTTP 401 secret", "provider_credentials_or_quota"),
                              (b"HTTP 400 bad effort", "provider_contract"),
                              (b"input token counting failed", "admission_preflight_failed"),
                              (b"run token budget exhausted", "budget_exhausted"),
                              (b"blank output", "runtime_contract")):
            self.assertEqual(classify_failure(raw), expected)


class ContractTests(unittest.TestCase):
    def test_marker_is_unique_and_typed(self):
        self.assertEqual(marker(b'IC048_RESULT {"ok":true}\n'), {"ok": True})
        for raw in (b"", b"IC048_RESULT []", b"IC048_RESULT {", b"IC048_RESULT {}\nIC048_RESULT {}"):
            with self.assertRaises(SmokeError):
                marker(raw)

    def test_full_human_sequence_and_refusal_of_extra_questions(self):
        driver = HumanDriver()
        self.assertEqual(driver.answer("[writer] Ready?", "question"), "confirmed")
        self.assertEqual(driver.answer("[approval] Agent 'writer' file_write(output/writer.txt)", "approval"), "allow")
        driver.answer("[reviewer] Ready?", "question")
        self.assertEqual(driver.answer("[approval] Agent 'reviewer' file_write(output/reviewer.txt)", "approval"), "deny")
        driver.complete()
        with self.assertRaises(SmokeError):
            driver.answer("[writer] Again?", "question")

    def test_human_approval_does_not_allow_unknown_agent_or_target(self):
        for prompt, kind in (("[other] Ready?", "question"), ("[writer] Ready?", "approval")):
            with self.assertRaises(SmokeError):
                HumanDriver().answer(prompt, kind)
        driver = HumanDriver()
        driver.answer("[writer] Ready?", "question")
        with self.assertRaises(SmokeError):
            driver.answer("[approval] Agent 'writer' file_write(/outside)", "approval")

    def test_checked_receipts_and_blank_output(self):
        payload = {"usage": usage(), "results": [{"task": "answer", "success": True, "output": "ok"}]}
        self.assertEqual(verify(CASES[0], payload, Path("."), False)["status"], "passed")
        for change in (lambda p: p["usage"].update(coverage="partial"),
                       lambda p: p["usage"]["settled"].update(requests="2"),
                       lambda p: p["results"][0].update(output=" \n"),
                       lambda p: p["results"][0].update(success=False)):
            bad = copy.deepcopy(payload)
            change(bad)
            with self.assertRaises(SmokeError):
                verify(CASES[0], bad, Path("."), False)

    def test_live_receipts_require_active_fully_reconciled_budget(self):
        payload = {"usage": usage(True), "results": [{"task": "answer", "success": True, "output": "ok"}]}
        verify(CASES[0], payload, Path("."), True)
        for key, value in (("state", "disabled"), ("retained", "1"), ("charged", "109"), ("limit", "20000")):
            bad = copy.deepcopy(payload)
            bad["usage"]["budget"][key] = value
            with self.assertRaises(SmokeError):
                verify(CASES[0], bad, Path("."), True)


class ProcessTests(unittest.TestCase):
    def run_child(self, script, deadline=2, **kwargs):
        with tempfile.TemporaryDirectory() as directory:
            child = Process([sys.executable, "-c", script], directory, {}, **kwargs)
            try:
                return child.wait(Deadline(deadline))
            finally:
                child.close()
                self.assertIsNotNone(child.child.poll())

    def test_success_and_nonzero_failure(self):
        self.assertEqual(self.run_child("print('ok')"), b"ok\n")
        with self.assertRaises(SmokeError):
            self.run_child("raise SystemExit(1)")

    def test_timeout_and_capture_cap(self):
        with self.assertRaisesRegex(SmokeError, "wall_time_limit"):
            self.run_child("import time; time.sleep(10)", deadline=0.05)
        with self.assertRaisesRegex(SmokeError, "capture_limit"):
            self.run_child("print('x' * 1100000)")

    def test_secret_capture_rejected(self):
        with self.assertRaisesRegex(SmokeError, "secret_output_rejected"):
            self.run_child("print('fake-sensitive-value')", canaries=("fake-sensitive-value",))


@unittest.skipUnless(os.environ.get("IRONCREW_SMOKE_TEST_BIN"), "real binary contract selected separately")
class BinaryTests(unittest.TestCase):
    def test_real_binary_all_four_paths(self):
        report, code = smoke.execute(Path(os.environ["IRONCREW_SMOKE_TEST_BIN"]).resolve(), plan(), "contract")
        self.assertEqual(code, 0, report.get("failure"))
        self.assertEqual(report["observed_generation_requests"], 15)
        self.assertTrue(report["disposable_runtime_removed"])

    def test_faults_do_not_become_success(self):
        for fault in ("blank", "missing_usage", "outage"):
            with self.subTest(fault=fault), patch.object(smoke, "MockProvider", side_effect=lambda: MockProvider(fault)):
                report, code = smoke.execute(Path(os.environ["IRONCREW_SMOKE_TEST_BIN"]).resolve(), plan(), "contract")
                self.assertEqual(code, 1)
                self.assertEqual(report["status"], "failed")
                if fault == "outage":
                    self.assertEqual(report["failure"], "provider_unavailable")
                self.assertEqual(len(report["cases"]), 1)
                self.assertTrue(report["disposable_runtime_removed"])


if __name__ == "__main__":
    unittest.main()

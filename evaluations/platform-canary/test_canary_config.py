from __future__ import annotations

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path


CANARY_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(CANARY_ROOT))

from canary_config import CanaryConfigError, canary_environment  # noqa: E402
from config_contract import (  # noqa: E402
    CONFIG_ENV_ALLOWLIST,
    FORBIDDEN_CONFIG_NAMES,
    OPTIONAL_CONFIG_ENV_NAMES,
    SENSITIVE_NAME_FRAGMENTS,
    UNHASHED_IRONCREW_ENV_NAMES,
)
from fingerprints import FingerprintError, configuration_manifest  # noqa: E402


TABLE_PREFIX = "ic007_test_"
PROVIDER_BASE_URL = "http://provider.internal:8080/v1"


class CanaryConfigTests(unittest.TestCase):
    def test_mapping_matches_complete_contract_exactly_once(self) -> None:
        environment = canary_environment(TABLE_PREFIX, PROVIDER_BASE_URL)
        self.assertEqual(tuple(environment), CONFIG_ENV_ALLOWLIST)
        self.assertEqual(len(environment), len(CONFIG_ENV_ALLOWLIST))
        self.assertEqual(len(environment), len(set(CONFIG_ENV_ALLOWLIST)))
        self.assertTrue(all(isinstance(value, str) for value in environment.values()))

        manifest = configuration_manifest(environment, require_complete=True)
        for name in CONFIG_ENV_ALLOWLIST:
            self.assertTrue(manifest[name].startswith("present:"), name)
        for name in OPTIONAL_CONFIG_ENV_NAMES:
            self.assertEqual(manifest[name], "absent")

    def test_substitutions_are_validated_and_each_result_is_independent(self) -> None:
        first = canary_environment(TABLE_PREFIX, PROVIDER_BASE_URL)
        second = canary_environment("ic007_other_", "https://provider.example/v1")
        boundary = canary_environment(f"{'a' * 36}_", "http://[::1]:8080/v1")
        self.assertEqual(first["IRONCREW_PG_TABLE_PREFIX"], TABLE_PREFIX)
        self.assertEqual(first["PLATFORM_CANARY_PROVIDER_BASE_URL"], PROVIDER_BASE_URL)
        self.assertEqual(second["IRONCREW_PG_TABLE_PREFIX"], "ic007_other_")
        self.assertEqual(
            second["PLATFORM_CANARY_PROVIDER_BASE_URL"],
            "https://provider.example/v1",
        )
        self.assertEqual(boundary["IRONCREW_PG_TABLE_PREFIX"], f"{'a' * 36}_")
        first["IRONCREW_STORE"] = "json"
        self.assertEqual(second["IRONCREW_STORE"], "postgres")

    def test_security_and_coordination_policies_are_explicit(self) -> None:
        environment = canary_environment(TABLE_PREFIX, PROVIDER_BASE_URL)
        expected = {
            "IRONCREW_STORE": "postgres",
            "IRONCREW_REQUIRE_IDEMPOTENCY_KEY": "true",
            "IRONCREW_ALLOW_UNAUTHENTICATED": "false",
            "IRONCREW_TRUST_PROXY": "false",
            "IRONCREW_RUN_LEASE_TTL_SECONDS": "60",
            "IRONCREW_EVENT_JOURNAL_WRITE_TIMEOUT_MS": "5000",
            "IRONCREW_MAX_BODY_SIZE": "10485760",
            "IRONCREW_ASK_HUMAN_MAX_PENDING": "8",
            "IRONCREW_HITL_PG_MAX_CONCURRENT_READS": "1",
            "IRONCREW_ADMISSION_WORK_BURST": "10",
            "IRONCREW_MAX_ACTIVE_RUNS": "2",
            "IRONCREW_LUA_MAX_MEMORY_BYTES": "16777216",
            "IRONCREW_LUA_MAX_SOURCE_BYTES": "1048576",
            "IRONCREW_API_MESSAGE_MAX_BYTES": "262144",
            "IRONCREW_CHAT_HISTORY_MAX_BYTES": "33554432",
            "IRONCREW_MCP_ALLOWED_COMMANDS": "__disabled__",
            "IRONCREW_MCP_ALLOWED_HTTP_HOSTS": "__disabled__",
            "IRONCREW_ENV_ALLOWLIST": "PLATFORM_CANARY_PROVIDER_BASE_URL",
        }
        for name, value in expected.items():
            self.assertEqual(environment[name], value, name)

        self.assertGreaterEqual(
            int(environment["IRONCREW_IDEMPOTENCY_TTL_SECONDS"]),
            int(environment["IRONCREW_MAX_RUN_LIFETIME"]) + 3600,
        )
        self.assertLessEqual(
            int(environment["IRONCREW_EVENT_MAX_BYTES"]),
            int(environment["IRONCREW_EVENT_JOURNAL_PAGE_MAX_BYTES"]),
        )
        self.assertLessEqual(
            int(environment["IRONCREW_EVENT_REPLAY_MAX_BYTES"]),
            int(environment["IRONCREW_EVENT_JOURNAL_MAX_TOTAL_BYTES"]),
        )

    def test_mapping_excludes_secrets_identity_markers_and_ports(self) -> None:
        names = set(canary_environment(TABLE_PREFIX, PROVIDER_BASE_URL))
        blocked = FORBIDDEN_CONFIG_NAMES | {
            "PORT",
            "IRONCREW_PORT",
            "IRONCREW_INSTANCE_ID",
            "RAILWAY_REPLICA_ID",
            "IRONCREW_DEPLOYMENT_REVISION",
            "IRONCREW_ARTIFACT_FINGERPRINT",
            "IRONCREW_FLOW_FINGERPRINT",
            "IRONCREW_CONFIG_FINGERPRINT",
            "IRONCREW_HITL_KEYRING_FINGERPRINT",
        }
        self.assertTrue(names.isdisjoint(blocked))
        self.assertFalse(
            any(fragment in name for name in names for fragment in SENSITIVE_NAME_FRAGMENTS)
        )

    def test_complete_manifest_rejects_unexpected_ironcrew_overrides(self) -> None:
        environment = canary_environment(TABLE_PREFIX, PROVIDER_BASE_URL)
        environment.update(
            {
                "IRONCREW_INSTANCE_ID": "replica-a",
                "IRONCREW_HITL_ACTIVE_KEY_ID": "old",
                "IRONCREW_SSE_OUTPUT_MAX_CHARS": "4096",
            }
        )
        manifest = configuration_manifest(environment, require_complete=True)
        self.assertEqual(manifest["IRONCREW_SSE_OUTPUT_MAX_CHARS"], "present:4096")
        self.assertTrue(
            UNHASHED_IRONCREW_ENV_NAMES.issuperset(
                {
                    "IRONCREW_INSTANCE_ID",
                    "IRONCREW_HITL_ACTIVE_KEY_ID",
                }
            )
        )

        environment["IRONCREW_DIALOG_MAX_HISTORY"] = "different-on-one-replica"
        with self.assertRaisesRegex(FingerprintError, "unexpected IronCrew configuration"):
            configuration_manifest(environment, require_complete=True)

    def test_invalid_table_prefixes_fail_without_echoing_input(self) -> None:
        invalid = ("", "a", "Upper_", "hyphen-_", "_leading_", "a" * 37, "secret\nvalue_")
        for value in invalid:
            with self.subTest(value=repr(value)):
                with self.assertRaises(CanaryConfigError) as failure:
                    canary_environment(value, PROVIDER_BASE_URL)
                self.assertEqual(
                    str(failure.exception),
                    "table prefix must be 2-37 lowercase ASCII alphanumeric/underscore "
                    "bytes and end with underscore",
                )

    def test_invalid_provider_urls_fail_without_echoing_input(self) -> None:
        invalid = (
            "provider.internal/v1",
            "ftp://provider.internal/v1",
            "http://user:secret@provider.internal/v1",
            "http://provider.internal/v1/",
            "http://provider.internal/v1?secret=value",
            "http://provider.internal/v1#fragment",
            "http://provider.internal:/v1",
            "http://provider.internal:0/v1",
            "http://provider.internal:70000/v1",
            "http://bad_host/v1",
            "http://provider.internal./v1",
            "http://provider.internal/\x00v1",
            "http://provider.internal/v2",
            "http://prøvider.internal/v1",
        )
        for value in invalid:
            with self.subTest(value=value):
                with self.assertRaises(CanaryConfigError) as failure:
                    canary_environment(TABLE_PREFIX, value)
                self.assertEqual(
                    str(failure.exception),
                    "provider base URL must be a canonical HTTP(S) /v1 URL",
                )

    def test_cli_outputs_only_the_json_mapping(self) -> None:
        clean_environment = {"PYTHONDONTWRITEBYTECODE": "1"}
        for name in ("SYSTEMROOT", "WINDIR"):
            if name in os.environ:
                clean_environment[name] = os.environ[name]
        process = subprocess.run(
            [
                sys.executable,
                os.fspath(CANARY_ROOT / "canary_config.py"),
                "--table-prefix",
                TABLE_PREFIX,
                "--provider-base-url",
                PROVIDER_BASE_URL,
            ],
            cwd=CANARY_ROOT,
            env=clean_environment,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(process.stderr, "")
        self.assertEqual(
            json.loads(process.stdout),
            canary_environment(TABLE_PREFIX, PROVIDER_BASE_URL),
        )
        self.assertEqual(process.stdout.count("\n"), 1)


if __name__ == "__main__":
    unittest.main()

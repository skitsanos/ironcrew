from __future__ import annotations

import base64
import os
import sys
import tempfile
import unittest
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))

from fingerprints import (  # noqa: E402
    CONFIG_ENV_ALLOWLIST,
    FingerprintError,
    configuration_fingerprint,
    configuration_manifest,
    flow_tree_fingerprint,
    hitl_keyring_fingerprint,
)


def encoded_key(byte: int) -> str:
    return base64.b64encode(bytes([byte]) * 32).decode("ascii")


class FlowFingerprintTests(unittest.TestCase):
    def test_tree_hash_is_creation_order_independent_and_content_bound(self) -> None:
        with tempfile.TemporaryDirectory() as first_raw, tempfile.TemporaryDirectory() as second_raw:
            first = Path(first_raw)
            second = Path(second_raw)
            (first / "nested").mkdir()
            (first / "nested" / "agent.lua").write_text("return 'agent'\n")
            (first / "crew.lua").write_text("return 'crew'\n")
            (second / "crew.lua").write_text("return 'crew'\n")
            (second / "nested").mkdir()
            (second / "nested" / "agent.lua").write_text("return 'agent'\n")

            expected = flow_tree_fingerprint(first)
            self.assertEqual(expected, flow_tree_fingerprint(second))
            self.assertRegex(expected, r"\Asha256:[0-9a-f]{64}\Z")

            (second / "nested" / "agent.lua").write_text("return 'changed'\n")
            self.assertNotEqual(expected, flow_tree_fingerprint(second))

    def test_path_is_framed_into_the_hash(self) -> None:
        with tempfile.TemporaryDirectory() as first_raw, tempfile.TemporaryDirectory() as second_raw:
            first = Path(first_raw)
            second = Path(second_raw)
            (first / "a.lua").write_bytes(b"same")
            (second / "b.lua").write_bytes(b"same")
            self.assertNotEqual(flow_tree_fingerprint(first), flow_tree_fingerprint(second))

    @unittest.skipIf(os.name == "nt", "symlink behavior differs on Windows")
    def test_symlink_file_directory_and_root_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "crew.lua").write_text("return true\n")
            (root / "crew-link.lua").symlink_to(root / "crew.lua")
            with self.assertRaisesRegex(FingerprintError, "symlinks"):
                flow_tree_fingerprint(root)

            (root / "crew-link.lua").unlink()
            real = root / "real"
            real.mkdir()
            (root / "dir-link").symlink_to(real, target_is_directory=True)
            with self.assertRaisesRegex(FingerprintError, "symlinks"):
                flow_tree_fingerprint(root)

            root_link = root.parent / f"{root.name}-link"
            root_link.symlink_to(root, target_is_directory=True)
            try:
                with self.assertRaisesRegex(FingerprintError, "real directory"):
                    flow_tree_fingerprint(root_link)
            finally:
                root_link.unlink()


class ConfigurationFingerprintTests(unittest.TestCase):
    def test_allowlisted_environment_is_order_stable_and_secret_blind(self) -> None:
        first = {
            "IRONCREW_STORE": "postgres",
            "IRONCREW_DB_POOL_SIZE": "2",
            "DATABASE_URL": "postgres://user:first-secret@db/test",
        }
        second = {
            "DATABASE_URL": "postgres://user:second-secret@db/test",
            "IRONCREW_DB_POOL_SIZE": "2",
            "IRONCREW_STORE": "postgres",
        }
        self.assertEqual(
            configuration_fingerprint(first),
            configuration_fingerprint(second),
        )
        second["IRONCREW_DB_POOL_SIZE"] = "3"
        self.assertNotEqual(
            configuration_fingerprint(first),
            configuration_fingerprint(second),
        )

    def test_absent_and_empty_are_distinct(self) -> None:
        self.assertNotEqual(
            configuration_fingerprint({}, ("IRONCREW_STORE",)),
            configuration_fingerprint({"IRONCREW_STORE": ""}, ("IRONCREW_STORE",)),
        )

    def test_unsafe_or_duplicate_allowlist_is_rejected(self) -> None:
        with self.assertRaisesRegex(FingerprintError, "unsafe name"):
            configuration_fingerprint(
                {"IRONCREW_API_TOKEN": "do-not-echo"},
                ("IRONCREW_API_TOKEN",),
            )
        with self.assertRaisesRegex(FingerprintError, "unsafe name"):
            configuration_fingerprint(
                {"CUSTOM_PROVIDER_PASSWORD": "do-not-echo"},
                ("CUSTOM_PROVIDER_PASSWORD",),
            )
        with self.assertRaisesRegex(FingerprintError, "duplicates"):
            configuration_fingerprint({}, ("IRONCREW_STORE", "IRONCREW_STORE"))
        self.assertNotIn("DATABASE_URL", CONFIG_ENV_ALLOWLIST)
        self.assertNotIn("IRONCREW_HITL_ENCRYPTION_KEYS", CONFIG_ENV_ALLOWLIST)

    def test_complete_manifest_fails_closed_and_exposes_presence_only(self) -> None:
        with self.assertRaisesRegex(FingerprintError, "incomplete"):
            configuration_manifest(
                {"IRONCREW_STORE": "postgres"},
                ("IRONCREW_STORE", "IRONCREW_DB_POOL_SIZE"),
                require_complete=True,
            )
        secret = "postgres://user:do-not-retain@db/test"
        manifest = configuration_manifest(
            {
                "IRONCREW_STORE": "postgres",
                "IRONCREW_DB_POOL_SIZE": "2",
                "DATABASE_URL": secret,
                "IRONCREW_API_TOKEN": "also-do-not-retain",
            },
            ("IRONCREW_STORE", "IRONCREW_DB_POOL_SIZE"),
            require_complete=True,
        )
        encoded = repr(manifest)
        self.assertNotIn(secret, encoded)
        self.assertNotIn("also-do-not-retain", encoded)
        self.assertEqual(manifest["DATABASE_URL_PRESENT"], "true")
        self.assertEqual(manifest["IRONCREW_API_TOKEN_PRESENT"], "true")


class KeyringFingerprintTests(unittest.TestCase):
    def test_order_is_stable_and_active_key_is_bound(self) -> None:
        old = encoded_key(1)
        new = encoded_key(2)
        first = f'{{"old":"{old}","new":"{new}"}}'
        reordered = f'{{"new":"{new}","old":"{old}"}}'
        old_active = hitl_keyring_fingerprint(first, "old")
        self.assertEqual(old_active, hitl_keyring_fingerprint(reordered, "old"))
        self.assertNotEqual(old_active, hitl_keyring_fingerprint(first, "new"))
        self.assertRegex(old_active, r"\Asha256:[0-9a-f]{64}\Z")

    def test_same_ids_with_different_key_material_change_digest(self) -> None:
        first = f'{{"primary":"{encoded_key(3)}"}}'
        second = f'{{"primary":"{encoded_key(4)}"}}'
        self.assertNotEqual(
            hitl_keyring_fingerprint(first, "primary"),
            hitl_keyring_fingerprint(second, "primary"),
        )

    def test_key_material_never_appears_in_validation_errors(self) -> None:
        secret = encoded_key(5)
        duplicate_id = f'{{"old":"{secret}","old":"{secret}"}}'
        with self.assertRaises(FingerprintError) as duplicate:
            hitl_keyring_fingerprint(duplicate_id, "old")
        self.assertNotIn(secret, str(duplicate.exception))

        malformed = "secret-material-that-must-not-be-echoed"
        with self.assertRaises(FingerprintError) as invalid:
            hitl_keyring_fingerprint(f'{{"old":"{malformed}"}}', "old")
        self.assertNotIn(malformed, str(invalid.exception))

    def test_duplicate_material_and_partial_configuration_fail_closed(self) -> None:
        shared = encoded_key(6)
        with self.assertRaisesRegex(FingerprintError, "duplicate key material"):
            hitl_keyring_fingerprint(
                f'{{"old":"{shared}","new":"{shared}"}}',
                "old",
            )
        with self.assertRaisesRegex(FingerprintError, "configured together"):
            hitl_keyring_fingerprint(f'{{"old":"{shared}"}}', None)
        self.assertEqual(
            hitl_keyring_fingerprint(None, None),
            hitl_keyring_fingerprint(None, None),
        )


if __name__ == "__main__":
    unittest.main()

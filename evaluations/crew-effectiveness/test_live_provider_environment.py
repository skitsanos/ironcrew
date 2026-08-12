import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))
from live_provider_environment import (  # noqa: E402
    MAX_DOTENV_BYTES,
    live_provider_environment,
    redaction_canaries,
)


VALID_KEY = "sk-test-live-provider-key-1234567890"


def _repo(root: Path, *, ignore_dotenv: bool = True) -> None:
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    if ignore_dotenv:
        (root / ".gitignore").write_text(".env\n", encoding="utf-8")


class LiveProviderEnvironmentTests(unittest.TestCase):
    def test_process_environment_is_minimal_and_model_is_forced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _repo(root)
            process = {
                "PATH": "/usr/bin",
                "LANG": "C.UTF-8",
                "SSL_CERT_FILE": "/safe/ca.pem",
                "HTTPS_PROXY": "https://proxy.example",
                "OPENAI_API_KEY": VALID_KEY,
                "OPENAI_BASE_URL": "https://api.openai.com/v1",
                "OPENAI_MODEL": "untrusted-model",
                "OTHER_API_KEY": "must-not-pass",
                "GITHUB_TOKEN": "must-not-pass",
                "DB_PASSWORD": "must-not-pass",
                "APP_SECRET": "must-not-pass",
                "UNRELATED": "must-not-pass",
            }
            child = live_provider_environment(root, "gpt-5.6-luna", process)
            self.assertEqual(
                child,
                {
                    "PATH": "/usr/bin",
                    "LANG": "C.UTF-8",
                    "SSL_CERT_FILE": "/safe/ca.pem",
                    "HTTPS_PROXY": "https://proxy.example",
                    "OPENAI_API_KEY": VALID_KEY,
                    "OPENAI_BASE_URL": "https://api.openai.com/v1",
                    "OPENAI_MODEL": "gpt-5.6-luna",
                },
            )
            self.assertEqual(redaction_canaries(child), (VALID_KEY,))

    def test_ignored_dotenv_supports_basic_quoted_and_unquoted_values(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _repo(root)
            (root / ".env").write_text(
                "# provider only\n"
                f'export OPENAI_API_KEY="{VALID_KEY}" # secret\n'
                "OPENAI_BASE_URL='https://api.openai.com/v1'\n"
                "UNRELATED_TOKEN=ignored-value\n",
                encoding="utf-8",
            )
            child = live_provider_environment(root, "gpt-5.6-luna", {"PATH": "/bin"})
            self.assertEqual(child["OPENAI_API_KEY"], VALID_KEY)
            self.assertEqual(child["OPENAI_BASE_URL"], "https://api.openai.com/v1")
            self.assertNotIn("UNRELATED_TOKEN", child)

    def test_missing_malformed_and_duplicate_keys_fail_without_echoing_secret(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _repo(root)
            with self.assertRaisesRegex(ValueError, "required"):
                live_provider_environment(root, "gpt-5.6-luna", {})
            for malformed in ("short", "sk-key with space", "your-api-key-here"):
                with self.subTest(malformed=malformed):
                    with self.assertRaises(ValueError) as raised:
                        live_provider_environment(
                            root, "gpt-5.6-luna", {"OPENAI_API_KEY": malformed}
                        )
                    self.assertNotIn(malformed, str(raised.exception))

            (root / ".env").write_text(
                f"OPENAI_API_KEY={VALID_KEY}\nOPENAI_API_KEY={VALID_KEY}x\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "duplicate OPENAI_API_KEY"):
                live_provider_environment(root, "gpt-5.6-luna", {})

    def test_cross_source_duplicate_unignored_symlink_and_oversize_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _repo(root)
            dotenv = root / ".env"
            dotenv.write_text(f"OPENAI_API_KEY={VALID_KEY}\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "duplicate OPENAI_API_KEY"):
                live_provider_environment(
                    root, "gpt-5.6-luna", {"OPENAI_API_KEY": VALID_KEY + "x"}
                )
            dotenv.unlink()
            target = root / "provider.env"
            target.write_text(f"OPENAI_API_KEY={VALID_KEY}\n", encoding="utf-8")
            try:
                os.symlink(target.name, dotenv)
            except (OSError, NotImplementedError) as error:
                self.skipTest(f"symlinks unavailable: {error}")
            with self.assertRaisesRegex(ValueError, "symlink"):
                live_provider_environment(root, "gpt-5.6-luna", {})
            dotenv.unlink()
            dotenv.write_bytes(b"#" * (MAX_DOTENV_BYTES + 1))
            with self.assertRaisesRegex(ValueError, "size limit"):
                live_provider_environment(root, "gpt-5.6-luna", {})

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _repo(root, ignore_dotenv=False)
            (root / ".env").write_text(f"OPENAI_API_KEY={VALID_KEY}\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "must be ignored"):
                live_provider_environment(root, "gpt-5.6-luna", {})

    def test_malformed_dotenv_url_model_and_credential_proxy_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _repo(root)
            (root / ".env").write_text(
                f'OPENAI_API_KEY="{VALID_KEY}\n', encoding="utf-8"
            )
            with self.assertRaisesRegex(ValueError, "unterminated"):
                live_provider_environment(root, "gpt-5.6-luna", {})
            (root / ".env").unlink()
            with self.assertRaisesRegex(ValueError, "BASE_URL"):
                live_provider_environment(
                    root,
                    "gpt-5.6-luna",
                    {
                        "OPENAI_API_KEY": VALID_KEY,
                        "OPENAI_BASE_URL": "https://user:secret@example.com/v1",
                    },
                )
            with self.assertRaisesRegex(ValueError, "model identifier"):
                live_provider_environment(root, "bad model", {"OPENAI_API_KEY": VALID_KEY})
            with self.assertRaisesRegex(ValueError, "credential-bearing proxy"):
                live_provider_environment(
                    root,
                    "gpt-5.6-luna",
                    {
                        "OPENAI_API_KEY": VALID_KEY,
                        "HTTPS_PROXY": "https://user:secret@proxy.example",
                    },
                )


if __name__ == "__main__":
    unittest.main()

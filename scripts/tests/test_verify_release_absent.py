import io
import sys
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from verify_release_absent import ReleasePresenceError, require_absent  # noqa: E402


class Response:
    status = 200

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False

    def read(self, _limit):
        return b'{"tag_name":"v1.2.3"}'


class ReleaseAbsenceTests(unittest.TestCase):
    @patch("urllib.request.urlopen", return_value=Response())
    def test_existing_release_fails_closed(self, _open):
        with self.assertRaisesRegex(ReleasePresenceError, "already exists"):
            require_absent("owner/repo", "v1.2.3", "token")

    @patch("urllib.request.urlopen")
    def test_only_not_found_proves_absence(self, open_request):
        open_request.side_effect = urllib.error.HTTPError(
            "https://api.github.test", 404, "Not Found", {}, io.BytesIO()
        )
        require_absent("owner/repo", "v1.2.3", "token")

    @patch("urllib.request.urlopen")
    def test_permission_and_network_failures_fail_closed(self, open_request):
        open_request.side_effect = urllib.error.HTTPError(
            "https://api.github.test", 403, "Forbidden", {}, io.BytesIO()
        )
        with self.assertRaisesRegex(ReleasePresenceError, "HTTP 403"):
            require_absent("owner/repo", "v1.2.3", "token")
        open_request.side_effect = urllib.error.URLError("offline")
        with self.assertRaisesRegex(ReleasePresenceError, "failed closed"):
            require_absent("owner/repo", "v1.2.3", "token")

    def test_rejects_unsafe_inputs_before_network(self):
        with self.assertRaisesRegex(ReleasePresenceError, "owner/name"):
            require_absent("owner/repo/extra", "v1.2.3", "token")
        with self.assertRaisesRegex(ReleasePresenceError, "stable"):
            require_absent("owner/repo", "latest", "token")
        with self.assertRaisesRegex(ReleasePresenceError, "GH_TOKEN"):
            require_absent("owner/repo", "v1.2.3", "")


if __name__ == "__main__":
    unittest.main()

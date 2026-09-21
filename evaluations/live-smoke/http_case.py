"""Single-process local HTTP acceptance, explicitly not a replica test."""
import json
import socket
import time
import urllib.error
import urllib.request
import uuid

from process import Process
from support import SmokeError, classify_failure, reject_secrets


def request(base, method, path, deadline, body=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if method == "POST" and path.endswith("/run"):
        headers["Idempotency-Key"] = f"ic048-{uuid.uuid4().hex}"
    query = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    # Never proxy the local HTTP control channel or follow a server redirect.
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args):
            return None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        response = opener.open(query, timeout=min(2, deadline.remaining()))
    except urllib.error.HTTPError as error:
        response = error
    with response:
        raw = response.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise SmokeError("http_response_limit")
        try:
            return response.status, json.loads(raw)
        except (ValueError, UnicodeError):
            raise SmokeError("http_invalid_json") from None


def run_http(binary, directory, environment, canaries, deadline, driver):
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    server = Process([str(binary), "serve", "--host", "127.0.0.1", "--port", str(port),
                      "--flows-dir", str(directory.parent)], directory.parent, environment, canaries)
    try:
        startup_end = time.monotonic() + min(15, deadline.remaining())
        while True:
            server.check()
            if server.child.poll() is not None:
                raise SmokeError("http_startup_failed")
            try:
                code, _ = request(base, "GET", "/health", deadline)
                if code == 200:
                    break
            except (urllib.error.URLError, TimeoutError):
                pass
            if time.monotonic() > startup_end:
                raise SmokeError("http_startup_timeout")
            time.sleep(0.05)
        flow = directory.name
        code, accepted = request(base, "POST", f"/flows/{flow}/run", deadline, {})
        if code not in (200, 202) or not isinstance(accepted.get("run_id"), str):
            raise SmokeError("http_run_not_accepted")
        run_id = accepted["run_id"]
        # Server-generated ids must remain path segments, never URLs.
        if not run_id or any(c not in "0123456789abcdef-" for c in run_id):
            raise SmokeError("http_invalid_run_id")
        answered = set()
        while True:
            server.check()
            code, record = request(base, "GET", f"/flows/{flow}/runs/{run_id}", deadline)
            status = str(record.get("status", "")).lower()
            if code == 200 and status in ("success", "failed", "partial_failure", "partialfailure", "aborted"):
                raw = json.dumps(record).encode()
                reject_secrets(raw, canaries)
                if status != "success":
                    raise SmokeError(classify_failure(raw))
                return record
            if code not in (200, 404):
                raise SmokeError("http_terminal_read_failed")
            code, pending = request(base, "GET", f"/flows/{flow}/questions/{run_id}", deadline)
            if code == 200:
                questions = pending.get("questions", [])
                if not isinstance(questions, list) or len(questions) > 4:
                    raise SmokeError("unexpected_human_question")
                for question in questions:
                    question_id = question.get("question_id")
                    if not isinstance(question_id, str) or len(question_id) > 128:
                        raise SmokeError("invalid_question_id")
                    if question_id in answered:
                        continue
                    answer = driver.answer(question.get("prompt", ""), question.get("kind", ""))
                    status, _ = request(base, "POST", f"/flows/{flow}/answer/{run_id}", deadline,
                                        {"question_id": question_id, "answer": answer})
                    if status not in (200, 202):
                        raise SmokeError("human_answer_not_accepted")
                    answered.add(question_id)
            elif code not in (404, 429):
                raise SmokeError("human_questions_read_failed")
            time.sleep(min(0.25, deadline.remaining()))
    finally:
        server.close()
        server.check()
        if server.forced:
            raise SmokeError("http_forced_shutdown")

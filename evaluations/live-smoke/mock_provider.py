"""Synthetic Responses fixture: transport validation, never live evidence."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading
import uuid


class MockProvider:
    def __init__(self, fault=None):
        self.requests = 0
        self.fault = fault
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                if self.path != "/v1/responses" or not 1 <= length <= 32768:
                    self.send_error(400)
                    return
                body = json.loads(self.rfile.read(length))
                owner.requests += 1
                # Built-in schemas have optional fields and are not strict-mode
                # schemas. Mirror the native rejection that IC-048 uncovered.
                if any(tool.get("strict") is not False for tool in body.get("tools", [])
                       if tool.get("type") == "function"):
                    self.send_error(400)
                    return
                if owner.requests > 47:
                    self.send_error(429)
                    return
                if owner.fault == "outage":
                    self.send_error(503)
                    return
                response = owner.reply(body)
                if body.get("stream"):
                    text = response["output"][0]["content"][0]["text"]
                    events = [("response.output_text.delta", {"delta": text}),
                              ("response.completed", {"response": response})]
                    raw = "".join(f"event: {event}\ndata: {json.dumps(data)}\n\n"
                                  for event, data in events).encode()
                    content_type = "text/event-stream"
                else:
                    raw = json.dumps(response).encode()
                    content_type = "application/json"
                self.send_response(200)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def reply(self, body):
        instructions = body.get("instructions", "")
        who = next((name for name in ("writer", "reviewer")
                    if f"IC048_AGENT:{name}" in instructions), None)
        outputs = [item for item in body.get("input", []) if item.get("type") == "function_call_output"]
        text = "A bounded fixture reply."
        item = None
        if who is not None:
            if len(outputs) < 2:
                name = "ask_human" if not outputs else "file_write"
                arguments = ({"question": "Is the disposable fixture ready?", "timeout_s": 15}
                             if not outputs else {"path": f"output/{who}.txt", "content": "smoke fixture"})
                item = {"type": "function_call", "name": name, "call_id": "call_" + uuid.uuid4().hex,
                        "arguments": json.dumps(arguments)}
            else:
                text = json.dumps({"performed": who == "writer", "summary": "Fixture outcome confirmed."})
        if self.fault == "blank":
            text, item = "  ", None
        usage = {"input_tokens": 100, "output_tokens": 10, "total_tokens": 110,
                 "input_tokens_details": {"cached_tokens": 0},
                 "output_tokens_details": {"reasoning_tokens": 0}}
        if self.fault == "missing_usage":
            usage = None
        return {"id": "resp_fixture", "status": "completed", "usage": usage,
                "output": [item or {"type": "message", "role": "assistant",
                                    "content": [{"type": "output_text", "text": text}]}]}

    def __enter__(self):
        self.thread.start()
        return f"http://127.0.0.1:{self.server.server_port}"

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

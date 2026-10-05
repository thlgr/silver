#!/usr/bin/env python3
"""End-to-end check of silver's ai-memory lifecycle capture, against a stub ai-memory server.

A native run in a workspace must (a) claim the project handoff at run start and inject it into the
system prompt, and (b) post its session-start, prompt, tool calls and end to `/hook/batch` tagged
`extension=silver`, scoped to the workspace's project. The stub stands in for the real server, so
this runs anywhere; a stub on the configured bind is adopted, exactly as a real one would be.
Needs `cargo build -p silver`."""
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STUB_PORT, MOCK_PORT, DAEMON_PORT = 18095, 18096, 18097
BASE = f"http://127.0.0.1:{DAEMON_PORT}"
HANDOFF = "**Next steps**\n- finish the parser\n\n<!-- ai-memory:untrusted-history:start -->\nquoted\n<!-- ai-memory:untrusted-history:end -->"
PROMPT_PATH = "/tmp/silver-system-prompt.txt"
SEEN = {"handoff": None, "events": []}


class Stub(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _send(self, code, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = urllib.parse.urlparse(self.path)
        if path.path == "/healthz":
            return self._send(200, {})
        if path.path == "/handoff":
            SEEN["handoff"] = urllib.parse.parse_qs(path.query)
            return self._send(200, HANDOFF.encode(), "text/plain")
        self._send(404, {})

    def do_POST(self):
        path = urllib.parse.urlparse(self.path)
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b""
        if path.path == "/hook/batch":
            items = json.loads(raw or b"[]")
            for item in items:
                query = urllib.parse.parse_qs(urllib.parse.urlparse(item["url"]).query)
                SEEN["events"].append((query.get("event", [""])[0], query, item.get("body", {})))
            return self._send(200, {"accepted": len(items)})
        self._send(404, {})


def call(method, path, body=None, base=BASE):
    request = urllib.request.Request(
        base + path,
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        text = response.read().decode()
        return json.loads(text) if text else None


def until(what, check, seconds=30):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            value = check()
        except OSError:
            value = None
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {what}")


def event_names():
    return [name for name, _, _ in SEEN["events"]]


def main():
    work = tempfile.mkdtemp(prefix="silver-ai-memory-e2e-")
    project = os.path.join(work, "proj")
    config = os.path.join(work, "cfg")
    os.makedirs(project)
    os.makedirs(config)
    with open(os.path.join(project, ".ai-memory.toml"), "w") as f:
        f.write('workspace = "default"\nproject = "memtest"\n')
    with open(os.path.join(config, "config.toml"), "w") as f:
        f.write(
            f'[model]\nprovider = "custom"\nkind = "openai_compatible"\nname = "mock-model"\n'
            f'base_url = "http://127.0.0.1:{MOCK_PORT}/v1"\napi_key_env = "SILVER_API_KEY"\n\n'
            "[tools]\nwrite_requires_approval = false\ncommand_requires_approval = false\n\n"
            # The stub on this bind stands in for ai-memory; silver adopts it like a real one.
            f'[memory]\nenabled = true\nbind = "127.0.0.1:{STUB_PORT}"\ndata_dir = "{work}/mem"\n'
        )
    with open(os.path.join(config, "secrets.env"), "w") as f:
        f.write("SILVER_API_KEY=dummy\n")

    stub = ThreadingHTTPServer(("127.0.0.1", STUB_PORT), Stub)
    threading.Thread(target=stub.serve_forever, daemon=True).start()
    until("the stub", lambda: call("GET", "/healthz", base=f"http://127.0.0.1:{STUB_PORT}") is not None, 10)

    procs = [
        subprocess.Popen([sys.executable, os.path.join(ROOT, "scripts/mock_openai_server.py"), str(MOCK_PORT)]),
        subprocess.Popen(
            [os.path.join(ROOT, "target/debug/silver"), "--bind", f"127.0.0.1:{DAEMON_PORT}",
             "--data-dir", os.path.join(work, "data")],
            env={**os.environ, "SILVER_CONFIG_DIR": config},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        ),
    ]
    try:
        until("the daemon", lambda: call("GET", "/health") is not None, 30)
        workspace = call("POST", "/v1/workspaces", {"name": "memtest", "path": project})["id"]
        body = {"workspace_id": workspace,
                "message": {"content": [{"type": "text", "text": "please run a shell command"}]}}
        run_id = call("POST", "/v1/runs", body)["run_id"]
        until("the run", lambda: call("GET", f"/v1/runs/{run_id}")["status"] in
              ("completed", "failed", "cancelled"), 60)

        # Graceful shutdown flushes the queue, which delivers the session-end.
        procs[1].send_signal(signal.SIGTERM)
        procs[1].wait(timeout=15)

        names = event_names()
        for expected in ("session-start", "user-prompt-submit", "post-tool-use", "stop", "session-end"):
            assert expected in names, f"missing {expected}; saw {names}"

        handoff = SEEN["handoff"]
        assert handoff is not None, "the run never claimed the project handoff"
        assert handoff["workspace"] == ["default"] and handoff["project"] == ["memtest"], handoff
        assert handoff["agent"] == ["silver"], handoff
        prompt = open(PROMPT_PATH).read()
        assert "finish the parser" in prompt, "the handoff never reached the system prompt"

        for name, query, _ in SEEN["events"]:
            assert query["extension"] == ["silver"] and query["agent"] == ["silver"], name
            assert query["workspace"] == ["default"] and query["project"] == ["memtest"], name
            assert len(query["ingest_key"][0]) == 64, name
        tool = next(body for name, _, body in SEEN["events"] if name == "post-tool-use")
        assert tool["tool_name"] == "bash" and "hi-from-bash" in json.dumps(tool), tool
        print("ai-memory e2e: ok")
    finally:
        for proc in procs:
            proc.send_signal(signal.SIGTERM)
        for proc in procs:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        stub.shutdown()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Mock OpenAI-compatible streaming endpoint for silver end-to-end tests.

Emulates DeepSeek's thinking-mode contract: any assistant message that carries
tool_calls MUST echo back its reasoning_content or the request is answered with
HTTP 400. Branches on the latest user message until a tool result is present:
  'shell' -> bash, 'todo' -> todo_list, 'file' -> write_file, 'delegate' -> delegate_task
  with one subagent task, 'picture' -> view_image, 'document' -> search_documents, else text.
"""
import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
# What the OpenCode Go usage endpoint reports; POST /set-usage/<percent> changes it.
USAGE = {"percent": 10}


def event(obj):
    return ("data: " + json.dumps(obj) + "\n\n").encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _read_json(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            return json.loads(raw or b"{}")
        except Exception:
            return {}

    def _json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.endswith("/models"):
            return self._json({"object": "list", "data": [{"id": "mock-model", "object": "model"}]})
        if self.path.endswith("/zen/go/v1/usage"):
            window = lambda percent: {"status": "ok", "percent": percent, "resetsAt": "2099-01-01T00:00:00.000Z"}
            return self._json({"usage": {"rolling": window(USAGE["percent"]), "weekly": window(39), "monthly": window(19)}})
        self.send_response(404)
        self.end_headers()

    def _reject(self, message):
        body = json.dumps({"error": {"message": message}}).encode()
        self.send_response(400)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path.startswith("/set-usage/"):
            USAGE["percent"] = int(self.path.rsplit("/", 1)[1])
            return self._json(USAGE)
        if not self.path.endswith("/chat/completions"):
            self.send_response(404)
            self.end_headers()
            return
        request = self._read_json()
        messages = request.get("messages", [])
        for message in messages:
            if message.get("role") == "system":
                try:
                    with open("/tmp/silver-system-prompt.txt", "w") as handle:
                        handle.write(message.get("content") or "")
                except Exception:
                    pass
                break
        try:
            names = [
                tool.get("function", {}).get("name")
                for tool in request.get("tools", [])
                if isinstance(tool, dict)
            ]
            with open("/tmp/silver-tools-sent.txt", "w") as handle:
                handle.write("\n".join(str(n) for n in names))
        except Exception:
            pass

        for message in messages:
            if (
                message.get("role") == "assistant"
                and message.get("tool_calls")
                and "reasoning_content" not in message
            ):
                self._reject(
                    "The `reasoning_content` in the thinking mode must be passed back to the API."
                )
                return

        last_user = ""
        images = 0
        image_bytes = 0
        for message in messages:
            if message.get("role") == "user":
                content = message.get("content")
                if isinstance(content, str):
                    last_user = content
                elif isinstance(content, list):
                    for part in content:
                        if not isinstance(part, dict):
                            continue
                        last_user += part.get("text", "")
                        if part.get("type") == "image_url":
                            images += 1
                            image_bytes = len(part.get("image_url", {}).get("url", ""))
        has_tool_result = any(message.get("role") == "tool" for message in messages)
        lowered = last_user.lower()

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def write_chunk(data):
            self.wfile.write(("%X\r\n" % len(data)).encode() + data + b"\r\n")
            self.wfile.flush()

        def emit(delta, finish=None):
            write_chunk(event({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }))

        def tool(name, arguments, call_id):
            emit({"reasoning_content": "I should call " + name + "."})
            emit({"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                                  "function": {"name": name, "arguments": json.dumps(arguments)}}]})
            emit({}, "tool_calls")

        if not has_tool_result and "delegate" in lowered:
            # "delegate this to the <name> agent" picks the subagent, so a bad name exercises
            # the refusal path.
            agent = re.search(r"to the ([a-z-]+) agent", lowered)
            tool("delegate_task", {"tasks": [
                {"agent": agent.group(1) if agent else "general-purpose",
                 "description": "look around",
                 "prompt": "Look for the greeting the app prints. Report it."}
            ]}, "call_delegate_1")
        elif not has_tool_result and "shell" in lowered:
            tool("bash", {"command": "printf hi-from-bash"}, "call_shell_1")
        elif not has_tool_result and "askbot" in lowered:
            # "askbot bob" has the bot put a request to the bot called bob.
            target = re.search(r"askbot ([a-z0-9]+)", lowered)
            tool("ask_bot", {"bot": target.group(1) if target else "", "message": "Please say hello."}, "call_ask_1")
        elif not has_tool_result and "todo" in lowered:
            tool("todo_list", {"todos": [{"id": "1", "content": "task one", "status": "pending"}]}, "call_todo_1")
        elif not has_tool_result and "file" in lowered:
            tool("write_file", {"path": "note.txt", "content": "hello from mock"}, "call_file_1")
        elif not has_tool_result and ".silver/attachments/" in lowered:
            # An attached file: the model reads the path the composer put in the prompt.
            attached = re.search(r"\.silver/attachments/[^\s\]]+", lowered).group(0)
            if attached.endswith(".pdf"):
                tool("read_file", {"path": attached}, "call_attach_1")
            else:
                tool("view_image", {"path": attached, "question": "what does it show?"}, "call_attach_1")
        elif not has_tool_result and "picture" in lowered:
            tool("view_image", {"path": "shot.png", "question": "what is in it?"}, "call_image_1")
        elif not has_tool_result and "document" in lowered:
            tool("search_documents", {"query": "revenue", "path": "notes.md"}, "call_doc_1")
        elif "slowpoke" in lowered:
            # A reply that takes a while, so a test can stop it halfway.
            for index in range(40):
                emit({"content": "tick %d " % index})
                time.sleep(0.15)
            emit({}, "stop")
        elif images:
            # The follow-up turn carries the picture the tool loaded; report that it arrived.
            text = "Mock reply: I can see the image (%d characters of data URL)." % image_bytes
            for index in range(0, len(text), 8):
                emit({"content": text[index:index + 8]})
                time.sleep(0.02)
            emit({}, "stop")
        elif "group chat" in lowered and "you are " in lowered:
            # A room turn: answer the user, pass on everyone else's replies; "stay silent" passes.
            who = re.search(r"you are ([^,]+),", last_user, re.I).group(1)
            # Only the user's own message in the room is worth answering, not the replies to it.
            text = who + " here: on it." if "\nuser:" in lowered and "stay silent" not in lowered else "(pass)"
            for index in range(0, len(text), 8):
                emit({"content": text[index:index + 8]})
                time.sleep(0.02)
            emit({}, "stop")
        else:
            text = "Mock reply: you said '" + last_user.strip() + "'."
            for index in range(0, len(text), 8):
                emit({"content": text[index:index + 8]})
                time.sleep(0.02)
            emit({}, "stop")
        write_chunk(b"data: [DONE]\n\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


class Server(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        # A client that hangs up mid-stream (a stopped run) is not worth a traceback.
        pass


if __name__ == "__main__":
    Server(("127.0.0.1", PORT), Handler).serve_forever()

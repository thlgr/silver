#!/usr/bin/env python3
"""A stand-in ACP agent for chat_e2e.py. It answers "ok", and before a prompt that says "write" it
asks leave to write a file, then reports what it was told: "allowed:<option>" or "refused:<option>"."""
import json
import os
import sys

sessions = {}


def send(obj):
    print(json.dumps(obj), flush=True)


def read_answer(mid):
    """The answer to request `mid`; a notice that arrives first, such as a cancel, is skipped."""
    while True:
        msg = json.loads(sys.stdin.readline())
        if msg.get("id") == mid and "method" not in msg:
            return msg


for line in sys.stdin:
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        sid = f"s{len(sessions) + 1}"
        sessions[sid] = msg["params"].get("cwd", os.getcwd())
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": sid}})
    elif method == "session/prompt":
        sid = msg["params"]["sessionId"]
        text = msg["params"]["prompt"][0]["text"]
        reply = f"ok in {sessions[sid]}"
        if "write" in text:
            send({
                "jsonrpc": "2.0", "id": 900, "method": "session/request_permission",
                "params": {
                    "sessionId": sid,
                    "toolCall": {"toolCallId": "t1", "title": "Write note.txt", "kind": "edit",
                                 "rawInput": {"file_path": "note.txt", "content": "hi"}},
                    "options": [
                        {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
                        {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                    ],
                },
            })
            answer = read_answer(900)
            chosen = answer["result"]["outcome"].get("optionId", "cancelled")
            reply = ("allowed:" if chosen in ("yes", "always") else "refused:") + chosen
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {
            "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": reply}}}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    elif method == "session/cancel":
        pass
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})

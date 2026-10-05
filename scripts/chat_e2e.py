#!/usr/bin/env python3
"""End-to-end check of the bot chat against the mock model: bots, replies, read marks, reactions,
threads, group rooms, bots asking each other, stop and delete. Needs `cargo build -p silver`."""
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MOCK_PORT, DAEMON_PORT = 18092, 18093
BASE = f"http://127.0.0.1:{DAEMON_PORT}"


def call(method, path, body=None):
    request = urllib.request.Request(
        BASE + path,
        method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"content-type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        text = response.read().decode()
        return json.loads(text) if text else None


def until(what, check, seconds=20):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            value = check()
        except OSError:  # the daemon is still starting
            value = None
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {what}")


def bot(name, **fields):
    return call("POST", "/v1/chat/bots", {"name": name, **fields})["id"]


def entries(chat, thread=None):
    path = f"/v1/chat/bots/{chat}/entries" + (f"?thread={thread}" if thread else "")
    return call("GET", path)["entries"]


def roster():
    return {b["name"]: b for b in call("GET", "/v1/chat/bots")["bots"]}


def idle(*names):
    return lambda: all(roster()[n]["status"] == "idle" for n in names)


def said(chat, kind="agent", thread=None):
    return [e for e in entries(chat, thread) if e["kind"] == kind and e["final"]]


def follow_entries(live, seen):
    """Keep the latest text of each entry the live stream carries, as a client would show it,
    and every text it ever carried, however briefly."""
    with urllib.request.urlopen(BASE + "/v1/chat/events") as stream:
        for line in stream:
            if line.startswith(b"data:"):
                event = json.loads(line[5:])
                if event["type"] == "entry":
                    live[event["entry"]["id"]] = event["entry"]["text"]
                    seen.add(event["entry"]["text"])


WORK = tempfile.mkdtemp(prefix="silver-chat-e2e-")


def main():
    work = WORK
    config = os.path.join(work, "cfg")
    os.makedirs(config)
    with open(os.path.join(config, "config.toml"), "w") as f:
        f.write(
            f'[model]\nprovider = "custom"\nkind = "openai_compatible"\nname = "mock-model"\n'
            f'base_url = "http://127.0.0.1:{MOCK_PORT}/v1"\napi_key_env = "SILVER_API_KEY"\n\n'
            "[tools]\nwrite_requires_approval = false\ncommand_requires_approval = true\n"
        )
    with open(os.path.join(config, "secrets.env"), "w") as f:
        f.write("SILVER_API_KEY=dummy\n")
    # No Claude Code sign-in here, so the daemon never asks Anthropic for a reading.
    env = {**os.environ, "SILVER_CONFIG_DIR": config, "CLAUDE_CONFIG_DIR": os.path.join(work, "claude")}
    procs = [
        subprocess.Popen([sys.executable, os.path.join(ROOT, "scripts/mock_openai_server.py"), str(MOCK_PORT)]),
        subprocess.Popen(
            [os.path.join(ROOT, "target/debug/silver"), "--bind", f"127.0.0.1:{DAEMON_PORT}",
             "--data-dir", os.path.join(work, "data")],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        ),
    ]
    try:
        until("the daemon", lambda: call("GET", "/health"), 30)
        scenarios()
        print("chat e2e: ok")
    finally:
        for proc in procs:
            proc.send_signal(signal.SIGTERM)
        for proc in procs:
            proc.wait(timeout=10)
        shutil.rmtree(work, ignore_errors=True)


def scenarios():
    assert call("GET", "/v1/chat/bots") == {"bots": []}
    alice, bob = bot("Alice", description="Reviews code"), bot("Bob")
    assert roster()["Alice"]["avatar_shape"], "an avatar is chosen from the name"

    # A message gets one final reply; it is unread until the chat is read.
    sent = call("POST", f"/v1/chat/bots/{alice}/send", {"text": "hello there", "nonce": "n1"})
    until("alice's reply", lambda: said(alice))
    until("alice idle", idle("Alice"))
    reply = said(alice)[0]
    assert reply["text"] == "Mock reply: you said 'hello there'." and reply["author"] == alice
    assert roster()["Alice"]["unread"] == 1
    call("POST", f"/v1/chat/bots/{alice}/read", {})
    assert roster()["Alice"]["unread"] == 0
    again = call("POST", f"/v1/chat/bots/{alice}/send", {"text": "hello there", "nonce": "n1"})
    assert again["id"] == sent["id"] and len(said(alice, "user")) == 1, "a retry is not a second message"

    # A reaction toggles.
    assert call("POST", f"/v1/chat/entries/{reply['id']}/react", {"emoji": "👍"})["reactions"] == ["👍"]
    assert "reactions" not in call("POST", f"/v1/chat/entries/{reply['id']}/react", {"emoji": "👍"})

    # A thread is its own conversation; the main chat does not see it.
    call("POST", f"/v1/chat/bots/{alice}/send", {"text": "and in a thread", "thread_id": reply["id"]})
    until("the thread reply", lambda: said(alice, thread=reply["id"]))
    until("alice idle", idle("Alice"))
    threaded = said(alice, thread=reply["id"])[0]
    assert "thread on one message" in threaded["text"], threaded["text"]
    assert threaded["session_id"] != reply["session_id"], "a thread has a session of its own"
    main_chat = entries(alice)
    assert all(e["thread_id"] is None for e in main_chat if "thread_id" in e)
    root = next(e for e in main_chat if e["id"] == reply["id"])
    assert root["thread"]["count"] == 2 and root["thread"]["authors"] == ["user", alice], root["thread"]
    assert root["thread"]["unread"] == 1
    call("POST", f"/v1/chat/bots/{alice}/read", {"thread_id": reply["id"]})
    assert next(e for e in entries(alice) if e["id"] == reply["id"])["thread"]["unread"] == 0

    # A group: everyone answers once; an @mention picks one.
    team = bot("Team", kind="group", members=[alice, bob])
    assert bot("Team again", kind="group", members=[bob, alice]) == team, "the same members share a group"
    call("POST", f"/v1/chat/bots/{team}/send", {"text": "hi all"})
    until("both answered", lambda: len(said(team)) == 2)
    until("everyone idle", idle("Alice", "Bob"))
    assert sorted(e["author"] for e in said(team)) == sorted([alice, bob])
    assert {e["text"] for e in said(team)} == {"Alice here: on it.", "Bob here: on it."}
    assert roster()["Team"]["last_message"].endswith("here: on it.")
    call("POST", f"/v1/chat/bots/{team}/send", {"text": "@bob just you"})
    until("bob alone", lambda: len(said(team)) == 3)
    until("everyone idle", idle("Alice", "Bob"))
    time.sleep(0.5)
    assert len(said(team)) == 3 and said(team)[-1]["author"] == bob

    # A thread in a group is its own room: the members answer inside it, the main chat stays.
    root = said(team)[0]
    main_before = len(entries(team))
    call("POST", f"/v1/chat/bots/{team}/send", {"text": "and in a thread?", "thread_id": root["id"]})
    until("the room answers in the thread", lambda: len(said(team, thread=root["id"])) == 2)
    until("everyone idle", idle("Alice", "Bob"))
    assert len(entries(team)) == main_before, "a thread's messages are not in the main chat"
    assert next(e for e in entries(team) if e["id"] == root["id"])["thread"]["count"] == 3

    # A bot with a folder that needs approval shows a card and waits for the answer.
    folder = os.path.join(WORK, "folder")
    os.makedirs(folder)
    workspace = call("POST", "/v1/workspaces", {"name": "folder", "path": folder})["id"]
    coder = bot("Coder", workspace_id=workspace)
    call("POST", f"/v1/chat/bots/{coder}/send", {"text": "please run a shell command"})
    until("the card", lambda: roster()["Coder"]["status"] == "needs_input")
    card = next(e for e in entries(coder) if e["kind"] == "permission")
    assert card["permission"]["status"] == "pending" and card["permission"]["tool"] == "bash"
    assert roster()["Coder"]["activity"].startswith("Needs your approval")
    call("POST", f"/v1/chat/entries/{card['id']}/answer", {"decision": "approve"})
    until("coder done", idle("Coder"))
    assert next(e for e in entries(coder) if e["kind"] == "permission")["permission"]["status"] == "approved"
    assert said(coder), "the run went on after the approval"

    # A run shows only its final reply, and only once the turn ends: text written before a tool
    # call is the agent's running commentary and never reaches the bubble.
    live = {}
    seen = set()
    threading.Thread(target=follow_entries, args=(live, seen), daemon=True).start()
    time.sleep(0.5)
    writer = bot("Writer", workspace_id=workspace)
    call("POST", f"/v1/chat/bots/{writer}/send", {"text": "tailcut please"})
    until("the card", lambda: roster()["Writer"]["status"] == "needs_input")
    assert not any("Looking at how limits" in text for text in live.values()), "commentary is not streamed"
    card = next(e for e in entries(writer) if e["kind"] == "permission")
    call("POST", f"/v1/chat/entries/{card['id']}/answer", {"decision": "approve"})
    until("writer done", idle("Writer"))
    assert "Mock reply: you said 'tailcut please'." in live.values(), live
    call("DELETE", f"/v1/chat/bots/{writer}")
    call("DELETE", f"/v1/chat/bots/{coder}")

    # A bot on a provider with usage limits stamps them on each reply, is stopped when the session
    # limit crosses 90%, is not started while it stays there, and answers again once it is back
    # under. The mock stands in for OpenCode Go's usage endpoint.
    call("POST", "/v1/auth/opencode-go", {"api_key": "k", "base_url": f"http://127.0.0.1:{MOCK_PORT}/zen/go/v1"})
    usage = lambda percent: urllib.request.urlopen(f"http://127.0.0.1:{MOCK_PORT}/set-usage/{percent}", b"").read()
    # Editing a bot reads its limit at once, so the test need not wait out the minute.
    reread = lambda bot_id: call("PATCH", f"/v1/chat/bots/{bot_id}", {"provider": "opencode-go"})
    limited = bot("Limited", provider="opencode-go", workspace_id=workspace)
    time.sleep(1)  # a new bot has its provider's limits read at once
    call("POST", f"/v1/chat/bots/{limited}/send", {"text": "how much is left"})
    until("a reply", lambda: said(limited))
    assert [(w["name"], w["percent"]) for w in said(limited)[0]["limits"]] == [("Session", 10), ("Week", 39), ("Month", 19)]
    assert "limits" not in said(alice)[0], "a provider with no limit to read stamps nothing"
    # The agent's roster row carries the latest reading too, and follows it.
    assert [(w["name"], w["percent"]) for w in roster()["Limited"]["limits"]] == [("Session", 10), ("Week", 39), ("Month", 19)]
    assert "limits" not in roster()["Alice"]
    call("POST", f"/v1/chat/bots/{limited}/send", {"text": "please run a shell command"})
    until("the card", lambda: roster()["Limited"]["status"] == "needs_input")
    usage(95)
    reread(limited)
    until("stopped", idle("Limited"))
    stopped = next(e for e in entries(limited) if e["kind"] == "notice" and e["text"].startswith("Stopped:"))
    assert "95%" in stopped["text"] and "OpenCode Go" in stopped["text"], stopped
    assert next(e for e in entries(limited) if e["kind"] == "permission")["permission"]["status"] == "expired"
    assert roster()["Limited"]["limits"][0]["percent"] == 95
    call("POST", f"/v1/chat/bots/{limited}/send", {"text": "hello again"})
    until("held", lambda: any(e["text"].startswith("Not started:") for e in entries(limited)))
    assert len(said(limited)) == 1, "a bot at its limit does not answer"
    usage(40)
    reread(limited)
    time.sleep(1)
    call("POST", f"/v1/chat/bots/{limited}/send", {"text": "hello once more"})
    until("it answers again", lambda: len(said(limited)) == 2)
    assert said(limited)[1]["limits"][0]["percent"] == 40
    call("DELETE", f"/v1/chat/bots/{limited}")

    # An external ACP agent's permission requests become approval cards too, and "approve
    # automatically" answers them itself. The agent here is scripts/fake_acp_agent.py.
    agent = os.path.join(ROOT, "scripts/fake_acp_agent.py")
    call("POST", "/v1/auth/opencode", {"base_url": f"{sys.executable} {agent}", "activate": False})
    outside = bot("Outside", provider="opencode", workspace_id=workspace)
    send_to = lambda bot_id, text: call("POST", f"/v1/chat/bots/{bot_id}/send", {"text": text})
    cards = lambda bot_id: [e for e in entries(bot_id) if e["kind"] == "permission"]
    send_to(outside, "please write the note")
    until("the agent's card", lambda: roster()["Outside"]["status"] == "needs_input")
    card = cards(outside)[0]
    assert card["permission"]["tool"] == "patch" and card["permission"]["description"] == "Write note.txt"
    assert roster()["Outside"]["activity"].startswith("Needs your approval")
    call("POST", f"/v1/chat/entries/{card['id']}/answer", {"decision": "approve"})
    until("allowed", lambda: any(e["text"] == "allowed:yes" for e in said(outside)))
    until("outside idle", idle("Outside"))
    assert cards(outside)[0]["permission"]["status"] == "approved"
    assert any(f"in {folder}" in e["text"] or e["text"] == "allowed:yes" for e in said(outside))
    send_to(outside, "write it again")
    until("a second card", lambda: len(cards(outside)) == 2 and roster()["Outside"]["status"] == "needs_input")
    call("POST", f"/v1/chat/entries/{cards(outside)[1]['id']}/answer", {"decision": "deny"})
    until("refused", lambda: any(e["text"] == "refused:no" for e in said(outside)))
    until("outside idle", idle("Outside"))
    assert cards(outside)[1]["permission"]["status"] == "denied"
    send_to(outside, "write while I stop you")
    until("a third card", lambda: len(cards(outside)) == 3 and roster()["Outside"]["status"] == "needs_input")
    call("POST", f"/v1/chat/bots/{outside}/stop")
    until("stopped", idle("Outside"))
    assert cards(outside)[2]["permission"]["status"] == "expired", "a card left open expires with its turn"
    trusting = bot("Trusting", provider="opencode", workspace_id=workspace, yolo=True)
    send_to(trusting, "write quietly")
    until("allowed without asking", lambda: any(e["text"] == "allowed:yes" for e in said(trusting)))
    assert not cards(trusting), "an automatic bot is never asked"
    call("DELETE", f"/v1/chat/bots/{outside}")
    call("DELETE", f"/v1/chat/bots/{trusting}")

    # Claude's plan usage comes with its turns, so its bar needs no endpoint. The agent is the fake
    # one again, standing in for the Claude adapter.
    call("POST", "/v1/auth/claude", {"base_url": f"{sys.executable} {agent}", "activate": False})
    claude = bot("Claude", provider="claude", workspace_id=workspace)
    assert "limits" not in roster()["Claude"]
    send_to(claude, "showusage")
    until("claude's reading", lambda: roster()["Claude"].get("limits"))
    assert [(w["name"], w["percent"]) for w in roster()["Claude"]["limits"]] == [("Session", 72), ("Week", 31)]
    until("claude idle", idle("Claude"))
    assert [w["percent"] for w in said(claude)[0]["limits"]] == [72, 31], "the reply carries it too"
    call("DELETE", f"/v1/chat/bots/{claude}")

    # One turn can hold several assistant messages (Claude Code sends one per step); the chat
    # shows only the last of them, and never streams the earlier ones.
    twain = bot("Twain", provider="claude", workspace_id=workspace)
    send_to(twain, "twomessages please")
    until("twain done", idle("Twain"))
    assert [e["text"] for e in said(twain)] == ["the last message"], said(twain)
    assert "the first message" not in seen, "an earlier message is never streamed"
    call("DELETE", f"/v1/chat/bots/{twain}")

    # A member whose turn fails, such as an agent at its usage limit, shows the failure once and sits
    # out the rest of the turn; the others carry on.
    broken = bot("Broken", provider="opencode", workspace_id=workspace)
    room = bot("Room", kind="group", members=[broken, alice])
    call("POST", f"/v1/chat/bots/{room}/send", {"text": "status please"})
    failures = lambda: [e for e in entries(room) if e["kind"] == "notice" and e.get("style") == "error"]
    until("the failure", failures)
    until("alice carries on", lambda: any(e["author"] == alice for e in said(room)))
    time.sleep(2)
    assert len(failures()) == 1, "a failing member is asked once per turn"
    assert "session limit" in failures()[0]["text"]
    assert len([e for e in said(room) if e["author"] == alice]) == 1
    # A group is filed under a workspace like a bot, and can be moved into one or out again.
    assert roster()["Room"].get("workspace_id") is None
    call("PATCH", f"/v1/chat/bots/{room}", {"workspace_id": workspace})
    assert roster()["Room"]["workspace_id"] == workspace, "a group takes a workspace"
    call("DELETE", f"/v1/chat/bots/{room}")
    call("DELETE", f"/v1/chat/bots/{broken}")

    # A bot asks another bot, and both chats say so; the answer is not a bubble in the asked chat.
    before = len(said(bob))
    call("POST", f"/v1/chat/bots/{alice}/send", {"text": "askbot bob"})
    until("alice done", lambda: len(said(alice)) >= 2)
    until("idle", idle("Alice", "Bob"))
    asked = [e for e in entries(alice) if e.get("style") == "request"]
    received = [e for e in entries(bob) if e.get("style") == "request"]
    assert asked and asked[-1]["status"] == "completed", asked
    assert received and received[-1]["status"] == "completed", received
    assert asked[-1]["text"].startswith("Asked Bob") and received[-1]["text"].startswith("Request from Alice")
    assert len(said(bob)) == before, "the request is not a message in bob's chat"
    refused = call("POST", f"/v1/chat/bots/{alice}/send", {"text": "askbot alice"})
    until("alice told off", lambda: idle("Alice")())
    assert refused["kind"] == "user"

    # Stop ends a turn and drops what is queued behind it.
    call("POST", f"/v1/chat/bots/{bob}/send", {"text": "slowpoke"})
    until("bob working", lambda: roster()["Bob"]["status"] == "working")
    queued = call("POST", f"/v1/chat/bots/{bob}/send", {"text": "after that"})
    assert queued["status"] == "queued"
    time.sleep(0.6)
    call("POST", f"/v1/chat/bots/{bob}/stop")
    until("bob idle", idle("Bob"))
    after = {e["id"]: e for e in entries(bob)}
    assert after[queued["id"]]["status"] == "cancelled", after[queued["id"]]
    assert not any(e["text"].endswith("after that'.") for e in after.values())

    # Deleting a bot takes it out of its groups.
    call("DELETE", f"/v1/chat/bots/{bob}")
    assert "Bob" not in roster() and roster()["Team"]["members"] == [alice]
    call("DELETE", f"/v1/chat/bots/{alice}")
    assert roster() == {}, "a group left without bots goes too"


if __name__ == "__main__":
    main()

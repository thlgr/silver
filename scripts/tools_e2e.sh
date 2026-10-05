#!/usr/bin/env bash
# Drive real tool calls (bash, todo_list, write_file) through the mock provider.
set -u
WS="$(cd "$(dirname "$0")/.." && pwd)"
cd "$WS"
export CARGO_HOME="$WS/.cargo-home"
export CARGO_TARGET_DIR="$WS/target"
MPORT=18070
DPORT=18071
CFG=/tmp/silver-tools-e2e-cfg
DATA=/tmp/silver-tools-e2e-data
WSDIR=/tmp/silver-tools-e2e-ws

cargo build -p silver >/dev/null 2>&1 || { echo BUILD_FAILED; exit 1; }
rm -rf "$CFG" "$DATA" "$WSDIR"; mkdir -p "$CFG" "$WSDIR"

python3 scripts/mock_openai_server.py "$MPORT" >/tmp/silver-tools-mock.log 2>&1 &
MOCK_PID=$!
cat > "$CFG/config.toml" <<EOF
[model]
provider = "custom"
kind = "openai_compatible"
name = "mock-model"
base_url = "http://127.0.0.1:$MPORT/v1"
api_key_env = "SILVER_API_KEY"

[tools]
write_requires_approval = false
command_requires_approval = false

# Never adopt a real ai-memory on 49374: this script must not write to a real store.
[memory]
enabled = false
EOF
printf 'SILVER_API_KEY=dummy\n' > "$CFG/secrets.env"
SILVER_CONFIG_DIR="$CFG" ./target/debug/silver --bind 127.0.0.1:$DPORT --data-dir "$DATA" >/tmp/silver-tools-daemon.log 2>&1 &
DAEMON_PID=$!
cleanup() { kill "$DAEMON_PID" "$MOCK_PID" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT
for i in $(seq 1 60); do curl -sf "http://127.0.0.1:$DPORT/health" >/dev/null 2>&1 && break; sleep 0.25; done

WSID=$(curl -s -X POST "http://127.0.0.1:$DPORT/v1/workspaces" -H 'content-type: application/json' \
  -d "{\"name\": \"tools\", \"path\": \"$WSDIR\"}" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')

run() {
  local prompt="$1"
  local body
  body=$(python3 -c 'import json,sys; print(json.dumps({"workspace_id": sys.argv[1], "message": {"content": [{"type":"text","text": sys.argv[2]}]}}))' "$WSID" "$prompt")
  local rid
  rid=$(curl -s -X POST "http://127.0.0.1:$DPORT/v1/runs" -H 'content-type: application/json' -d "$body" | python3 -c 'import sys,json; print(json.load(sys.stdin)["run_id"])')
  for i in $(seq 1 100); do
    status=$(curl -s "http://127.0.0.1:$DPORT/v1/runs/$rid" | python3 -c 'import sys,json; print(json.load(sys.stdin)["status"])')
    [ "$status" = "completed" ] || [ "$status" = "failed" ] || [ "$status" = "cancelled" ] && break
    sleep 0.1
  done
  sid=$(curl -s "http://127.0.0.1:$DPORT/v1/runs/$rid" | python3 -c 'import sys,json; print(json.load(sys.stdin)["session_id"])')
  LAST="$sid"
  echo "--- prompt: $prompt (status=$status) ---"
  curl -s "http://127.0.0.1:$DPORT/v1/sessions/$sid/messages" | python3 -c '
import sys, json
for m in json.load(sys.stdin):
    for p in m.get("content", []):
        if p.get("type") == "tool_result":
            print("  tool_result:", p.get("content", "")[:200].replace("\n", " "))
        elif p.get("type") == "text" and m["role"] == "assistant":
            print("  assistant:", p.get("text", "")[:120])
'
}

run "please run a shell command"
run "add a todo item"
run "write a file please"
# A picture: the tool loads it, the follow-up request must carry the image back to the model.
printf '\211PNG\r\n\032\nmock' > "$WSDIR/shot.png"
run "look at this picture and tell me what is in it"
echo "--- the image the browser can fetch ---"
curl -s -o /dev/null -w '  %{http_code} %{content_type}\n' \
  "http://127.0.0.1:$DPORT/v1/workspaces/$WSID/files?path=shot.png"
echo "--- a document the model was not given ---"
printf 'Quarterly revenue report\nTotal revenue: 4200\n' > "$WSDIR/notes.md"
run "find the total revenue in that document"

echo "--- an attached picture, as the composer would store it ---"
printf '\211PNG\r\n\032\nmock' | base64 -w0 > /tmp/attach.b64
curl -s -X POST "http://127.0.0.1:$DPORT/v1/workspaces/$WSID/attachments" -H 'content-type: application/json' \
  -d "{\"name\":\"from-ui.png\",\"data\":\"$(cat /tmp/attach.b64)\"}" | python3 -c '
import sys, json
a = json.load(sys.stdin)
print("  stored at", a["path"], "-", a["bytes"], "bytes")
'
run "here is a screenshot [attached: from-ui.png → .silver/attachments/from-ui.png]"

LAST=""
run "please delegate this: find the greeting"
echo "--- written file ---"; cat "$WSDIR/note.txt" 2>/dev/null || echo "(no note.txt)"

echo "--- a custom project agent, written through the API ---"
curl -s -X POST "http://127.0.0.1:$DPORT/v1/agents?name=greet-reader&scope=project&workspace_id=$WSID" \
  -H 'content-type: application/json' -w ' [%{http_code}]\n' \
  --data-binary '{"markdown":"---\nname: greet-reader\ndescription: Reads the greeting the app prints\ntools: [read_file]\n---\n\nYou read one file and report the greeting.\n"}' \
  | python3 -c 'import sys; raw=sys.stdin.read(); print("  saved:", raw[-8:].strip())'
head -4 "$WSDIR/.silver/agents/greet-reader.md" | sed "s/^/  file: /"
echo "--- a task for an agent that does not exist ---"
run "please delegate this to the shoddy agent"

echo "--- subagent catalogue ---"
curl -s "http://127.0.0.1:$DPORT/v1/agents?workspace_id=$WSID" | python3 -c '
import sys, json
for a in json.load(sys.stdin)["agents"]:
    print("  {}: {} [{}]".format(a["name"], a["description"][:60], a["source"]))
'
echo "--- subagent events of the last run ---"
curl -s "http://127.0.0.1:$DPORT/v1/sessions/$LAST/injected" | python3 -c '
import sys, json
for event in json.load(sys.stdin):
    # EventPayload is flattened into the event, so `type` sits beside the fields.
    kind = event["type"]
    payload = event
    if kind == "subagent.started":
        print("  started:", payload["agent"], "-", payload["description"], "on", payload["model"])
    elif kind == "subagent.completed":
        print("  completed:", payload["status"], payload["tool_uses"], "tool calls in", payload["duration_ms"], "ms")
    elif kind == "subagent.step":
        print("  step:", payload["event"]["type"])
'

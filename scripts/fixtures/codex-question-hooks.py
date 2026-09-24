#!/usr/bin/env python3
"""Stock 0.155.1 hook payload/ancestor fixture, not a Codex implementation."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root = Path(sys.argv[1])
session = "question-fixture-root" if root.name == "fixture" else root.name
transcript = Path(os.environ["CODEX_HOME"]) / "sessions" / (session + ".jsonl")
transcript.parent.mkdir(parents=True, exist_ok=True)
transcript.write_text(json.dumps({"type": "session_meta", "payload": {"id": session, "thread_source": "user"}}) + "\n")
started_turns = set()
completed_turns = set()
ui = "normal"


def render():
    height = os.get_terminal_size().lines
    body = "  ? 1 question" if ui == "question" else ("Action Required" if ui == "overlay" else "Synthetic fixture")
    sys.stdout.write(f"\033[2J\033[H{body}\033[{height-3};1H› Ask Codex\033[{height-1};1H  ? for shortcuts")
    sys.stdout.flush()


def append(kind, turn):
    with transcript.open("a") as stream:
        stream.write(json.dumps({"type": "event_msg", "payload": {"type": kind, "turn_id": turn}}) + "\n")


def hook(event, fields):
    global ui
    fields = dict(fields)
    ui = fields.pop("_ui", ui)
    padding = fields.pop("_padding", 0)
    if padding:
        with transcript.open("a") as stream:
            stream.write(json.dumps({"type": "response_item", "payload": {"text": "synthetic-body-canary" * padding}}) + "\n")
    payload = {
        "hook_event_name": event,
        "session_id": session,
        "transcript_path": str(transcript),
        "turn_id": "question-fixture-turn",
        **fields,
    }
    turn = payload["turn_id"]
    if event == "UserPromptSubmit" and turn not in started_turns:
        append("task_started", turn)
        started_turns.add(turn)
    if event == "Stop" and turn not in completed_turns:
        append("task_complete", turn)
        completed_turns.add(turn)
    render()
    started = time.monotonic()
    result = subprocess.run([os.environ["VT_BIN"], "hook", "codex", event], input=json.dumps(payload), text=True, capture_output=True, timeout=10)
    return {"code": result.returncode, "error": result.stderr, "started": started, "finished": time.monotonic()}


(root / "startup.json").write_text(json.dumps(hook("SessionStart", {"source": "startup"})))
(root / "ready").touch()
render()
while True:
    request = root / "request.json"
    if not request.exists():
        time.sleep(0.01)
        continue
    message = json.loads(request.read_text())
    request.unlink()
    result = hook(message["event"], message.get("fields", {}))
    temporary = root / "result.tmp"
    temporary.write_text(json.dumps(result))
    temporary.replace(root / "result.json")

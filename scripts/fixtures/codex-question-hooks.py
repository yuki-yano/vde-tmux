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
transcript = root / "session.jsonl"
transcript.write_text(json.dumps({"type": "session_meta", "payload": {"id": session, "thread_source": "user"}}) + "\n")


def hook(event, fields):
    payload = {
        "hook_event_name": event,
        "session_id": session,
        "transcript_path": str(transcript),
        "turn_id": "question-fixture-turn",
        **fields,
    }
    started = time.monotonic()
    result = subprocess.run([os.environ["VT_BIN"], "hook", "codex", event], input=json.dumps(payload), text=True, capture_output=True, timeout=10)
    return {"code": result.returncode, "error": result.stderr, "started": started, "finished": time.monotonic()}


hook("SessionStart", {"source": "startup"})
(root / "ready").touch()
print("READY: question hook fixture", flush=True)
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

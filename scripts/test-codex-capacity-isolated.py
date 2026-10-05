#!/usr/bin/env python3
"""Acceptance against stock Codex 0.160.0; all responses and tmux state are isolated."""
import fcntl
import gzip
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

REPO = Path(__file__).resolve().parent.parent
VT = str(Path(os.environ.get("VDE_VT_BIN", REPO / "target/debug/vt")).resolve())
PROMPT_FILE = os.environ.get("VDE_CAPACITY_TEST_PROMPT_FILE")
RECOVERY_PROMPT = Path(PROMPT_FILE).read_text().rstrip("\n") if PROMPT_FILE else None
ROOT = Path(tempfile.mkdtemp(prefix="vde-codex-capacity-"))
BINARY_IDENTITY = {"path": VT, "sha256": hashlib.sha256(Path(VT).read_bytes()).hexdigest()}
(ROOT / "selected-binary.json").write_text(json.dumps(BINARY_IDENTITY, indent=2) + "\n")
print("selected binary:", json.dumps(BINARY_IDENTITY), flush=True)
SOCKET = "vde-codex-capacity-" + str(os.getpid())
ENV = dict(os.environ, CODEX_HOME=str(ROOT / "codex"), XDG_CONFIG_HOME=str(ROOT / "config"),
           XDG_STATE_HOME=str(ROOT / "state"), XDG_RUNTIME_DIR=str(ROOT / "runtime"),
           VDE_TMUX_SOCKET_NAME=SOCKET, ZDOTDIR=str(ROOT / "zdot"),
           PATH=str(Path(VT).parent) + ":" + os.environ["PATH"])
# Exercise normal terminal rendering even when the automation host disables colors.
ENV.pop("NO_COLOR", None)
ENV.update(TERM="xterm-256color", COLORTERM="truecolor")
INTERNAL_RETRY = "--internal-retry" in sys.argv
requests = []
lock = threading.Lock()


def run(*args, check=True):
    r = subprocess.run(args, env=ENV, cwd=ROOT, capture_output=True, text=True, timeout=15)
    if check and r.returncode:
        raise RuntimeError(f"{args}: {r.returncode}: {r.stderr}")
    return r.stdout


def tmux(*args, check=True):
    return run("tmux", "-u", "-L", SOCKET, *args, check=check)


def vt(*args):
    return json.loads(run(VT, *args, "--json"))


def wait(predicate, timeout=20):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return last
        except (subprocess.SubprocessError, KeyError, ValueError):
            pass
        time.sleep(.25)
    raise AssertionError(f"timed out after {timeout}s: {last!r}")


def diagnostic():
    value = vt("daemon", "diagnostics")
    return value["counters"]["codex_capacity_auto_resume"]


def chain():
    values = diagnostic()["chains"]
    return values[0] if values else None


def is_recovery_prompt(prompt):
    return prompt == RECOVERY_PROMPT if RECOVERY_PROMPT is not None else prompt.startswith("The previous request was interrupted")


class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"data":[]}')

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.headers.get("Content-Encoding") == "gzip":
            raw = gzip.decompress(raw)
        body = json.loads(raw)
        users = [item for item in body.get("input", []) if item.get("role") == "user"]
        prompt = " ".join(c.get("text", "") for c in users[-1].get("content", [])) if users else ""
        with lock:
            requests.append(prompt)
            (ROOT / "requests.json").write_text(json.dumps(requests, indent=2))
        if prompt == "CAPACITY_TEST_INTERNAL" and requests.count(prompt) == 1:
            data = json.dumps({"error": {"code": "server_is_overloaded"}}).encode()
            self.send_response(503)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        if prompt.startswith("CAPACITY_TEST_") and prompt not in {"CAPACITY_TEST_INTERNAL", "CAPACITY_TEST_LINK"}:
            events = [{"type": "response.failed", "response": {"id": "capacity-failure", "error": {
                "code": "server_is_overloaded", "message": "Selected model is at capacity. Please try a different model."}}}]
        else:
            text = '{"title":"Capacity fixture"}' if '"thread_source":"system"' in raw.decode() else ("History [fixture link](https://example.test/capacity)" if prompt == "CAPACITY_TEST_LINK" else "CAPACITY_RECOVERY_DONE")
            events = [{"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "id": "reply", "content": [{"type": "output_text", "text": text}]}},
                      {"type": "response.completed", "response": {"id": "success", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}]
        data = "".join("data: " + json.dumps(event) + "\n\n" for event in events).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Mock)
threading.Thread(target=server.serve_forever, daemon=True).start()
client = None
master = None
try:
    for folder in ["codex", "config/vde/tmux", "state", "runtime", "zdot", "project"]:
        (ROOT / folder).mkdir(parents=True, exist_ok=True)
    capacity_config = "sidebar:\n  task_summary:\n    enabled: false\ncodex:\n  capacity_auto_resume:\n    enabled: true\n"
    if RECOVERY_PROMPT is not None:
        capacity_config += "    prompt: " + json.dumps(RECOVERY_PROMPT, ensure_ascii=False) + "\n"
    (ROOT / "config/vde/tmux/config.yml").write_text(capacity_config)
    (ROOT / "codex/config.toml").write_text(f'''model = "gpt-5.4"
model_provider = "fixture"
approval_policy = "never"
sandbox_mode = "read-only"
[features]
codex_hooks = true
[projects.{json.dumps(str(ROOT))}]
trust_level = "trusted"
[model_providers.fixture]
name = "Capacity fixture"
base_url = "http://127.0.0.1:{server.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 1
stream_max_retries = 0
''')
    hooks = {kind: [{"hooks": [{"type": "command", "command": shlex.quote(VT) + " hook codex " + kind}]}]
             for kind in ["SessionStart", "UserPromptSubmit", "Stop"]}
    (ROOT / "codex/hooks.json").write_text(json.dumps({"hooks": hooks}))
    assert run("codex", "--version").strip() == "codex-cli 0.160.0"
    tmux("-f", "/dev/null", "new-session", "-d", "-s", "capacity", "-x", "120", "-y", "40", "exec /bin/sh")
    tmux("set-option", "-g", "status", "off")
    pane = tmux("display-message", "-p", "#{pane_id}").strip()
    ENV["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0").strip()
    run(VT, "daemon", "start")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    client = subprocess.Popen(["tmux", "-u", "-L", SOCKET, "attach-session", "-t", "capacity"], stdin=slave, stdout=slave, stderr=slave, env=dict(ENV, TERM="xterm-256color"))
    os.close(slave)
    def drain():
        pending = b""
        while client.poll() is None:
            if select.select([master], [], [], .2)[0]:
                try:
                    data = os.read(master, 65536)
                    pending = (pending + data)[-131072:]
                    if b"\x1b[c" in pending:
                        os.write(master, b"\x1b[?1;2c")
                        pending = pending.replace(b"\x1b[c", b"")
                    if b"\x1b]11;?" in pending:
                        os.write(master, b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\")
                        pending = pending.replace(b"\x1b]11;?", b"")
                    if b"\x1b]10;?" in pending:
                        os.write(master, b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\")
                        pending = pending.replace(b"\x1b]10;?", b"")
                except OSError:
                    break
    threading.Thread(target=drain, daemon=True).start()
    env_flags = " ".join(shlex.quote(k + "=" + v) for k, v in ENV.items() if k in {"CODEX_HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "VDE_TMUX_SOCKET_NAME", "PATH", "ZDOTDIR", "TERM", "COLORTERM"})
    tmux("respawn-pane", "-k", "-t", pane, "cd " + shlex.quote(str(ROOT / "project")) + "; exec env " + env_flags + " codex --no-daemon --no-alt-screen")
    wait(lambda: "Hooks need review" in tmux("capture-pane", "-p", "-t", pane))
    print("hook-review ready", ROOT, flush=True)
    # Trust only this newly generated fixture's three absolute-path hooks.
    tmux("send-keys", "-t", pane, "Down", "Enter")
    wait(lambda: "Ask Codex to do anything" in tmux("capture-pane", "-p", "-t", pane))
    wait(lambda: any(a["pane_id"] == pane and a["identity"] == "exact"
                     for a in vt("agent", "list")["result"]["agents"]))
    if not INTERNAL_RETRY:
        tmux("send-keys", "-l", "-t", pane, "CAPACITY_TEST_LINK")
        time.sleep(.5)
        tmux("send-keys", "-t", pane, "Enter")
        wait(lambda: (v := vt("agent", "get", pane)) and v["result"]["agent"]["summary"]["status"] == "done" and v)
        linked = tmux("capture-pane", "-p", "-e", "-t", pane)
        assert "\x1b]8;" in linked, repr(linked)
        (ROOT / "linked-ansi.txt").write_text(linked)
        print("stock TUI history includes OSC 8 link", flush=True)
        run(VT, "daemon", "restart")
        recovered = wait(lambda: (v := vt("agent", "get", pane))
                         and v["result"]["agent"]["summary"]["presentation"]["reason"] == "provider_resynchronized" and v)
        reference = recovered["result"]["agent"]["summary"]["agent_ref"]
        resumed = subprocess.run([VT, "agent", "prompt", reference, "--operation-id",
                                  "stock-restart-readiness-0001", "--stdin"],
                                 input="CAPACITY_TEST_LINK\n", env=ENV, cwd=ROOT,
                                 capture_output=True, text=True, timeout=20)
        assert resumed.returncode == 0, resumed.stderr
        (ROOT / "restart-prompt.json").write_text(resumed.stdout)
        wait(lambda: (v := vt("agent", "get", pane))
             and v["result"]["agent"]["summary"]["status"] == "done" and v)
        print("stock daemon restart -> external prompt -> Done passed", flush=True)
    completed_before = vt("agent", "get", pane)["result"]["agent"]["completed_seq"]
    tmux("send-keys", "-l", "-t", pane, "CAPACITY_TEST_INTERNAL" if INTERNAL_RETRY else "CAPACITY_TEST_INITIAL")
    time.sleep(.5)
    tmux("send-keys", "-t", pane, "Enter")
    if INTERNAL_RETRY:
        final = wait(lambda: (v := vt("agent", "get", pane)) and v["result"]["agent"]["summary"]["status"] == "done" and v, 30)
        assert requests.count("CAPACITY_TEST_INTERNAL") == 2
        assert diagnostic()["failures"] == 0 and not diagnostic()["chains"]
        (ROOT / "done.json").write_text(json.dumps(final, indent=2))
        print("native HTTP request retry recovered without auto prompt; passed", ROOT, flush=True)
        sys.exit(0)
    waiting = wait(lambda: (c := chain()) and c["state"] == "waiting" and c)
    assert waiting["attempts"] == 0
    failed = vt("agent", "get", pane)
    assert failed["result"]["agent"]["summary"]["current_run"]["execution_phase"] == "error"
    assert failed["result"]["agent"]["summary"]["current_run"]["semantic_outcome"] == "unresolved"
    assert failed["result"]["agent"]["completed_seq"] == completed_before
    (ROOT / "failed-shape.txt").write_text(tmux("display-message", "-p", "-t", pane, "#{pane_pid}:#{pane_width}:#{pane_height}:#{cursor_x}:#{cursor_y}:#{pane_in_mode}"))
    (ROOT / "failed.json").write_text(json.dumps(failed, indent=2))
    (ROOT / "failed-ansi.txt").write_text(tmux("capture-pane", "-p", "-e", "-t", pane))
    assert "\x1b]8;" in (ROOT / "failed-ansi.txt").read_text()
    assert re.search(r"\x1b\[(?:\d+;)*48;(?:2|5);", (ROOT / "failed-ansi.txt").read_text()), "background color absent"
    print("terminal capacity failure recorded; waiting", waiting, flush=True)
    wait(lambda: "CAPACITY_RECOVERY_DONE" in tmux("capture-pane", "-p", "-t", pane), 100)
    final = wait(lambda: (v := vt("agent", "get", pane)) and v["result"]["agent"]["summary"]["status"] == "done" and v)
    wait(lambda: (c := chain()) and c["stop_reason"] == "completed" and c)
    operations = [value for p in (ROOT / "state").glob("**/operations/*.json")
                  if (value := json.loads(p.read_text()))["dispatch_option"] == "capacity_auto_resume"]
    assert len(operations) == 1 and operations[0]["dispatch_state"] == "prompt_confirmed"
    assert operations[0]["dispatch_option"] == "capacity_auto_resume"
    (ROOT / "confirmed-operation.json").write_text(json.dumps(operations[0], indent=2))
    (ROOT / "done.json").write_text(json.dumps(final, indent=2))
    (ROOT / "done-diagnostics.json").write_text(json.dumps(diagnostic(), indent=2))
    assert chain()["attempts"] == 1
    assert sum(is_recovery_prompt(p) for p in requests) == 1
    print("Error -> PromptConfirmed -> Done passed", flush=True)
    tmux("send-keys", "-l", "-t", pane, "CAPACITY_TEST_DRAFT")
    time.sleep(.5)
    tmux("send-keys", "-t", pane, "Enter")
    wait(lambda: (c := chain()) and c["state"] == "waiting" and c)
    tmux("send-keys", "-l", "-t", pane, "manual draft")
    stopped = wait(lambda: (c := chain()) and c["state"] == "stopped" and c)
    assert stopped["attempts"] == 0
    assert sum(is_recovery_prompt(p) for p in requests) == 1
    print("manual draft cancels pending recovery passed", flush=True)
    (ROOT / "stopped.json").write_text(json.dumps(diagnostic(), indent=2))
    # Clear the unsent draft and prove a conversation switch cancels another wait.
    tmux("send-keys", "-t", pane, "C-u")
    tmux("send-keys", "-l", "-t", pane, "CAPACITY_TEST_NEW")
    time.sleep(.5)
    tmux("send-keys", "-t", pane, "Enter")
    wait(lambda: (c := chain()) and c["state"] == "waiting" and c)
    tmux("send-keys", "-l", "-t", pane, "/new")
    time.sleep(.5)
    tmux("send-keys", "-t", pane, "Enter")
    stopped = wait(lambda: (c := chain()) and c["state"] == "stopped" and c)
    assert stopped["attempts"] == 0
    print("conversation switch cancels pending recovery passed", flush=True)
    print("acceptance artifacts:", ROOT, flush=True)
finally:
    if master is not None:
        try:
            (ROOT / "last-pane.txt").write_text(tmux("capture-pane", "-p", "-t", "%0", check=False))
        except Exception:
            pass
    try:
        (ROOT / "last-diagnostics.json").write_text(run(VT, "daemon", "diagnostics", "--json", check=False))
        (ROOT / "last-agent.json").write_text(run(VT, "agent", "get", "%0", "--json", check=False))
    except Exception:
        pass
    run(VT, "daemon", "disable", check=False)
    if client is not None:
        client.terminate()
        client.wait(timeout=5)
    if master is not None:
        os.close(master)
    tmux("kill-server", check=False)
    server.shutdown()
    print("isolated artifacts retained:", ROOT, flush=True)

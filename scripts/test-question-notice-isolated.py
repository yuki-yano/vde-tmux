#!/usr/bin/env python3
"""Exercise the real vt hook/socket/API/sidebar path on an isolated tmux server.

The fixture supplies the stock Codex 0.155.1 PostToolUse schema and a verifiable
ancestor process. This does not replace acceptance with the actual Codex CLI.
"""
import json
import fcntl
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import struct
import subprocess
import sys
import termios
import threading
import tempfile
import time

repo = Path(__file__).resolve().parent.parent
root = Path(tempfile.mkdtemp(prefix="vde-question-notice-"))
socket = "vde-question-" + str(os.getpid())
build = Path(os.environ.get("VDE_TMUX_TEST_BUILD_BIN", repo / "target/debug/vt"))
for directory in ["bin", "config/vde/tmux", "state", "runtime", "home", "fixture"]:
    (root / directory).mkdir(parents=True, exist_ok=True, mode=0o700)
shutil.copy2(build, root / "bin/vt")
shutil.copy2(repo / "scripts/fixtures/codex-question-hooks.py", root / "bin/codex")
vt = str(root / "bin/vt")
env = {**os.environ, "HOME": str(root / "home"), "ZDOTDIR": str(root / "home"),
       "XDG_CONFIG_HOME": str(root / "config"), "XDG_STATE_HOME": str(root / "state"),
       "XDG_RUNTIME_DIR": str(root / "runtime"), "VDE_TMUX_SOCKET_NAME": socket,
       "VT_BIN": vt, "TERM": "xterm-256color", "PATH": str(root / "bin") + ":" + os.environ["PATH"]}
env.pop("TMUX", None)
env.pop("TMUX_PANE", None)
(root / "config/vde/tmux/config.yml").write_text("daemon:\n  poll_ms: 1000\nsidebar:\n  width: 60\n  task_summary:\n    enabled: false\nnotify:\n  enabled: false\n")


def run(args, check=True, **kwargs):
    result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=20, check=False, **kwargs)
    if check and result.returncode:
        raise AssertionError(f"command failed: {args!r}: {result.stderr}")
    return result


def tmux(*args):
    return run(["tmux", "-L", socket, *args]).stdout.strip()


def query(*args):
    reply = json.loads(run([vt, *args, "--json"]).stdout)
    assert reply["meta"]["api_version"] == 5, reply
    return reply["result"]


def wait(predicate, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (subprocess.CalledProcessError, AssertionError, KeyError, json.JSONDecodeError):
            pass
        time.sleep(0.05)
    raise AssertionError("timed out: " + label)


def emit(event, **fields):
    result = root / "fixture/result.json"
    result.unlink(missing_ok=True)
    temporary = root / "fixture/request.tmp"
    temporary.write_text(json.dumps({"event": event, "fields": fields}))
    temporary.replace(root / "fixture/request.json")
    wait(result.exists, event)
    return json.loads(result.read_text())


def post(tool):
    return emit("PostToolUse", tool_name="request_user_input_async", tool_use_id=tool,
                tool_input={"questions": [{"title": "fixture-private-question", "options": ["fixture-private-answer"]}]},
                tool_response='{"accepted":true}')


def agent():
    return query("agent", "get", pane)["agent"]


def notice():
    return agent()["summary"]["question_notice"]


def ack(seen, ok=True):
    return run([vt, "pane", "question-notice", "ack", pane_ref, "--owner-ref", seen["owner_ref"],
                "--through-order", str(seen["latest_order"]), "--json"], check=ok)


clients = []


def attach_client():
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 80, 200, 0, 0))
    process = subprocess.Popen(["tmux", "-L", socket, "attach-session", "-t", "questions"],
                               env=env, stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)

    def drain():
        while process.poll() is None:
            if select.select([master], [], [], 0.1)[0]:
                try:
                    if not os.read(master, 65536):
                        break
                except OSError:
                    break

    thread = threading.Thread(target=drain, daemon=True)
    thread.start()
    clients.append((process, master, thread))


def benchmark():
    for index in range(57):
        fixture = root / f"load-{index}"
        fixture.mkdir(mode=0o700)
        tmux("new-window", "-d", "-n", f"load-{index}", "-c", str(root),
             shlex.join([sys.executable, str(root / "bin/codex"), str(fixture)]))
    wait(lambda: len(query("agent", "list")["agents"]) == 58, "58 agent panes", timeout=60)
    for _ in range(2):
        attach_client()
    wait(lambda: len(tmux("list-clients", "-F", "#{client_tty}").splitlines()) == 2, "two attached clients")
    sidebars = []
    for window in ["@0", "@1"]:
        run([vt, "sidebar", "open", "--window", window])
        wait(lambda: run([vt, "sidebar", "input", "2", "--window", window], check=False).returncode == 0,
             "sidebar control ready")
        entries = tmux("list-panes", "-t", window, "-F", "#{pane_id} #{@vde_sidebar}").splitlines()
        sidebars.extend(line.split()[0] for line in entries if line.endswith(" 1"))
    assert len(sidebars) == 2, sidebars

    def frames_visible(expected):
        return all(("QUESTIONS" in tmux("capture-pane", "-p", "-t", target)) == expected for target in sidebars)

    samples = []
    for index in range(100):
        result = post(f"latency-{index}")
        assert result["code"] == 0, result
        wait(lambda: notice()["unacknowledged"], "benchmark issue API")
        api_ms = (time.monotonic() - result["started"]) * 1000
        wait(lambda: frames_visible(True), "benchmark issue frames")
        frame_ms = (time.monotonic() - result["started"]) * 1000
        run([vt, "sidebar", "input", "n", "--window", "@1"])
        # Selection is a shared daemon mutation. Let the selected row be drawn before Q.
        wait(lambda: any("▸" in line and "?" in line and "Codex" in line
                         for line in tmux("capture-pane", "-p", "-t", sidebars[1]).splitlines()),
             "selected notice row")
        started = time.monotonic()
        run([vt, "sidebar", "input", "ack-question-notice", "--window", "@1"])
        wait(lambda: not notice()["unacknowledged"], "benchmark Q API")
        ack_api_ms = (time.monotonic() - started) * 1000
        wait(lambda: frames_visible(False), "benchmark Q frames")
        ack_frame_ms = (time.monotonic() - started) * 1000
        samples.append({"issue_api_ms": api_ms, "issue_frame_ms": frame_ms,
                        "ack_api_ms": ack_api_ms, "ack_frame_ms": ack_frame_ms})
        assert max(samples[-1].values()) <= 2000, samples[-1]
    report = {"agent_panes": 58, "clients": 2, "poll_ms": 1000, "iterations": 100,
              "clock": "monotonic; issue before hook process launch, Q before control request (conservative upper bounds)",
              "max_ms": {key: max(sample[key] for sample in samples) for key in samples[0]}, "samples": samples}
    (root / "latency.json").write_text(json.dumps(report, indent=2) + "\n")
    print("58 panes / two clients / 100 issue + Q cycles: " + json.dumps(report["max_ms"]))


started_daemon = False
try:
    tmux("-f", "/dev/null", "new-session", "-d", "-s", "questions", "-x", "200", "-y", "60", "/bin/bash --noprofile --norc")
    tmux("set-option", "-g", "default-shell", "/bin/bash")
    tmux("set-option", "-g", "default-command", "/bin/bash --noprofile --norc")
    env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
    env["TMUX_PANE"] = tmux("display-message", "-p", "-t", "questions:0", "#{pane_id}")
    started_daemon = True
    run([vt, "daemon", "start"])
    pane = tmux("new-window", "-d", "-P", "-F", "#{pane_id}", "-n", "agent", "-c", str(root),
                shlex.join([sys.executable, str(root / "bin/codex"), str(root / "fixture")]))
    wait((root / "fixture/ready").exists, "fixture startup")
    wait(lambda: agent()["summary"]["identity"] == "exact", "exact Codex identity")
    pane_ref = agent()["summary"]["pane_ref"]
    assert emit("UserPromptSubmit", prompt="question notice acceptance fixture")["code"] == 0
    result = post("call-1")
    assert result["code"] == 0, result
    first = wait(lambda: notice() if notice()["unacknowledged"] else None, "issued notice")
    assert agent()["summary"]["status"] == "working"
    assert agent()["summary"]["needs_action"] is True
    assert "?" in run([vt, "sidebar", "attach", "--once"]).stdout
    tmux("set-option", "-p", "-t", env["TMUX_PANE"], "-u", "@vde_sidebar")
    post("call-2")
    ack(first)
    assert notice()["unacknowledged"] and notice()["acknowledged_order"] == 1
    current = notice()
    future = {**current, "latest_order": current["latest_order"] + 1}
    assert ack(future, ok=False).returncode != 0
    before = agent()
    ack(current)
    after = agent()
    assert not notice()["unacknowledged"]
    for field in ["state_revision", "run_seq", "completed_seq"]:
        assert before[field] == after[field], field
    post("call-1")
    assert not notice()["unacknowledged"], "acknowledged replay revived notification"
    post("call-3")
    assert emit("Stop", last_assistant_message="fixture completed")["code"] == 0
    assert notice()["unacknowledged"] and agent()["summary"]["status"] == "done"
    saved = notice()
    run([vt, "daemon", "stop"])
    run([vt, "daemon", "start"])
    wait(lambda: notice().get("owner_ref") == saved["owner_ref"], "rehydration and owner rebind")
    assert notice()["unacknowledged"]
    ack(saved)
    post("call-3")
    assert not notice()["unacknowledged"], "restart lost acknowledged dedup key"
    sidecars = list((root / "state").rglob("question-notices-v1.json"))
    assert len(sidecars) == 1
    assert "fixture-private" not in sidecars[0].read_text()
    assert sidecars[0].stat().st_mode & 0o777 == 0o600
    # A hook invoked outside the exact pane ancestry cannot create a notice there.
    outside = {"hook_event_name": "PostToolUse", "session_id": "question-fixture-root", "turn_id": "question-fixture-turn",
               "transcript_path": str(root / "fixture/session.jsonl"), "tool_name": "request_user_input_async",
               "tool_use_id": "outside", "tool_response": '{"accepted":true}'}
    external_env = {**env, "TMUX_PANE": pane}
    external = subprocess.run([vt, "hook", "codex", "PostToolUse"], env=external_env, input=json.dumps(outside), text=True, capture_output=True, timeout=10)
    assert external.returncode != 0 and "AncestorNotInPane" in external.stderr, external.stderr
    assert not notice()["unacknowledged"]
    if "--extended" in sys.argv:
        benchmark()
    post("last")
    tmux("kill-pane", "-t", pane)
    wait(lambda: all(owner["pane"]["pane_id"] != pane for owner in json.loads(sidecars[0].read_text())["owners"]),
         "confirmed owner cleanup")
    print("question notices: hook ancestry, API, sidebar rendering, fenced ack, dedup, restart, Stop, privacy, and owner cleanup passed")
finally:
    if started_daemon:
        run([vt, "daemon", "disable"], check=False)
    run(["tmux", "-L", socket, "kill-server"], check=False)
    for process, master, thread in clients:
        process.wait(timeout=5)
        thread.join(timeout=1)
        os.close(master)
    if os.environ.get("KEEP_ARTIFACTS") == "1":
        print("artifacts: " + str(root), file=sys.stderr)
    else:
        shutil.rmtree(root)

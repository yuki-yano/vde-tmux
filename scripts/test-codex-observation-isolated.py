#!/usr/bin/env python3
"""Exercise Codex hook ownership using native synthetic processes on a scratch tmux server.

No model request or production tmux operation is made. Fixture bodies are synthetic.
Requires cc and tmux; build target/debug/vt and vde-tmux before running.
"""
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("VDE_VT_BIN", ROOT / "target/debug/vt")).resolve()
NATIVE = r'''
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--version")) {
        puts("codex-cli 0.156.1");
        return 0;
    }
    const char *root = getenv("VDE_FIXTURE_ROOT");
    const char *role = getenv("VDE_FIXTURE_ROLE");
    if (argc > 1 && !strcmp(argv[1], "mcp-server")) {
        for (;;) pause();
    }
    if (argc > 1 && !strcmp(argv[1], "app-server")) {
        role = strstr(argv[0], "codex-managed") ? "renamed" : "shared";
    } else if (!strcmp(role, "first")) {
        if (fork() == 0) {
            execl(argv[0], argv[0], "app-server", NULL);
            _exit(127);
        }
        if (fork() == 0) {
            char renamed[4096];
            snprintf(renamed, sizeof(renamed), "%s/codex-managed", root);
            execl(renamed, renamed, "app-server", NULL);
            _exit(127);
        }
    } else if (!strcmp(role, "embedded") && fork() == 0) {
        execl(argv[0], argv[0], "mcp-server", NULL);
        _exit(127);
    }
    char path[4096], line[512], event[64], pane[64];
    snprintf(path, sizeof(path), "%s/%s.fifo", root, role);
    FILE *commands = fopen(path, "r+");
    if (!commands) return 2;
    while (fgets(line, sizeof(line), commands)) {
        if (sscanf(line, "%63s %63s", event, pane) != 2) return 3;
        if (!strncmp(event, "SCREEN_", 7)) {
            snprintf(path, sizeof(path), "%s/%s.txt", root, event);
            FILE *screen = fopen(path, "r");
            if (!screen) return 7;
            fputs("\033[2J\033[H", stdout);
            int ch;
            while ((ch = fgetc(screen)) != EOF) putchar(ch);
            fclose(screen);
            fflush(stdout);
            continue;
        }
        pid_t child = fork();
        if (child == 0) {
            setenv("TMUX_PANE", pane, 1);
            snprintf(path, sizeof(path), "%s/%s-%s.json", root, role, event);
            if (!freopen(path, "r", stdin)) _exit(4);
            snprintf(path, sizeof(path), "%s/%s-errors.log", root, role);
            if (!freopen(path, "a", stderr)) _exit(4);
            execl(getenv("VDE_FIXTURE_VT"), "vt", "hook", "codex", event, NULL);
            _exit(127);
        }
        int status;
        if (child < 0 || waitpid(child, &status, 0) < 0) return 5;
        snprintf(path, sizeof(path), "%s/%s.receipts", root, role);
        FILE *receipt = fopen(path, "a");
        if (!receipt) return 6;
        fprintf(receipt, "%s %s %d\n", event, pane,
            WIFEXITED(status) ? WEXITSTATUS(status) : 128);
        fclose(receipt);
    }
    return 0;
}
'''


def main():
    work = Path(tempfile.mkdtemp(prefix="vde-codex-observation-"))
    work.chmod(0o700)
    print(f"Codex ownership artifacts: {work}", flush=True)
    socket = "vde-codex-observation-" + work.name.rsplit("-", 1)[-1]
    env = {
        **os.environ,
        "HOME": str(work / "home"),
        "XDG_STATE_HOME": str(work / "state"),
        "XDG_CONFIG_HOME": str(work / "config"),
        "CODEX_HOME": str(work / "codex-home"),
        "VDE_TMUX_SOCKET_NAME": socket,
        "VDE_FIXTURE_ROOT": str(work),
        "VDE_FIXTURE_VT": str(BIN),
    }
    for name in ["home", "state", "config", "codex-home/sessions"]:
        (work / name).mkdir(parents=True)
    for role in ["first", "second", "embedded", "shared", "renamed"]:
        os.mkfifo(work / f"{role}.fifo", 0o600)
        transcript = work / "codex-home/sessions" / f"rollout-{role}.jsonl"
        transcript.write_text(json.dumps({"type": "session_meta", "payload": {
            "id": role, "thread_source": "user",
        }}) + "\n")
        for event in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest", "Stop"]:
            (work / f"{role}-{event}.json").write_text(json.dumps({
                "session_id": role, "transcript_path": str(transcript),
                "source": "startup", "prompt": "synthetic input",
                "turn_id": "synthetic-turn",
                "tool_name": "exec_command", "last_assistant_message": "synthetic response",
            }))
    screens = {
        "SCREEN_WORK": ("• Inspecting source (1m 12s • ctrl+x to interrupt)\n• Queued follow-up inputs\n  synthetic input\n› Ask Codex\n", "working", False),
        "SCREEN_APPROVAL": ("Would you like to run the following command?\n  › 1. Yes, proceed (y)\n  2. No (esc)\n", "blocked", True),
        "SCREEN_SYNC": ("Question 1/1 (1 unanswered)\n", "blocked", True),
        "SCREEN_ASYNC": ("• Reviewing (3s)\n  ? 1 question · 3s\n› Ask Codex\n", "working", False),
        "SCREEN_UNKNOWN": ("Unrecognized UI\n› Ask Codex\n", "unknown", False),
    }
    for name, (text, _, _) in screens.items():
        (work / f"{name}.txt").write_text(text)
    (work / "codex.c").write_text(NATIVE)

    def run(args, *, checked=True, timeout=30):
        result = subprocess.run([str(arg) for arg in args], env=env, text=True,
                                capture_output=True, timeout=timeout)
        if checked and result.returncode:
            raise RuntimeError(f"{args[:3]}: {result.stderr}")
        return result.stdout.strip()

    def tmux(*args, **kwargs):
        return run(["tmux", "-u", "-L", socket, *args], **kwargs)

    def vt(*args):
        return run([BIN, *args])

    def snapshot():
        return json.loads(vt("api", "snapshot", "--json"))["result"]

    def await_agents(predicate):
        deadline = time.monotonic() + 20
        while True:
            agents = {a["pane_id"]: a for a in snapshot()["agents"]}
            if predicate(agents):
                return agents
            if time.monotonic() >= deadline:
                raise AssertionError("scratch state condition was not observed: " + json.dumps({p: {k: a.get(k) for k in ["status", "badge", "needs_action"]} for p, a in agents.items()}))
            time.sleep(0.1)

    timings = []

    def hook(role, event, pane):
        receipts = work / f"{role}.receipts"
        previous = len(receipts.read_text().splitlines()) if receipts.exists() else 0
        started = time.monotonic()
        with (work / f"{role}.fifo").open("w") as stream:
            stream.write(f"{event} {pane}\n")
        deadline = started + 15
        while True:
            rows = receipts.read_text().splitlines() if receipts.exists() else []
            if len(rows) > previous:
                assert rows[-1] == f"{event} {pane} 0", rows[-1]
                timings.append({"role": role, "event": event,
                                "elapsed_ms": round((time.monotonic() - started) * 1000)})
                return
            if time.monotonic() >= deadline:
                raise AssertionError(f"native hook receipt missing: {role}/{event}")
            time.sleep(0.02)

    try:
        run(["cc", "-Wall", "-Wextra", "-Werror", "-o", work / "codex", work / "codex.c"])
        (work / "codex-managed").write_bytes((work / "codex").read_bytes())
        (work / "codex-managed").chmod(0o700)
        panes = {}
        for role in ["first", "second", "embedded"]:
            command = shlex.join(["env", f"VDE_FIXTURE_ROLE={role}", str(work / "codex")]
                                 + (["--", "app-server"] if role == "embedded" else []))
            args = (["new-session", "-d", "-s", "fixture", "-x", "140", "-y", "32"]
                    if not panes else ["new-window", "-d", "-t", "fixture"])
            panes[role] = tmux(*args, "-P", "-F", "#{pane_id}", command)
        env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
        env["TMUX_PANE"] = panes["embedded"]
        vt("daemon", "ensure")
        await_agents(lambda a: all(p in a for p in panes.values()))
        initial = {p: json.loads(vt("agent", "get", p, "--json"))["result"]["agent"]
                   for p in panes.values()}

        events = ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest", "Stop"]
        for role in ["shared", "renamed"]:
            for pane in [panes["first"], panes["second"]]:
                for event in events:
                    hook(role, event, pane)
                after = json.loads(vt("agent", "get", pane, "--json"))["result"]["agent"]
                for field in ["agent_epoch", "agent_session_id", "run_seq", "completed_seq", "prompt"]:
                    assert after.get(field) == initial[pane].get(field), (pane, field)
        log = "\n".join(p.read_text() for p in (work / "state/vde-tmux").glob("*/daemon.log"))
        assert log.count("hook_ownership: dropped: shared_server") == 24

        screen_cases = 0
        for role in ["first", "second"]:
            pane = panes[role]
            for name, (_, badge, needs_action) in screens.items():
                print(f"Checking screen {role}/{name} -> {badge}", flush=True)
                with (work / f"{role}.fifo").open("w") as stream:
                    stream.write(f"{name} {pane}\n")
                agents = await_agents(lambda a: a[pane]["badge"] == badge)
                assert agents[pane]["status"] == "idle"
                assert agents[pane]["needs_action"] == needs_action
                detail = json.loads(vt("agent", "get", pane, "--json"))["result"]["agent"]
                assert detail["run_seq"] == detail["completed_seq"] == 0
                screen_cases += 1

        target = panes["embedded"]
        hook("embedded", "SessionStart", target)
        hook("embedded", "UserPromptSubmit", target)
        await_agents(lambda a: a[target]["status"] == "working")
        hook("embedded", "PermissionRequest", target)
        await_agents(lambda a: a[target]["status"] == "blocked")
        hook("embedded", "PreToolUse", target)
        await_agents(lambda a: a[target]["status"] == "working")
        hook("embedded", "PostToolUse", target)
        hook("embedded", "Stop", target)
        await_agents(lambda a: a[target]["status"] == "done")
        after = json.loads(vt("agent", "get", target, "--json"))["result"]["agent"]
        assert after["agent_session_id"] == "embedded"
        assert after["run_seq"] == after["completed_seq"] == 1
        vt("daemon", "restart")
        await_agents(lambda a: a[target]["identity"] == "exact")
        for event in ["UserPromptSubmit", "Stop"]:
            path = work / f"embedded-{event}.json"
            payload = json.loads(path.read_text())
            payload["turn_id"] = "after-daemon-restart"
            path.write_text(json.dumps(payload))
        hook("embedded", "UserPromptSubmit", target)
        await_agents(lambda a: a[target]["status"] == "working")
        hook("embedded", "Stop", target)
        await_agents(lambda a: a[target]["status"] == "done")
        after = json.loads(vt("agent", "get", target, "--json"))["result"]["agent"]
        assert after["run_seq"] == after["completed_seq"] == 2
        # A working-looking screen after accepted Stop must not replace the
        # authoritative lifecycle, even without a repeated SessionStart.
        with (work / "embedded.fifo").open("w") as stream:
            stream.write(f"SCREEN_WORK {target}\n")
        time.sleep(2.2)
        agents = {a["pane_id"]: a for a in snapshot()["agents"]}
        assert agents[target]["badge"] in ["idle", "done"], agents[target]["badge"]
        report = {"screen_cases": screen_cases, "screen_created_runs": 0, "shared_hook_cases": 24, "wrong_pane_bindings": 0,
                  "embedded_hook_cases": 8, "embedded_completed_seq": 2,
                  "embedded_hooks_restore_authority_after_restart": True,
                  "embedded_with_mcp_sibling": True, "renamed_app_server_rejected": True,
                  "hook_timings": timings, "fixture_kind": "native synthetic process tree"}
        (work / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"PASS: Codex ownership, artifacts: {work}")
    finally:
        if env.get("TMUX"):
            run([BIN, "daemon", "disable"], checked=False)
        tmux("kill-server", checked=False)


if __name__ == "__main__":
    main()

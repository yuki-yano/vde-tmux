#!/usr/bin/env python3
"""Exercise Codex hook ownership using native synthetic processes on a scratch tmux server.

No model request or production tmux operation is made. Fixture bodies are synthetic.
Requires cc and tmux; build target/debug/vt and vde-tmux before running.
"""
import json
import os
from pathlib import Path
import shlex
import shutil
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
    FILE *rollout = NULL;
    FILE *subagent_rollouts[2] = {NULL, NULL};
    FILE *duplicate_rollout = NULL;
    if (!strcmp(role, "embedded")) {
        snprintf(path, sizeof(path), "%s/sessions/rollout-embedded.jsonl", getenv("CODEX_HOME"));
        rollout = fopen(path, "a+");
        if (!rollout) return 8;
    }
    snprintf(path, sizeof(path), "%s/%s.fifo", root, role);
    FILE *commands = fopen(path, "r+");
    if (!commands) return 2;
    while (fgets(line, sizeof(line), commands)) {
        if (sscanf(line, "%63s %63s", event, pane) != 2) return 3;
        if (!strcmp(event, "SUBAGENTS_OPEN")) {
            const char *names[] = {"active", "completed"};
            for (int i = 0; i < 2; i++) {
                snprintf(path, sizeof(path), "%s/sessions/rollout-subagent-%s.jsonl",
                    getenv("CODEX_HOME"), names[i]);
                subagent_rollouts[i] = fopen(path, "a+");
                if (!subagent_rollouts[i]) return 8;
            }
            continue;
        }
        if (!strcmp(event, "DUPLICATE_OPEN")) {
            snprintf(path, sizeof(path), "%s/sessions/duplicate/rollout-embedded.jsonl",
                getenv("CODEX_HOME"));
            duplicate_rollout = fopen(path, "a+");
            if (!duplicate_rollout) return 8;
            continue;
        }
        if (!strcmp(event, "OTHER_ROOT_OPEN")) {
            snprintf(path, sizeof(path), "%s/sessions/rollout-other-root.jsonl",
                getenv("CODEX_HOME"));
            duplicate_rollout = fopen(path, "a+");
            if (!duplicate_rollout) return 8;
            continue;
        }
        if (!strcmp(event, "DUPLICATE_CLOSE")) {
            if (duplicate_rollout) fclose(duplicate_rollout);
            duplicate_rollout = NULL;
            continue;
        }
        if (!strncmp(event, "SESSION_", 8) && rollout) {
            fclose(rollout);
            snprintf(path, sizeof(path), "%s/sessions/rollout-%s.jsonl", getenv("CODEX_HOME"),
                !strcmp(event, "SESSION_SWAP") ? "replacement" : "embedded");
            rollout = fopen(path, "a+");
            if (!rollout) return 8;
            continue;
        }
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
        "ZDOTDIR": str(work / "home"),
        "XDG_STATE_HOME": str(work / "state"),
        "XDG_CONFIG_HOME": str(work / "config"),
        "CODEX_HOME": str(work / "codex-home"),
        "VDE_TMUX_SOCKET_NAME": socket,
        "VDE_FIXTURE_ROOT": str(work),
        "VDE_FIXTURE_VT": str(BIN),
        "PATH": str(work) + ":" + str(BIN.parent) + ":" + os.environ["PATH"],
    }
    assert Path(shutil.which("vt", path=env["PATH"])).resolve() == BIN
    real_tmux = shutil.which("tmux")
    assert real_tmux is not None, "tmux is required"
    capture_failure = work / "fail-observation-capture"
    (work / "tmux").write_text(
        "#!/bin/sh\nif [ -f " + shlex.quote(str(capture_failure)) + " ]; then\n"
        "  case \"$*\" in *__vde_capture_identity_*) "
        "printf 'synthetic capture client failure\\n' >&2; exit 1;; esac\nfi\n"
        "exec " + shlex.quote(real_tmux) + " \"$@\"\n")
    (work / "tmux").chmod(0o700)
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
        "SCREEN_TRUST": ("> You are in /synthetic/project\nDo you trust the contents of this directory?\n› 1. Yes, continue\n  2. No, exit\n", "blocked", True),
        "SCREEN_UPDATE": ("Update available!\n› 1. Update now\n  2. Skip until next version\nPress enter to continue\n", "blocked", True),
        "SCREEN_READY": ("Codex\n› \033[2mAsk Codex to do anything\033[0m\n\n? for shortcuts\033[2;3H", "idle", False),
        "SCREEN_DRAFT": ("Codex\n› typed draft\n\n? for shortcuts\033[2;3H", "unknown", False),
        "SCREEN_QUEUE": ("Codex\n› \033[2mAsk Codex to do anything\033[0m\n\n• Queued follow-up inputs\033[2;3H", "unknown", False),
        "SCREEN_UNKNOWN": ("Unrecognized UI\n› Ask Codex\n", "unknown", False),
    }
    reasons = {"SCREEN_WORK": "screen_working", "SCREEN_APPROVAL": "screen_approval", "SCREEN_SYNC": "screen_question", "SCREEN_ASYNC": "screen_working", "SCREEN_TRUST": "screen_trust", "SCREEN_UPDATE": "screen_update", "SCREEN_UNKNOWN": "unknown_screen"}
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
        return run(["tmux", "-u", "-f", "/dev/null", "-L", socket, *args], **kwargs)

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
                (work / "failure-topology.txt").write_text(tmux("list-panes", "-a", "-F", "#{pane_id}:#{pane_pid}:#{pane_dead}:#{pane_current_command}"))
                (work / "failure-state.json").write_text(json.dumps(snapshot()))
                selected = [line for line in tmux("show-environment", "-g").splitlines()
                            if line.startswith(("PATH=", "HOME=", "ZDOTDIR=", "VDE_FIXTURE_", "VDE_TMUX_"))]
                (work / "failure-environment.txt").write_text("\n".join(selected))
                process_scan = run(["ps", "-ax", "-o", "pid=,ppid=,pgid=,tpgid=,comm="], checked=False)
                (work / "failure-process-scan.txt").write_text(process_scan)
                (work / "failure-daemon-diagnostics.json").write_text(vt("daemon", "diagnostics", "--json"))
                (work / "failure-daemon-status.txt").write_text(vt("daemon", "status"))
                if shutil.which("sample"):
                    for state_path in (work / "state/vde-tmux").glob("*/lifecycle.json"):
                        process = json.loads(state_path.read_text()).get("process")
                        if process:
                            run(["sample", str(process["pid"]), "1", "1", "-file",
                                 work / "failure-daemon-stack.txt"], checked=False, timeout=8)
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
                                 + (["--no-daemon"] if role == "embedded" else []))
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
                if name not in reasons: continue;
                print(f"Checking screen {role}/{name} -> {badge}", flush=True)
                with (work / f"{role}.fifo").open("w") as stream:
                    stream.write(f"{name} {pane}\n")
                agents = await_agents(lambda a: a[pane]["badge"] == badge and a[pane]["presentation"]["reason"] == reasons[name])
                assert agents[pane]["presentation"]["ttl_seconds"] == 3
                assert agents[pane]["status"] == "idle"
                assert agents[pane]["needs_action"] == needs_action
                detail = json.loads(vt("agent", "get", pane, "--json"))["result"]["agent"]
                assert detail["run_seq"] == detail["completed_seq"] == 0
                screen_cases += 1

        # A failed capture client cannot provide screen evidence, but must not
        # stop the daemon or allocate a Run. A later successful poll recovers.
        daemon_instance = json.loads(vt("api", "snapshot", "--json"))["meta"]["daemon_instance_id"]
        capture_failure.touch()
        await_agents(lambda a: all(a[panes[role]]["badge"] == "unknown"
                                   and a[panes[role]]["presentation"]["reason"] == "evidence_unavailable"
                                   for role in ["first", "second"]))
        assert json.loads(vt("api", "snapshot", "--json"))["meta"]["daemon_instance_id"] == daemon_instance
        capture_failure.unlink()
        await_agents(lambda a: all(a[panes[role]]["presentation"]["reason"] == "unknown_screen"
                                   for role in ["first", "second"]))
        for role in ["first", "second"]:
            detail = json.loads(vt("agent", "get", panes[role], "--json"))["result"]["agent"]
            assert detail["run_seq"] == detail["completed_seq"] == 0
        assert json.loads(vt("api", "snapshot", "--json"))["meta"]["daemon_instance_id"] == daemon_instance
        print("PASS: failed capture client retained daemon identity and recovered screen evidence", flush=True)

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
        transcript = work / "codex-home/sessions/rollout-embedded.jsonl"
        def structural(kind, turn):
            with transcript.open("a") as stream:
                stream.write(json.dumps({"type":"event_msg", "payload":{"type":kind,"turn_id":turn}})+"\n")
        def screen(name):
            with (work / "embedded.fifo").open("w") as stream:
                stream.write(f"{name} {target}\n")
            time.sleep(0.1)
        structural("task_started", "synthetic-turn")
        structural("task_complete", "synthetic-turn")
        screen("SCREEN_READY")
        subagent_paths = []
        for state in ["active", "completed"]:
            child = transcript.with_name(f"rollout-subagent-{state}.jsonl")
            child.write_text(json.dumps({"type":"session_meta", "payload":{
                "id":f"subagent-{state}", "thread_source":"subagent",
                "session_id":"embedded", "forked_from_id":"embedded",
                "source":{"subagent":{"thread_spawn":{"parent_thread_id":"embedded"}}},
                "base_instructions":{"text":"x" * (23 * 1024)}}})+"\n"
                +json.dumps({"type":"event_msg", "payload":{
                    "type":"task_started", "turn_id":f"child-{state}"}})+"\n")
            if state == "completed":
                with child.open("a") as stream:
                    stream.write(json.dumps({"type":"event_msg", "payload":{
                        "type":"task_complete", "turn_id":f"child-{state}"}})+"\n")
            subagent_paths.append(child)
        screen("SUBAGENTS_OPEN")
        recovered = []
        for cycle in range(int(os.environ.get("VDE_RESYNC_CYCLES", "20"))):
            started = time.monotonic()
            vt("daemon", "restart")
            await_agents(lambda a: a[target]["presentation"]["reason"] == "provider_resynchronized")
            elapsed = time.monotonic() - started
            assert elapsed <= 5, elapsed
            recovered.append(round(elapsed, 3))
        # Known completion is retained; no UserPromptSubmit hook was needed to recover.
        after = json.loads(vt("agent", "get", target, "--json"))["result"]["agent"]
        assert after["run_seq"] == after["completed_seq"] == 1
        def reject_prompt(label):
            before = tmux("capture-pane", "-p", "-t", target)
            reference = json.loads(vt("agent", "get", target, "--json"))["result"]["agent"]["summary"]["agent_ref"]
            result = subprocess.run([str(BIN), "agent", "prompt", reference, "--operation-id", "resync-reject-"+label,
                                     "--stdin"], input="must-not-send", env=env, text=True, capture_output=True, timeout=15)
            assert result.returncode != 0, result.stdout
            failure = json.loads(result.stdout or result.stderr)
            assert failure["error"]["code"] == "agent_not_ready", failure
            assert tmux("capture-pane", "-p", "-t", target) == before
        screen("SCREEN_DRAFT")
        reject_prompt("draft")
        screen("SCREEN_QUEUE")
        reject_prompt("queue")
        screen("SCREEN_READY")
        await_agents(lambda a: a[target]["presentation"]["reason"] == "provider_resynchronized")
        structural("task_started", "lost-hook-turn")
        reject_prompt("new-turn")
        structural("task_complete", "lost-hook-turn")
        await_agents(lambda a: a[target]["presentation"]["reason"] == "provider_resynchronized")
        duplicate = transcript.parent / "duplicate/rollout-embedded.jsonl"
        duplicate.parent.mkdir()
        duplicate.write_text(json.dumps({"type":"session_meta", "payload":{
            "id":"embedded", "thread_source":"user"}})+"\n")
        screen("DUPLICATE_OPEN")
        await_agents(lambda a: a[target]["presentation"]["reason"] != "provider_resynchronized")
        reject_prompt("duplicate-session-writer")
        screen("DUPLICATE_CLOSE")
        await_agents(lambda a: a[target]["presentation"]["reason"] == "provider_resynchronized")
        other_root = transcript.with_name("rollout-other-root.jsonl")
        other_root.write_text(json.dumps({"type":"session_meta", "payload":{
            "id":"other-root", "thread_source":"user"}})+"\n")
        screen("OTHER_ROOT_OPEN")
        await_agents(lambda a: a[target]["presentation"]["reason"] != "provider_resynchronized")
        reject_prompt("old-root-retained-with-stale-canonical-session")
        screen("DUPLICATE_CLOSE")
        await_agents(lambda a: a[target]["presentation"]["reason"] == "provider_resynchronized")
        replacement = transcript.with_name("rollout-replacement.jsonl")
        replacement.write_text(json.dumps({"type":"session_meta","payload":{"id":"replacement","thread_source":"user"}})+"\n")
        screen("SESSION_SWAP")
        await_agents(lambda a: a[target]["presentation"]["reason"] != "provider_resynchronized")
        reject_prompt("same-pid-new-session")
        screen("SESSION_RESTORE")
        screen("SCREEN_READY")
        await_agents(lambda a: a[target]["presentation"]["reason"] == "provider_resynchronized")
        # Finished subagents keep their writers open in the same process.
        with subagent_paths[0].open("a") as stream:
            stream.write(json.dumps({"type":"event_msg", "payload":{
                "type":"task_complete", "turn_id":"child-active"}})+"\n")
        # A history larger than one request must recover incrementally, and the
        # final dispatch must reuse that checkpoint instead of starting over.
        with transcript.open("a") as stream:
            stream.write('{"type":"response_item","payload":{"body":"')
            stream.write("x" * (34 * 1024 * 1024))
            stream.write('"}}\n')
        large_history_written = int(time.time())
        await_agents(lambda a: a[target]["presentation"]["reason"] == "provider_resynchronized"
                     and (a[target]["presentation"].get("observed_at") or 0) > large_history_written + 3)
        prompt = "synthetic external resync prompt"
        reference = json.loads(vt("agent", "get", target, "--json"))["result"]["agent"]["summary"]["agent_ref"]
        proc = subprocess.Popen([str(BIN), "agent", "prompt", reference, "--operation-id", "resync-external-prompt-0001", "--stdin"],
                                env=env, text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        proc.stdin.write(prompt+"\n")
        proc.stdin.close()
        deadline = time.monotonic()+5
        while prompt not in tmux("capture-pane", "-p", "-t", target):
            assert time.monotonic() < deadline, "external prompt was not delivered"
            time.sleep(0.05)
        for event in ["UserPromptSubmit", "Stop"]:
            path = work / f"embedded-{event}.json"
            payload = json.loads(path.read_text())
            payload.update(turn_id="external-resync-turn", prompt=prompt)
            path.write_text(json.dumps(payload))
        structural("task_started", "external-resync-turn")
        hook("embedded", "UserPromptSubmit", target)
        proc.wait(timeout=10)
        result = json.loads(proc.stdout.read())
        assert proc.returncode == 0, (result, proc.stderr.read())
        hook("embedded", "Stop", target)
        structural("task_complete", "external-resync-turn")
        await_agents(lambda a: a[target]["status"] == "done")
        (work / "resync-evidence.json").write_text(json.dumps({"restart_cycles":len(recovered), "restart_seconds":recovered,
            "external_prompt":"confirmed", "large_history_mib":34,
            "retained_subagent_writers":2,
            "negative_cases":["draft","queue","lost-hook-turn","duplicate-session-writer",
                              "old-root-retained-with-stale-canonical-session","same-pid-new-session"]}, indent=2)+"\n")
        print(f"PASS: {len(recovered)} restarts recovered with subagent writers retained; external prompt confirmed; draft/queue/new turn/duplicate writer/session replacement rejected", flush=True)
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
        assert after["run_seq"] == after["completed_seq"] == 3
        # A working-looking screen after accepted Stop must not replace the
        # authoritative lifecycle, even without a repeated SessionStart.
        with (work / "embedded.fifo").open("w") as stream:
            stream.write(f"SCREEN_WORK {target}\n")
        time.sleep(2.2)
        agents = {a["pane_id"]: a for a in snapshot()["agents"]}
        assert agents[target]["badge"] in ["idle", "done"], agents[target]["badge"]
        assert agents[target]["presentation"]["reason"] == "hook_authoritative"
        report = {"screen_cases": screen_cases, "screen_created_runs": 0, "shared_hook_cases": 24, "wrong_pane_bindings": 0,
                  "embedded_hook_cases": 10, "embedded_completed_seq": 3,
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

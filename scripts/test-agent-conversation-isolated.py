#!/usr/bin/env python3
"""Read-only conversation API acceptance against a private tmux server."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def fixture_agent():
    provider = os.environ["CONVERSATION_PROVIDER"]
    transcript = Path(os.environ["CONVERSATION_TRANSCRIPT"])
    binary = os.environ["CONVERSATION_BIN"]
    session = os.environ["CONVERSATION_SESSION"]
    # Codex's exact live writer locator must see this writable descriptor.
    with transcript.open("a", encoding="utf-8") as output:
        def hook(event, fields):
            return subprocess.run([binary, "hook", provider, event], input=json.dumps({"session_id": session, "transcript_path": str(transcript), **fields}), text=True, capture_output=True)
        first = hook("SessionStart", {"source": "startup"})
        if first.returncode:
            raise RuntimeError(first.stderr)
        print("READY", flush=True)
        with os.fdopen(os.open(os.environ["CONVERSATION_CONTROL"], os.O_RDWR), "r") as requests:
            for line in requests:
                request = json.loads(line)
                for record in request.get("records", []):
                    output.write(json.dumps(record, ensure_ascii=False) + "\n")
                if "partial" in request:
                    output.write(request["partial"])
                output.flush()
                response = hook(request["event"], request.get("fields", {})) if "event" in request else None
                receipt = {"id": request["id"], "code": response.returncode if response else 0, "stderr": response.stderr if response else ""}
                with open(os.environ["CONVERSATION_RECEIPTS"], "a") as receipts:
                    receipts.write(json.dumps(receipt) + "\n")


def main():
    root = Path(tempfile.mkdtemp(prefix="vde-conversation-"))
    sock = "vde-conversation-" + str(os.getpid())
    binary = Path(os.environ.get("VDE_TMUX_TEST_BUILD_BIN", "target/debug/vt")).resolve()
    if "VDE_TMUX_TEST_BUILD_BIN" not in os.environ:
        subprocess.run(["cargo", "build", "--bin", "vt"], check=True)
    env = os.environ.copy()
    env.pop("TMUX_PANE", None)
    env.update({"VDE_TMUX_SOCKET_NAME": sock, "XDG_CONFIG_HOME": str(root / "config"), "XDG_STATE_HOME": str(root / "state"), "XDG_RUNTIME_DIR": str(root / "runtime"), "CODEX_HOME": str(root / "codex-home"), "ZDOTDIR": str(root / "zsh")})
    for key in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "CODEX_HOME", "ZDOTDIR"]:
        Path(env[key]).mkdir(parents=True)
    (root / "bin").mkdir()
    subprocess.run(["scripts/fixtures/build-codex-embedded", str(root / "bin" / "codex")], check=True)
    shutil.copy2(root / "bin" / "codex", root / "bin" / "claude")
    def tmux(*args):
        return subprocess.check_output(["tmux", "-L", sock, *args], env=env, text=True).strip()
    def vt(*args, expected=0):
        reply = subprocess.run([str(binary), *args, "--json"], env=env, text=True, capture_output=True, timeout=15)
        if reply.returncode != expected:
            raise AssertionError((args, reply.returncode, reply.stdout, reply.stderr))
        return json.loads(reply.stdout if expected == 0 else reply.stderr)
    def until(action, predicate, seconds=12):
        deadline = time.monotonic() + seconds
        last = None
        while time.monotonic() < deadline:
            try:
                last = action()
                if predicate(last):
                    return last
            except (AssertionError, KeyError, FileNotFoundError, json.JSONDecodeError) as error:
                last = repr(error)
            time.sleep(0.05)
        raise AssertionError(("deadline", last))
    daemon_started = False
    try:
        tmux("-f", "/dev/null", "new-session", "-d", "-s", "conversation", "/bin/bash --noprofile --norc")
        env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
        subprocess.run([str(binary), "daemon", "start"], env=env, check=True, capture_output=True)
        daemon_started = True
        schema = vt("api", "schema")["result"]["contract"]
        assert schema["providers"]["claude"]["capabilities"]["conversation"] == "transcript_read_only"
        for provider in ["claude", "codex"]:
            pane = tmux("new-window", "-d", "-t", "conversation", "-P", "-F", "#{pane_id}", "/bin/bash --noprofile --norc")
            session = provider + "-conversation-session"
            transcript = root / "codex-home" / "sessions" / ("rollout-" + session + ".jsonl") if provider == "codex" else root / (session + ".jsonl")
            transcript.parent.mkdir(parents=True, exist_ok=True)
            transcript.write_text(json.dumps({"type": "session_meta", "payload": {"id": session, "thread_source": "user", "source": "cli"}}) + "\n" if provider == "codex" else "")
            control, receipts = root / (provider + ".fifo"), root / (provider + ".receipts")
            os.mkfifo(control)
            launch = ["env", "CONVERSATION_PROVIDER=" + provider, "CONVERSATION_TRANSCRIPT=" + str(transcript), "CONVERSATION_BIN=" + str(binary), "CONVERSATION_SESSION=" + session, "CONVERSATION_CONTROL=" + str(control), "CONVERSATION_RECEIPTS=" + str(receipts), "CODEX_FIXTURE_SCRIPT=" + str(Path(__file__).resolve()), str(root / "bin" / provider), "--no-daemon"]
            import shlex
            tmux("respawn-pane", "-k", "-t", pane, shlex.join(launch))
            detail = until(lambda: vt("agent", "get", pane)["result"]["agent"], lambda a: a["summary"]["identity"] == "exact" and a.get("agent_session_id") == session)
            agent_ref = detail["summary"]["agent_ref"]
            def send(**request):
                request["id"] = str(time.monotonic_ns())
                with control.open("w") as stream:
                    stream.write(json.dumps(request) + "\n")
                receipt = until(lambda: [json.loads(line) for line in receipts.read_text().splitlines()], lambda rows: any(row["id"] == request["id"] for row in rows))
                own = next(row for row in receipt if row["id"] == request["id"])
                assert own["code"] == 0, own
            prompt, final = "Scoped instruction\nPreserve this exact second line.", "Completed response\nFull second line."
            def user_record(index, parent=None):
                if provider == "codex":
                    return [{"type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-" + str(index)}}, {"type": "turn_context", "payload": {"turn_id": "turn-" + str(index)}}, {"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": prompt}]}}, {"type": "event_msg", "payload": {"type": "item_completed", "turn_id": "turn-" + str(index), "item": {"type": "UserMessage", "id": "u" + str(index), "content": [{"type": "text", "text": prompt}]}}}]
                return [{"type": "user", "uuid": "u" + str(index), "parentUuid": parent, "sessionId": session, "isSidechain": False, "promptId": "turn-" + str(index), "message": {"role": "user", "content": prompt}}]
            def final_records(index):
                if provider == "codex":
                    return [{"type": "response_item", "payload": {"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": final}]}}, {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-" + str(index), "last_agent_message": final}}]
                return [{"type": "assistant", "uuid": "a" + str(index), "parentUuid": "u" + str(index), "sessionId": session, "message": {"id": "m" + str(index), "role": "assistant", "content": [{"type": "text", "text": final}], "stop_reason": "end_turn"}}]
            fields = {"prompt": prompt, **({"turn_id" if provider == "codex" else "prompt_id": "turn-1"})}
            send(records=user_record(1), event="UserPromptSubmit", fields=fields)
            running = vt("agent", "conversation", agent_ref)["result"]["conversation"]
            assert running["status"] == "working" and running["history"]["messages"][-1]["text"] == prompt, running
            # Stop before flush: never return a fabricated or previous final response.
            send(event="Stop", fields={"last_assistant_message": final, **({"turn_id" if provider == "codex" else "prompt_id": "turn-1"})})
            unavailable = vt("agent", "conversation", agent_ref, expected=1)
            error = unavailable["error"]
            if os.environ.get("VDE_CONVERSATION_ERROR_FIXTURE") and provider == "codex":
                Path(os.environ["VDE_CONVERSATION_ERROR_FIXTURE"]).write_text(json.dumps(unavailable, indent=2) + "\n")
            assert error["code"] == "conversation_unavailable", error
            send(records=final_records(1))
            complete = until(lambda: vt("agent", "conversation", agent_ref)["result"]["conversation"], lambda c: c["status"] == "done")
            assert complete["run_seq"] == complete["completed_seq"] == 1 and complete["stop_boundary"], complete
            assert [m["text"] for m in complete["history"]["messages"] if m["role"] == "user"] == [prompt]
            assert complete["history"]["messages"][-1]["text"] == final
            if os.environ.get("VDE_CONVERSATION_RESPONSE_FIXTURE") and provider == "codex":
                Path(os.environ["VDE_CONVERSATION_RESPONSE_FIXTURE"]).write_text(json.dumps(vt("agent", "conversation", agent_ref), ensure_ascii=False, indent=2) + "\n")
            unchanged = vt("agent", "conversation", agent_ref, "--since-revision", complete["context_revision"])["result"]["conversation"]
            assert unchanged["update"] == "unchanged" and unchanged["history"]["messages"] == []
            timeout = vt("agent", "conversation", agent_ref, "--after-cursor", complete["cursor"], "--timeout-ms", "80", expected=1)["error"]
            assert timeout["code"] == "timeout", timeout
            # Same text in a later turn is still a different provider identity.
            send(records=user_record(2, "a1"))
            future = vt("agent", "conversation", agent_ref, expected=1)["error"]
            assert future["code"] == "conversation_unavailable", future
            waiter = subprocess.Popen([str(binary), "agent", "conversation", agent_ref, "--after-cursor", complete["cursor"], "--timeout-ms", "10000", "--json"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            send(event="UserPromptSubmit", fields={"prompt": prompt, **({"turn_id" if provider == "codex" else "prompt_id": "turn-2"})})
            stdout, stderr = waiter.communicate(timeout=12)
            if waiter.returncode:
                assert json.loads(stderr)["error"]["code"] in ["context_changed", "conversation_unavailable"], stderr
                changed = until(lambda: vt("agent", "conversation", agent_ref)["result"]["conversation"], lambda c: c["run_seq"] == 2)
            else:
                changed = json.loads(stdout)["result"]["conversation"]
            assert changed["run_seq"] == 2 and changed["cursor"] != complete["cursor"], changed
            # Transcript-only changes wake the existing daemon subscription.
            progress_waiter = subprocess.Popen([str(binary), "agent", "conversation", agent_ref, "--after-cursor", changed["cursor"], "--timeout-ms", "10000", "--json"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            progress = [{"type": "response_item", "payload": {"type": "message", "role": "assistant", "phase": "commentary", "content": [{"type": "output_text", "text": "Progress evidence."}]}}] if provider == "codex" else [{"type": "assistant", "uuid": "progress2", "parentUuid": "u2", "sessionId": session, "message": {"id": "mp2", "role": "assistant", "content": [{"type": "text", "text": "Progress evidence."}], "stop_reason": None}}]
            send(records=progress)
            stdout, stderr = progress_waiter.communicate(timeout=12)
            assert progress_waiter.returncode == 0, stderr
            updated = json.loads(stdout)["result"]["conversation"]
            assert updated["run_seq"] == 2 and updated["context_revision"] != changed["context_revision"], updated
            # The subscription process releases its socket when canceled.
            canceled = subprocess.Popen([str(binary), "agent", "conversation", agent_ref, "--after-cursor", updated["cursor"], "--timeout-ms", "10000", "--json"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(0.1)
            canceled.terminate()
            canceled.wait(timeout=5)
            second_final = final_records(2)
            if provider == "claude":
                second_final[0]["parentUuid"] = "progress2"
            send(records=second_final, event="Stop", fields={"last_assistant_message": final, **({"turn_id": "turn-2"} if provider == "codex" else {})})
            second = until(lambda: vt("agent", "conversation", agent_ref)["result"]["conversation"], lambda c: c["status"] == "done")
            assert second["provider_turn_id"] == "turn-2" and second["completed_seq"] == 2, second
            # Replacing an occupant invalidates the original reference.
            tmux("respawn-pane", "-k", "-t", pane, "/bin/bash --noprofile --norc")
            stale = until(lambda: vt("agent", "conversation", agent_ref, expected=1)["error"], lambda e: e["code"] in ["stale_reference", "agent_not_found"])
            assert stale["side_effect"] == "none", stale
            print(provider + ": exact full text, late flush, revisions, wait, cancellation, replacement OK")
        print("conversation API isolated acceptance OK")
    finally:
        if daemon_started:
            subprocess.run([str(binary), "daemon", "stop"], env=env, capture_output=True)
        subprocess.run(["tmux", "-L", sock, "kill-server"], env=env, capture_output=True)
        if os.environ.get("KEEP_ARTIFACTS") == "1":
            print("artifacts: " + str(root), file=sys.stderr)
        else:
            shutil.rmtree(root)


if __name__ == "__main__":
    fixture_agent() if "CONVERSATION_PROVIDER" in os.environ else main()

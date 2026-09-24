#!/usr/bin/env python3
"""Exercise the real vt hook/socket/API/sidebar path on an isolated tmux server.

The fixture supplies the stock Codex 0.155.1 PostToolUse schema and a verifiable
ancestor process. This does not replace acceptance with the actual Codex CLI.
"""
import json
import statistics
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
import socket as unix_socket
import termios
import threading
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from question_notice_statistics import PHASE_ORDER, percentile95, summarize, verdict

repo = Path(__file__).resolve().parent.parent
root = Path(tempfile.mkdtemp(prefix="vde-question-notice-"))
socket = "vde-question-" + str(os.getpid())
build = Path(os.environ.get("VDE_TMUX_TEST_BUILD_BIN", repo / "target/debug/vt"))
for directory in ["bin", "config/vde/tmux", "state", "runtime", "home", "fixture"]:
    (root / directory).mkdir(parents=True, exist_ok=True, mode=0o700)
shutil.copy2(build, root / "bin/vt")
shutil.copy2(repo / "scripts/fixtures/codex-question-hooks.py", root / "bin/hooks.py")
subprocess.run(["cc", "-O2", "-Wall", "-Wextra", "-Werror", str(repo / "scripts/fixtures/codex-question-owner.c"),
                "-o", str(root / "bin/codex")], check=True)
vt = str(root / "bin/vt")
daemon_socket = None
env = {**os.environ, "HOME": str(root / "home"), "ZDOTDIR": str(root / "home"),
       "XDG_CONFIG_HOME": str(root / "config"), "XDG_STATE_HOME": str(root / "state"),
       "XDG_RUNTIME_DIR": str(root / "runtime"), "VDE_TMUX_SOCKET_NAME": socket,
       "CODEX_HOME": str(root / "home/.codex"),
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


def start_daemon():
    global daemon_socket
    daemon_socket = run([vt, "daemon", "start"]).stdout.strip().removeprefix("daemon serving: ")
    assert Path(daemon_socket).is_absolute(), "unexpected daemon socket path"


def diagnostics():
    assert daemon_socket is not None, "daemon socket has not been established"
    with unix_socket.socket(unix_socket.AF_UNIX) as stream:
        stream.settimeout(3)
        stream.connect(daemon_socket)
        reader = stream.makefile("r")
        stream.sendall(b'{"op":"hello","proto":25}\n')
        json.loads(reader.readline())
        stream.sendall(b'{"op":"query_question_diagnostics","proto":25}\n')
        return json.loads(reader.readline())


shutdown_evidence = []


def stop_daemon(label, require_active_candidate=False):
    """Keep evidence for every stop, including subprocess timeouts, before cleanup."""
    def read_identity():
        lifecycle = next((root / "state").rglob("lifecycle.json"))
        return json.loads(lifecycle.read_text())["process"]
    identity, identity_failure = None, None
    try:
        identity = read_identity()
        if not identity:
            identity_failure = "missing_process"
        elif sys.platform == "linux":
            token = Path(f"/proc/{identity['pid']}/stat").read_text().rsplit(") ", 1)[1].split()[19]
            if token != identity["start_token"]:
                identity_failure = "identity_changed"
        else:
            token = run(["ps", "-o", "lstart=", "-p", str(identity["pid"])], check=False)
            if token.returncode or token.stdout.strip() != identity["start_token"]:
                identity_failure = "identity_absent_or_changed"
    except (OSError, ValueError, KeyError, TypeError, IndexError, StopIteration,
            subprocess.SubprocessError) as error:
        identity_failure = type(error).__name__
        identity = None
    diagnostic_failure = None
    try:
        before = diagnostics()["counters"]
        assert isinstance(before, dict), "invalid diagnostic counters"
    except (OSError, ValueError, KeyError, TypeError, AssertionError,
            subprocess.SubprocessError) as error:
        diagnostic_failure = type(error).__name__
        before = {}
    # Preserve bounded, body-free identity transitions to distinguish a slow
    # shutdown from an enabled hook starting a replacement during stop.
    watch_stop = threading.Event()
    identity_changes = []
    watch_started = time.monotonic()
    def observe_identity():
        previous = None
        while not watch_stop.is_set():
            try:
                current = read_identity()
                kind = "absent" if current is None else ("original" if current == identity else "other")
            except (OSError, ValueError, KeyError, TypeError, StopIteration):
                kind = "unreadable"
            if kind != previous and len(identity_changes) < 64:
                identity_changes.append({"ms": (time.monotonic() - watch_started) * 1000, "identity": kind})
                previous = kind
            watch_stop.wait(.005)
    watcher = threading.Thread(target=observe_identity)
    watcher.start()
    began = time.monotonic()
    result, failure = None, None
    try:
        result = run([vt, "daemon", "stop"], check=False)
        if result.returncode:
            failure = "nonzero_exit"
    except (OSError, subprocess.SubprocessError) as error:
        failure = type(error).__name__
    finally:
        watch_stop.set()
        watcher.join()
    stop_failure = failure
    active_candidates = sum(before.get(key, 0) for key in ["checking", "armed", "probing"])
    if identity_failure or diagnostic_failure:
        failure = "not_serving_before_stop"
    elif require_active_candidate and active_candidates == 0:
        failure = "no_active_candidates_at_stop"
    elif failure and any(change["identity"] == "other" for change in identity_changes):
        failure = "replacement_started"
    elapsed = (time.monotonic() - began) * 1000
    entry = {"label": label, "elapsed_ms": elapsed, "near_deadline": elapsed > 1500,
             "exit_code": result.returncode if result else None, "failure": failure,
             "before": before, "sample": "not_needed", "stop_failure": stop_failure,
             "identity_failure": identity_failure, "diagnostic_failure": diagnostic_failure,
             "identity_changes": identity_changes, "active_candidates_at_stop": active_candidates}
    shutdown_evidence.append(entry)
    evidence_path = root / "shutdown-evidence.json"
    evidence_path.write_text(json.dumps(shutdown_evidence, indent=2) + "\n")
    if failure:
        entry["sample"] = "unsupported_platform_or_missing_identity"
        if sys.platform == "darwin" and identity:
            try:
                # Match the daemon's lifecycle start-token contract immediately
                # before sampling. Never sample a replacement with a reused PID.
                token = run(["ps", "-o", "lstart=", "-p", str(identity["pid"])], check=False)
                if token.returncode == 0 and token.stdout.strip() == identity["start_token"]:
                    sampled = subprocess.run(
                        ["sample", str(identity["pid"]), "2", "10", "-file",
                         str(root / f"shutdown-{len(shutdown_evidence)}.sample")],
                        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
                    entry["sample"] = "captured" if sampled.returncode == 0 else "sampler_failed"
                else:
                    entry["sample"] = "identity_absent_or_changed"
            except (OSError, subprocess.SubprocessError) as error:
                entry["sample"] = type(error).__name__
        evidence_path.write_text(json.dumps(shutdown_evidence, indent=2) + "\n")
        raise AssertionError(f"daemon stop failed ({label}): {failure}; evidence: {evidence_path}")
    return entry


def emit_at(directory, event, **fields):
    result = directory / "result.json"
    result.unlink(missing_ok=True)
    temporary = directory / "request.tmp"
    temporary.write_text(json.dumps({"event": event, "fields": fields}))
    temporary.replace(directory / "request.json")
    wait(result.exists, event, timeout=20)
    value = json.loads(result.read_text())
    assert value["code"] == 0, value
    return value


def wait(predicate, label, timeout=15, interval=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (subprocess.CalledProcessError, AssertionError, KeyError, json.JSONDecodeError):
            pass
        time.sleep(interval)
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


def fixed_drop_retention(fixture):
    # All other producers are stopped. This is the sole candidate, so a drop
    # counter delta identifies this exact owner/order, not another pane's notice.
    wait(lambda: all(diagnostics()["counters"][key] == 0 for key in ["checking", "armed", "probing"]),
         "drained candidates before fixed drop case", timeout=20)
    time.sleep(1.6)  # Expire all previously queued 1.5-second probe jobs.
    # Use a new clean startup for this independent drop case. Resource evidence
    # below separately records any session loss during the preceding stress;
    # replacing one fixture keeps the 58-pane population unchanged.
    tmux("kill-window", "-t", "questions:load-56")
    fixture = root / "fixed-drop"
    fixture.mkdir()
    target = tmux("new-window", "-d", "-P", "-F", "#{pane_id}", "-n", "load-56",
                  "-c", str(root), fixture_command(fixture))
    wait((fixture / "ready").exists, "fixed drop clean startup")
    assert json.loads((fixture / "startup.json").read_text())["code"] == 0
    # Fault-inject only the external normal-capture subprocess in this scratch
    # daemon. Its existing 1-second timeout and queue limits remain unchanged.
    hold, entered = root / "hold-normal-capture", root / "normal-capture-entered"
    wrapper = root / "bin/tmux"
    real_tmux = shutil.which("tmux", path=os.environ["PATH"])
    blocker = root / "bin/hold-normal-capture.py"
    blocker.write_text("import time\nfrom pathlib import Path\np=Path(" + repr(str(hold)) + ")\n"
                       "Path(" + repr(str(entered)) + ").touch()\nend=time.monotonic()+0.85\n"
                       "while p.exists() and time.monotonic()<end: time.sleep(.005)\n")
    wrapper.write_text("#!/bin/sh\ncase \" $* \" in *capture-pane*-80*)\n"
                       "if [ -e " + shlex.quote(str(hold)) + " ]; then "
                       + shlex.join([sys.executable, str(blocker)]) + "; fi;; esac\nexec "
                       + shlex.quote(real_tmux) + " \"$@\"\n")
    wrapper.chmod(0o700)
    evidence = {"matched_drop": False}
    try:
        for attempt in range(4):
            issued, accepted = f"drop-{attempt}-a", f"drop-{attempt}-b"
            emit_at(fixture, "UserPromptSubmit", turn_id=issued, prompt="synthetic fixed drop issuer")
            emit_at(fixture, "PostToolUse", turn_id=issued, tool_name="request_user_input_async",
                    tool_use_id=issued, tool_response='{"accepted":true}', _ui="normal")
            emit_at(fixture, "Stop", turn_id=issued)
            before = query("agent", "get", target)["agent"]["summary"]["question_notice"]
            prepared = False
            for preparation in range(8):
                accepted = f"drop-{attempt}-b-{preparation}"
                emit_at(fixture, "UserPromptSubmit", turn_id=accepted, prompt="synthetic fixed drop ordinary input")
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    if diagnostics()["counters"]["armed"] == 1:
                        prepared = True
                        break
                    time.sleep(.02)
                if prepared:
                    break
                # A distinct accepted prompt may prepare a new candidate when a
                # prior order check conservatively retained on its 100ms budget.
                # This is only setup; no new prompt is sent after the tested drop.
                emit_at(fixture, "Stop", turn_id=accepted)
            if not prepared:
                (root / "fixed-drop-failure.json").write_text(json.dumps(diagnostics()["counters"]))
                raise AssertionError("fixed drop setup never armed a candidate")
            dropped = diagnostics()["counters"]["capture"]["probe_dropped"]
            entered.unlink(missing_ok=True)
            hold.touch()
            wait(entered.exists, "normal capture in flight")
            emit_at(fixture, "Stop", turn_id=accepted)
            deadline = time.monotonic() + 1.7
            delta = 0
            while time.monotonic() < deadline:
                delta = diagnostics()["counters"]["capture"]["probe_dropped"] - dropped
                if delta: break
                time.sleep(.02)
            hold.unlink(missing_ok=True)
            if delta:
                wait(lambda: all(diagnostics()["counters"][key] == 0 for key in ["checking", "armed", "probing"]),
                     "fixed dropped candidate completion applied")
                time.sleep(1.6)  # All bounded in-flight completions have expired.
            after = query("agent", "get", target)["agent"]["summary"]["question_notice"]
            if delta:
                # No later UPS, issue or Q is sent to this owner after the drop.
                evidence = {"matched_drop": True, "drop_count": delta, "attempt": attempt,
                            "preparation_prompts": preparation + 1,
                            "owner_ref": before["owner_ref"], "latest_order": before["latest_order"],
                            "ack_before": before["acknowledged_order"], "ack_after": after["acknowledged_order"],
                            "owner_unchanged": after["owner_ref"] == before["owner_ref"],
                            "latest_unchanged": after["latest_order"] == before["latest_order"],
                            "unacknowledged": after["unacknowledged"]}
                break
        return evidence
    finally:
        hold.unlink(missing_ok=True)
        wrapper.unlink(missing_ok=True)


def assert_auto_ack_under_load(phase_result):
    assert phase_result["while_producers_active"]["candidates_acked"] > 0, (
        phase_result["phase"], "no automatic acknowledgement under load",
        phase_result["while_producers_active"])


def benchmark():
    fixtures = []
    for index in range(57):
        fixture = root / f"load-{index}"
        fixture.mkdir(mode=0o700)
        fixtures.append(fixture)
        tmux("new-window", "-d", "-n", f"load-{index}", "-c", str(root),
             fixture_command(fixture))
        wait((fixture / "ready").exists, "load fixture startup", timeout=15)
        assert json.loads((fixture / "startup.json").read_text())["code"] == 0
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

    # Baseline: identical panes, clients, daemon and ordinary observation work,
    # with no resolver candidates. Keep it separate from the probe/reader phase.
    for index, fixture in enumerate(fixtures):
        wait((fixture / "ready").exists, "load fixture ready")
        emit_at(fixture, "UserPromptSubmit", turn_id=f"baseline-{index}", prompt="synthetic baseline")
    def status_delivery(phase, phase_started, iterations=20):
        values, api_values, observations = [], [], []
        def observe(predicate, ready):
            ready.wait()
            deadline = time.monotonic() + 15
            last_false = None
            durations = []
            while time.monotonic() < deadline:
                began = time.monotonic()
                matched = predicate()
                ended = time.monotonic()
                durations.append((ended - began) * 1000)
                if matched:
                    return {"last_false": last_false, "first_true": ended,
                            "first_true_query_started": began, "query_durations_ms": durations}
                last_false = ended
                time.sleep(.01)  # Minimum idle interval; query duration is recorded separately.
            raise AssertionError("independent status observer timed out")
        with ThreadPoolExecutor(max_workers=2) as observers:
            for index in range(iterations):
                turn = f"status-{phase}-{index}"
                for event, state, color in [("UserPromptSubmit", "working", "#4fd08a"),
                                             ("Stop", "done", "#45cbe6")]:
                    previous_run = agent()["summary"].get("current_run")
                    result_path = root / "fixture/result.json"
                    result_path.unlink(missing_ok=True)
                    request_path = root / "fixture/request.tmp"
                    request_path.write_text(json.dumps({"event": event, "fields": {
                        "turn_id": turn, "prompt": "synthetic status delivery"}}))
                    def api_ready():
                        current = agent()["summary"]
                        return current["status"] == state and (event != "UserPromptSubmit" or
                                                                 current.get("current_run") != previous_run)
                    ready = threading.Barrier(3)
                    api_future = observers.submit(observe, api_ready, ready)
                    rail_future = observers.submit(observe, lambda: f"#[fg={color}]─" in
                        tmux("show-options", "-pqv", "-t", pane, "@vde_status_pane"), ready)
                    ready.wait()
                    request_path.replace(root / "fixture/request.json")
                    api, rail = api_future.result(), rail_future.result()
                    wait(result_path.exists, "normal status hook completion")
                    result = json.loads(result_path.read_text())
                    assert result["code"] == 0, result
                    started = result["started"]
                    observed = max(api["first_true"], rail["first_true"])
                    # Independent observers: one path never waits for the other's query.
                    values.append((rail["first_true"] - started) * 1000)
                    api_values.append((api["first_true"] - started) * 1000)
                    for item in [api, rail]:
                        for key in ["last_false", "first_true", "first_true_query_started"]:
                            if item[key] is not None:
                                item[key + "_ms"] = (item[key] - started) * 1000
                            del item[key]
                    observations.append({"api": api, "rail": rail,
                                         "interval_start_ms": (started - phase_started) * 1000,
                                         "interval_end_ms": (observed - phase_started) * 1000})
        return {"rail_ms": values, "api_ms": api_values,
                "both_ms": [max(api, rail) for api, rail in zip(api_values, values)],
                "observations": observations}

    pidfiles = list((root / "state").rglob("lifecycle.json"))
    assert len(pidfiles) == 1, pidfiles
    daemon_pid = json.loads(pidfiles[0].read_text())["process"]["pid"]

    def rss_kib():
        return int(run(["ps", "-o", "rss=", "-p", str(daemon_pid)]).stdout.strip())

    def journal_evidence():
        files = list((root / "state/vde-tmux/codex-question-journal-v1").glob("*.json"))
        assert len(files) == 1, files
        journal = json.loads(files[0].read_text())
        return {"epoch": journal["home_dirty_epoch"], "dirty_count": len(journal["dirty"])}

    def resolver_evidence(value):
        return {key: value[key] for key in ["trusted", "unknown", "home_failures", "invalid_observations", "retained_by"]}

    rss_baseline = rss_kib()
    load_errors = []
    load_stop = threading.Event()

    def load_one(index, fixture, resolve, phase, phase_started, activity, ready):
        activity["started_ms"] = (time.monotonic() - phase_started) * 1000
        activity["cycles"] = 0
        activity["hook_started_ms"] = []
        ready.set()
        def send(event, **fields):
            activity["hook_started_ms"].append((time.monotonic() - phase_started) * 1000)
            return emit_at(fixture, event, **fields)
        try:
            while not load_stop.is_set():
                cycle = activity["cycles"]
                issued = f"{phase}-{index}-{cycle}-a"
                accepted = f"{phase}-{index}-{cycle}-b"
                prompt = "synthetic ordinary load input" if resolve else '<send_user_message_question_reply>{"questionItemId":"synthetic"}</send_user_message_question_reply>'
                send("UserPromptSubmit", turn_id=issued, prompt=prompt, _padding=16384)
                send("PostToolUse", turn_id=issued,
                        tool_name="request_user_input_async",
                        tool_use_id=issued, tool_response='{"accepted":true}', _ui="normal")
                send("Stop", turn_id=issued)
                send("UserPromptSubmit", turn_id=accepted, prompt=prompt)
                send("Stop", turn_id=accepted)
                activity["cycles"] += 1
                load_stop.wait(1.7)  # Let each candidate finish before a new ordinary trigger.
        except Exception as error:
            load_errors.append(str(error))
        finally:
            activity["ended_ms"] = (time.monotonic() - phase_started) * 1000
            activity["stopped_by_phase"] = load_stop.is_set()

    rss_peak = rss_baseline

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
        rss_peak = max(rss_peak, rss_kib())
    # Keep background normal hook pressure identical in both phases. The baseline
    # sends canonical answer framing, so all issue/journal/sidecar IO remains but
    # no OrdinaryPrompt creates resolver candidate/probe work.
    stress_begin = diagnostics()["counters"]
    journal_begin = journal_evidence()
    baseline = stress_begin
    rss_baseline = rss_kib()
    rss_peak = rss_baseline
    phase_results = []
    baseline_samples, load_samples = [], []
    baseline_api, load_api = [], []
    load_probes = load_drops = load_retained = 0
    def monitor_rss():
        nonlocal rss_peak
        while not load_stop.wait(0.2):
            rss_peak = max(rss_peak, rss_kib())
    # Two fixed ABBA cycles, 2000 events per condition. The plan's gate uses
    # condition-wise observed p95; all load and health validity checks remain.
    for phase, resolve in PHASE_ORDER:
        load_stop.clear()
        before = diagnostics()["counters"]
        monitor = threading.Thread(target=monitor_rss)
        monitor.start()
        phase_started = time.monotonic()
        producer_activity = [{"producer": index} for index in range(3)]
        producer_ready = [threading.Event() for _ in range(3)]
        with ThreadPoolExecutor(max_workers=3) as pool:
            tasks = [pool.submit(load_one, index, fixture, resolve, phase, phase_started,
                                 producer_activity[index], producer_ready[index])
                     for index, fixture in enumerate(fixtures[:3])]
            try:
                assert all(ready.wait(5) for ready in producer_ready), "producer startup timed out"
                under_load_before = diagnostics()["counters"]
                measured = status_delivery(phase, phase_started, iterations=250)
                # This observation precedes load_stop: all producer tasks still
                # run. A completion during the later drain cannot satisfy the gate.
                under_load_after = diagnostics()["counters"]
            finally:
                load_stop.set()
                monitor.join()
        wait(lambda: all(diagnostics()["counters"][key] == 0 for key in ["checking", "armed", "probing"]),
             "phase candidates drained", timeout=20)
        after = diagnostics()["counters"]
        covered = all(activity["started_ms"] <= sample["interval_start_ms"] <=
                      sample["interval_end_ms"] <= activity["ended_ms"]
                      for sample in measured["observations"] for activity in producer_activity)
        interval_start = measured["observations"][0]["interval_start_ms"]
        interval_end = measured["observations"][-1]["interval_end_ms"]
        measurement_seconds = (interval_end - interval_start) / 1000
        hook_count = sum(interval_start <= started <= interval_end for activity in producer_activity
                         for started in activity["hook_started_ms"])
        effectiveness = {key: after[key] - before[key] for key in
                         ["candidates_started", "candidates_armed", "candidates_acked"]}
        effectiveness["acked_per_armed"] = (effectiveness["candidates_acked"] / effectiveness["candidates_armed"]
                                               if effectiveness["candidates_armed"] else None)
        effectiveness["mutation_busy_retained"] = (after["retained_by"].get("mutation_busy", 0)
                                                     - before["retained_by"].get("mutation_busy", 0))
        status_before, status_after = before["status_push"], after["status_push"]
        age_observed = sum(status_after["timings"].get("publication_to_snapshot", {}).get("buckets", [])) - sum(
            status_before["timings"].get("publication_to_snapshot", {}).get("buckets", []))
        age_unavailable = status_after["publication_age_unavailable"] - status_before["publication_age_unavailable"]
        age_total = age_observed + age_unavailable
        phase_results.append({"phase": phase, **measured,
                              "producers": producer_activity, "all_samples_under_load": covered,
                              "measurement_seconds": measurement_seconds,
                              "background_hook_count": hook_count,
                              "background_hooks_per_second": hook_count / measurement_seconds,
                              "health_before": resolver_evidence(before),
                              "health_after": resolver_evidence(after),
                              "mutation_before": before["mutation_queue"]["timings"],
                              "mutation_after": after["mutation_queue"]["timings"],
                              "queue_before": before["mutation_queue"]["queue_timings"],
                              "queue_after": after["mutation_queue"]["queue_timings"],
                              "journal_failures_before": before["journal_failures"],
                              "journal_failures_after": after["journal_failures"],
                              "status_push_before": before["status_push"],
                              "status_push_after": after["status_push"],
                              "publication_age_coverage": {"observed": age_observed,
                                  "unavailable": age_unavailable,
                                  "unavailable_fraction": age_unavailable / age_total if age_total else None},
                              "auto_ack_effectiveness_including_drain": effectiveness,
                              "while_producers_active": {key: under_load_after[key] - under_load_before[key]
                                  for key in ["candidates_started", "candidates_armed", "candidates_acked"]},
                              "automatic_acks_after_stop": after["candidates_acked"] - under_load_after["candidates_acked"]})
        # Save each phase before asserting, so a failed load cannot masquerade as
        # a valid measurement and its activity intervals remain inspectable.
        (root / "status-phases.json").write_text(json.dumps(phase_results, indent=2) + "\n")
        assert not load_errors and covered, "producer failed or measurement outlived load"
        assert all(activity["stopped_by_phase"] and activity["cycles"] > 0
                   for activity in producer_activity), producer_activity
        for key in ["trusted", "unknown", "home_failures", "invalid_observations"]:
            assert before[key] == after[key], (phase, key, before[key], after[key])
        if resolve:
            assert_auto_ack_under_load(phase_results[-1])
        (load_samples if resolve else baseline_samples).extend(measured["both_ms"])
        (load_api if resolve else baseline_api).extend(measured["api_ms"])
        if resolve:
            load_probes += sum(after["capture"]["probe_classes"]) - sum(before["capture"]["probe_classes"])
            load_drops += after["capture"]["probe_dropped"] - before["capture"]["probe_dropped"]
            load_retained += after["retained"] - before["retained"]
    loaded = diagnostics()["counters"]
    rss_peak = max(rss_peak, rss_kib())
    status_statistics = summarize(phase_results)
    retained_notices = sum(item["question_notice"]["unacknowledged"]
                           for item in query("agent", "list")["agents"])
    background_rates = {}
    for condition in ["baseline", "load"]:
        phases = [item for item in phase_results if item["phase"].startswith(condition)]
        background_rates[condition] = sum(item["background_hook_count"] for item in phases) / sum(
            item["measurement_seconds"] for item in phases)
    rate_ratio = background_rates["load"] / background_rates["baseline"]
    phase_rates = {item["phase"]: item["background_hooks_per_second"] for item in phase_results}
    phase_rate_ratios = [phase_rates[f"load-{index}"] / phase_rates[f"baseline-{index}"]
                         for index in range(4)]
    comparable_load = all(0.9 <= ratio <= 1 / 0.9 for ratio in [rate_ratio, *phase_rate_ratios])
    resource = {"measurement": "normal hook launch through API and pane status rail delivery observed concurrently with the hook; two ABBA cycles with 2000 samples per condition; condition-wise observed p95 of event-wise max(API, rail); independent observers with minimum 10ms idle and recorded query times; same 3 background hook producers and question persistence; baseline answer framing vs loaded ordinary inputs",
                "baseline_p95_ms": percentile95(baseline_samples), "load_p95_ms": percentile95(load_samples),
                "normal_capture_failures": loaded["capture"]["normal_failures"] - baseline["capture"]["normal_failures"],
                "rss_baseline_kib": rss_baseline, "rss_peak_kib": rss_peak,
                "rss_increase_mib": max(0, rss_peak - rss_baseline) / 1024,
                "baseline_status_samples_ms": baseline_samples, "load_status_samples_ms": load_samples,
                "load_probe_count": load_probes, "load_probe_dropped": load_drops, "load_retained": load_retained,
                "phase_results": phase_results,
                "load_auto_acknowledgements": [{key: phase[key] for key in
                    ["phase", "while_producers_active", "automatic_acks_after_stop",
                     "auto_ack_effectiveness_including_drain"]}
                    for phase in phase_results if phase["phase"].startswith("load")],
                "background_hooks_per_second": background_rates,
                "background_rate_load_to_baseline": rate_ratio,
                "background_phase_rate_load_to_baseline": phase_rate_ratios,
                "comparable_background_load": comparable_load,
                "baseline_api_p95_ms": percentile95(baseline_api), "load_api_p95_ms": percentile95(load_api),
                "baseline_rail_p95_ms": percentile95([v for p in phase_results if p["phase"].startswith("baseline") for v in p["rail_ms"]]),
                "load_rail_p95_ms": percentile95([v for p in phase_results if p["phase"].startswith("load") for v in p["rail_ms"]]),
                "baseline_median_ms": statistics.median(baseline_samples), "load_median_ms": statistics.median(load_samples),
                "status_statistics": status_statistics,
                "retained_notices": retained_notices, "load_errors": load_errors, "resolver": loaded,
                "session_health": {"before_baseline": resolver_evidence(stress_begin),
                                   "after_load": resolver_evidence(loaded),
                                   "journal_before": journal_begin, "journal_after": journal_evidence()}}
    resource["performance_verdict"] = verdict(status_statistics, comparable_load)
    (root / "resource-budget.json").write_text(json.dumps(resource, indent=2) + "\n")
    resource["fixed_drop_retention"] = fixed_drop_retention(fixtures[-1])
    (root / "resource-budget.json").write_text(json.dumps(resource, indent=2) + "\n")
    report = {"agent_panes": 58, "clients": 2, "poll_ms": 1000, "iterations": 100,
              "clock": "monotonic; issue before hook process launch, Q before control request (conservative upper bounds)",
              "max_ms": {key: max(sample[key] for sample in samples) for key in samples[0]}, "samples": samples}
    (root / "latency.json").write_text(json.dumps(report, indent=2) + "\n")
    # Full bounded numeric evidence stays in JSON; failure output is a small summary.
    gate_summary = {key: resource[key] for key in ["baseline_p95_ms", "load_p95_ms",
        "normal_capture_failures", "rss_increase_mib", "performance_verdict", "status_statistics"]}
    assert not load_errors, gate_summary
    assert resource["load_probe_count"] >= 4, gate_summary
    fixed = resource["fixed_drop_retention"]
    assert fixed["matched_drop"] and fixed["owner_unchanged"] and fixed["latest_unchanged"], gate_summary
    assert fixed["unacknowledged"] and fixed["ack_before"] == fixed["ack_after"], gate_summary
    assert resource["load_p95_ms"] - resource["baseline_p95_ms"] <= 50, gate_summary
    assert resource["performance_verdict"] == "pass", gate_summary
    assert resource["normal_capture_failures"] == 0, gate_summary
    assert resource["rss_increase_mib"] <= 20, gate_summary
    print("resource: " + json.dumps({key: resource[key] for key in ["baseline_p95_ms", "load_p95_ms", "normal_capture_failures", "rss_increase_mib", "load_probe_count", "load_probe_dropped", "session_health", "performance_verdict", "load_auto_acknowledgements"]}))
    print("58 panes / two clients / 100 issue + Q cycles: " + json.dumps(report["max_ms"]))


started_daemon = False


def fixture_command(directory):
    return shlex.join([str(root / "bin/codex"), sys.executable, str(root / "bin/hooks.py"), str(directory)])


def auto_ack_cases():
    global pane, pane_ref
    old_pane, old_ref = pane, pane_ref
    directory = root / "auto-ack"
    directory.mkdir()
    pane = tmux("new-window", "-d", "-P", "-F", "#{pane_id}", "-n", "auto-ack", fixture_command(directory))
    wait((directory / "ready").exists, "auto-ack startup")
    startup = json.loads((directory / "startup.json").read_text())
    assert startup["code"] == 0, startup
    assert diagnostics()["counters"]["trusted"] >= 1
    wait(lambda: agent()["summary"]["identity"] == "exact", "auto-ack identity")
    pane_ref = agent()["summary"]["pane_ref"]

    def send(event, **fields):
        result = directory / "result.json"
        result.unlink(missing_ok=True)
        temporary = directory / "request.tmp"
        temporary.write_text(json.dumps({"event": event, "fields": fields}))
        temporary.replace(directory / "request.json")
        wait(result.exists, event)
        value = json.loads(result.read_text())
        assert value["code"] == 0, value

    def issue(turn, call):
        send("UserPromptSubmit", turn_id=turn, prompt="synthetic ordinary request")
        send("PostToolUse", turn_id=turn, tool_name="request_user_input_async", tool_use_id=call,
             tool_response='{"accepted":true}', _ui="question")
        send("Stop", turn_id=turn)
        assert notice()["unacknowledged"]

    # Answer framing is a no-op even if accepted in a later turn with a normal viewport.
    issue("a", "a-question")
    send("UserPromptSubmit", turn_id="answer", prompt='<send_user_message_question_reply>{"questionItemId":"synthetic"}</send_user_message_question_reply>', _ui="normal")
    send("Stop", turn_id="answer")
    time.sleep(0.7)
    assert notice()["unacknowledged"], "answer framing acknowledged notice"
    # Direct and accepted queued payloads are identical; only acceptance triggers this.
    for turn in ["direct", "accepted-queue"]:
        send("UserPromptSubmit", turn_id=turn, prompt="synthetic next ordinary request", _ui="normal")
        send("Stop", turn_id=turn)
        try:
            wait(lambda: not notice()["unacknowledged"], turn + " automatic acknowledgement", timeout=6)
        except AssertionError:
            print("auto-ack counters: " + json.dumps(diagnostics()), file=sys.stderr)
            raise
        if turn == "direct":
            issue("a2", "a2-question")
            time.sleep(0.7)  # Queue registration produces no hook and cannot resolve anything.
            assert notice()["unacknowledged"]
    issue("veto-a", "veto-question")
    send("UserPromptSubmit", turn_id="veto-b", prompt="synthetic next ordinary request", _ui="overlay")
    send("Stop", turn_id="veto-b")
    time.sleep(0.8)
    assert notice()["unacknowledged"], "unknown overlay acknowledged notice"
    ack(notice())
    tmux("kill-pane", "-t", pane)
    pane, pane_ref = old_pane, old_ref
    print("auto-ack: direct/accepted queue, answer framing, no-hook queue, and overlay veto passed")


def excluded_mode_lifecycle():
    directory = root / "excluded-mode"
    directory.mkdir()
    command = shlex.join([str(root / "bin/codex"), "--remote=synthetic", sys.executable,
                          str(root / "bin/hooks.py"), str(directory)])
    target = tmux("new-window", "-d", "-P", "-F", "#{pane_id}", "-n", "excluded-mode", command)
    wait((directory / "ready").exists, "excluded mode startup")
    assert json.loads((directory / "startup.json").read_text())["code"] == 0
    def summary():
        return query("agent", "get", target)["agent"]["summary"]
    emit_at(directory, "UserPromptSubmit", turn_id="excluded", prompt="synthetic mode lifecycle")
    wait(lambda: summary()["status"] == "working", "excluded mode UPS lifecycle")
    # This existing rejection is returned to the hook while lifecycle still applies.
    result = directory / "result.json"
    result.unlink()
    temporary = directory / "request.tmp"
    temporary.write_text(json.dumps({"event": "PostToolUse", "fields": {
        "turn_id": "excluded", "tool_name": "request_user_input_async", "tool_use_id": "excluded",
        "tool_response": '{"accepted":true}'}}))
    temporary.replace(directory / "request.json")
    wait(result.exists, "excluded mode question")
    reply = json.loads(result.read_text())
    assert reply["code"] != 0 and "AncestorNotInPane" in reply["error"], reply
    assert not summary()["question_notice"]["unacknowledged"]
    emit_at(directory, "Stop", turn_id="excluded")
    wait(lambda: summary()["status"] == "done", "excluded mode Stop lifecycle")
    tmux("kill-pane", "-t", target)
    print("excluded mode: rejected notice and preserved UPS/Stop lifecycle passed")


def shutdown_under_question_load():
    evidence = []
    for iteration in range(3):
        workers = []
        for index in range(3):
            directory = root / f"shutdown-{iteration}-{index}"
            directory.mkdir()
            target = tmux("new-window", "-d", "-P", "-F", "#{pane_id}", "-n", directory.name,
                          fixture_command(directory))
            wait((directory / "ready").exists, "shutdown fixture startup")
            startup = json.loads((directory / "startup.json").read_text())
            assert startup["code"] == 0, startup
            wait(lambda: query("agent", "get", target)["agent"]["summary"]["identity"] == "exact",
                 "shutdown fixture owner identity")
            emit_at(directory, "UserPromptSubmit", turn_id="a", prompt="synthetic shutdown issuer")
            emit_at(directory, "PostToolUse", turn_id="a", tool_name="request_user_input_async",
                    tool_use_id="shutdown", tool_response='{"accepted":true}', _ui="normal")
            emit_at(directory, "Stop", turn_id="a")
            workers.append((directory, target))
        for directory, _ in workers:
            emit_at(directory, "UserPromptSubmit", turn_id="b", prompt="synthetic shutdown trigger")
        before = diagnostics()["counters"]
        assert sum(before[key] for key in ["checking", "armed", "probing"]) > 0, before
        # Keep one owner Armed (B is still working) so candidate presence does
        # not depend on how quickly probes finish. Stop the other two concurrently
        # and drain their hook delivery before stopping the daemon.
        # `stop` leaves auto-start enabled; an unfinished hook could otherwise
        # start a replacement and confound this question-worker shutdown case.
        for directory, _ in workers[:2]:
            (directory / "result.json").unlink(missing_ok=True)
            temporary = directory / "request.tmp"
            temporary.write_text(json.dumps({"event": "Stop", "fields": {"turn_id": "b"}}))
            temporary.replace(directory / "request.json")
        for directory, _ in workers[:2]:
            wait((directory / "result.json").exists, "shutdown Stop hook delivery")
            result = json.loads((directory / "result.json").read_text())
            assert result["code"] == 0, result
        after_hooks = diagnostics()["counters"]
        assert sum(after_hooks[key] for key in ["checking", "armed", "probing"]) > 0, after_hooks
        entry = {**stop_daemon(f"question-load-{iteration}", require_active_candidate=True),
                 "active_candidates_before": before,
                 "active_candidates_after_hooks": after_hooks}
        evidence.append(entry)
        (root / "shutdown-load.json").write_text(json.dumps(evidence, indent=2) + "\n")
        for _, target in workers:
            tmux("kill-window", "-t", target)
        start_daemon()
    print("question-load shutdown: three concurrent owners, three rounds passed")


try:
    tmux("-f", "/dev/null", "new-session", "-d", "-s", "questions", "-x", "200", "-y", "60", "/bin/bash --noprofile --norc")
    tmux("set-option", "-g", "default-shell", "/bin/bash")
    tmux("set-option", "-g", "default-command", "/bin/bash --noprofile --norc")
    env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0")
    env["TMUX_PANE"] = tmux("display-message", "-p", "-t", "questions:0", "#{pane_id}")
    started_daemon = True
    start_daemon()
    pane = tmux("new-window", "-d", "-P", "-F", "#{pane_id}", "-n", "agent", "-c", str(root),
                fixture_command(root / "fixture"))
    wait((root / "fixture/ready").exists, "fixture startup")
    wait(lambda: agent()["summary"]["identity"] == "exact", "exact Codex identity")
    pane_ref = agent()["summary"]["pane_ref"]
    auto_ack_cases()
    excluded_mode_lifecycle()
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
    stop_daemon("rehydration")
    start_daemon()
    wait(lambda: notice().get("owner_ref") == saved["owner_ref"], "rehydration and owner rebind")
    assert notice()["unacknowledged"]
    ack(saved)
    post("call-3")
    assert not notice()["unacknowledged"], "restart lost acknowledged dedup key"
    sidecars = list((root / "state").rglob("question-notices-v1.json"))
    assert len(sidecars) == 1
    assert "fixture-private" not in sidecars[0].read_text()
    assert sidecars[0].stat().st_mode & 0o777 == 0o600
    # Exercise the real scratch state directory boundary, including restart.
    sidecar = sidecars[0]
    saved_sidecar = sidecar.read_bytes()
    marker = sidecar.with_name("question-notices-v1.expected")
    assert marker.exists() and marker.stat().st_mode & 0o777 == 0o600
    for fault in ["missing", "corrupt"]:
        stop_daemon(f"before-{fault}-sidecar")
        if fault == "missing":
            sidecar.unlink()
        else:
            sidecar.write_text("invalid synthetic sidecar")
        start_daemon()
        wait(lambda: notice()["tracking_health"] != "healthy", fault + " expected sidecar unhealthy")
        stop_daemon(f"restore-{fault}-sidecar")
        sidecar.write_bytes(saved_sidecar)
        sidecar.chmod(0o600)
        start_daemon()
        wait(lambda: notice()["tracking_health"] == "healthy", "restored sidecar healthy")
    # A hook invoked outside the exact pane ancestry cannot create a notice there.
    outside = {"hook_event_name": "PostToolUse", "session_id": "question-fixture-root", "turn_id": "question-fixture-turn",
               "transcript_path": str(root / "home/.codex/sessions/question-fixture-root.jsonl"), "tool_name": "request_user_input_async",
               "tool_use_id": "outside", "tool_response": '{"accepted":true}'}
    external_env = {**env, "TMUX_PANE": pane}
    external = subprocess.run([vt, "hook", "codex", "PostToolUse"], env=external_env, input=json.dumps(outside), text=True, capture_output=True, timeout=10)
    assert external.returncode != 0 and "AncestorNotInPane" in external.stderr, external.stderr
    assert not notice()["unacknowledged"]
    shutdown_under_question_load()
    if "--extended" in sys.argv:
        benchmark()
    post("last")
    tmux("kill-pane", "-t", pane)
    wait(lambda: all(owner["pane"]["pane_id"] != pane for owner in json.loads(sidecars[0].read_text())["owners"]),
         "confirmed owner cleanup")
    print("question notices: hook ancestry, API, sidebar rendering, fenced ack, dedup, restart, Stop, privacy, and owner cleanup passed")
    print("shutdown evidence: " + json.dumps({"count": len(shutdown_evidence),
          "max_ms": max(item["elapsed_ms"] for item in shutdown_evidence),
          "near_deadline_count": sum(item["near_deadline"] for item in shutdown_evidence)}))
finally:
    if started_daemon:
        run([vt, "daemon", "disable"], check=False)
    run(["tmux", "-L", socket, "kill-server"], check=False)
    for process, master, thread in clients:
        process.wait(timeout=5)
        thread.join(timeout=1)
        os.close(master)
    if os.environ.get("KEEP_ARTIFACTS") == "1" or sys.exc_info()[0] is not None:
        print("artifacts: " + str(root), file=sys.stderr)
    else:
        shutil.rmtree(root)

#!/usr/bin/env python3
"""Check automatic config reload and selector errors on an isolated tmux server."""

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
import tempfile
import termios
import time


ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("VDE_VT_BIN", ROOT / "target/debug/vt")).resolve()


def main():
    work = Path(tempfile.mkdtemp(prefix="vde-config-reload-"))
    work.chmod(0o700)
    server = work.name
    tmux_bin = shutil.which("tmux")
    assert tmux_bin and BIN.is_file()
    env = {
        **os.environ,
        "HOME": str(work / "home"),
        "ZDOTDIR": str(work / "home"),
        "XDG_CONFIG_HOME": str(work / "config"),
        "XDG_STATE_HOME": str(work / "state"),
        "VDE_TMUX_SOCKET_NAME": server,
        "PATH": f"{work / 'bin'}:{BIN.parent}:{os.environ['PATH']}",
        "FZF_DEFAULT_OPTS": "",
        "RELOAD_OWNER": "alpha",
        "RELOAD_TEAM": "team",
    }
    env.pop("RELOAD_MISSING", None)
    for name in ("TMUX", "TMUX_PANE", "FZF_DEFAULT_OPTS_FILE"):
        env.pop(name, None)
    for name in ("home", "state", "config/vde/tmux", "bin", "repo"):
        (work / name).mkdir(parents=True, mode=0o700)
    config = work / "config/vde/tmux/config.yml"
    (work / "repo/README.md").write_text("# Config reload fixture\n")
    ghq = work / "bin/ghq"
    ghq.write_text(f"#!/bin/sh\nprintf '%s\\n' {shlex.quote(str(work / 'repo'))}\n")
    ghq.chmod(0o700)
    fzf = work / "bin/fzf"
    fzf.write_text("#!/bin/sh\ncat >/dev/null\nexit 130\n")
    fzf.chmod(0o700)

    def tmux(*args, check=True):
        return subprocess.run(
            [tmux_bin, "-L", server, "-f", "/dev/null", *args],
            env=env, capture_output=True, text=True, check=check, timeout=15,
        )

    def vt(*args, check=True):
        result = subprocess.run(
            [str(BIN), *args], env=env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=15,
            start_new_session=True,
        )
        if check and result.returncode:
            raise AssertionError(f"vt {args}: {result.stderr}")
        return result

    def write_config(show_index, patterns=None):
        if patterns is None:
            patterns = ["github.com/${RELOAD_OWNER}/*"]
        config.write_text(
            "statusline:\n  sessions:\n"
            f"    show_index: {'true' if show_index else 'false'}\n"
            "categories:\n  rules:\n    - category: work\n"
            f"      path_patterns: {json.dumps(patterns)}\n"
        )

    def lifecycle():
        paths = list((work / "state").rglob("lifecycle.json"))
        assert len(paths) == 1, paths
        return json.loads(paths[0].read_text())

    def instance():
        status = vt("daemon", "status").stdout
        assert "phase: Serving" in status and "hooks: Healthy" in status, status
        assert "control: Ready" in status, status
        return lifecycle()["process"]["daemon_instance_id"]

    def screen_until(window, expected):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            screen = tmux("capture-pane", "-p", "-t", window).stdout
            if expected in screen:
                return screen
            time.sleep(0.05)
        raise AssertionError(f"screen did not contain {expected!r}: {screen}")

    def selector_window(name):
        exit_file = work / f"{name}.exit"
        command = (
            f"{shlex.quote(str(BIN))} project selector; "
            f"printf '%s\\n' \"$?\" > {shlex.quote(str(exit_file))}; sleep 3600"
        )
        tmux("new-window", "-d", "-t", "test", "-n", name, command)
        return f"test:{name}", exit_file

    def wait_exit(exit_file):
        deadline = time.monotonic() + 10
        while not exit_file.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert exit_file.exists(), exit_file
        return int(exit_file.read_text())

    def popup_test(name, expected, close_key):
        exit_file = work / f"popup-{name}.exit"
        command = (f"{shlex.quote(str(BIN))} project selector; "
                   f"printf '%s\\n' \"$?\" > {shlex.quote(str(exit_file))}")
        tmux("bind-key", "-n", "M-t", "display-popup", "-E", "-d",
             "#{pane_current_path}", command)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 100, 0, 0))

        def terminal():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        client_env = {key: value for key, value in env.items()
                      if key not in ("TMUX", "TMUX_PANE")}
        client_env["TERM"] = "xterm-256color"
        client = subprocess.Popen([tmux_bin, "-L", server, "attach-session", "-t", "test"],
            stdin=slave, stdout=slave, stderr=slave, env=client_env, preexec_fn=terminal)
        os.close(slave)
        output = bytearray()
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                clients = tmux("list-clients", "-F", "#{client_control_mode}").stdout.splitlines()
                if "0" in clients:
                    break
                time.sleep(0.02)
            else:
                raise AssertionError("PTY client did not attach")
            os.write(master, b"\x1bt")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and expected.encode() not in output:
                if select.select([master], [], [], 0.1)[0]:
                    output.extend(os.read(master, 65536))
            assert expected.encode() in output, output.decode(errors="replace")
            assert not exit_file.exists()
            os.write(master, close_key)
            return wait_exit(exit_file)
        finally:
            (work / f"popup-{name}.ansi").write_bytes(output)
            os.close(master)
            client.terminate()
            try:
                client.wait(timeout=5)
            except subprocess.TimeoutExpired:
                client.kill()
                client.wait(timeout=5)

    print(f"Config reload artifacts: {work}", flush=True)
    try:
        write_config(False)
        tmux("new-session", "-d", "-s", "test", "-x", "100", "-y", "35", "sleep 3600")
        tmux("set-option", "-g", "default-shell", "/bin/sh")
        env["TMUX"] = tmux("display-message", "-p", "#{socket_path},#{pid},0").stdout.strip()
        env["TMUX_PANE"] = tmux("display-message", "-p", "#{pane_id}").stdout.strip()
        vt("daemon", "enable")
        initial = instance()
        generation = lifecycle()["generation"]
        storage = json.loads(vt("agent", "storage", "status", "--json").stdout)["result"]["usage"]

        vt("project", "selector")
        assert instance() == initial and lifecycle()["generation"] == generation
        print("PASS: matching config does not restart", flush=True)

        write_config(True)
        processes = [subprocess.Popen(
            [str(BIN), "project", "selector"], env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True,
        ) for _ in range(12)]
        results = [(*p.communicate(timeout=15), p.returncode) for p in processes]
        assert all(result[2] == 0 for result in results), results
        assert instance() != initial
        assert lifecycle()["generation"] == generation + 1, lifecycle()
        after_storage = json.loads(vt("agent", "storage", "status", "--json").stdout)["result"]["usage"]
        assert storage["generation"] == after_storage["generation"]
        for table in ("runs", "operations", "prompts", "artifacts"):
            assert storage[table]["records"] == after_storage[table]["records"]
        print("PASS: 12 concurrent selectors reload once and preserve storage", flush=True)

        current = instance()
        generation = lifecycle()["generation"]
        write_config(True, ["github.com/${RELOAD_OWNER}/${RELOAD_TEAM}/*"])
        vt("project", "selector")
        assert instance() != current and lifecycle()["generation"] == generation + 1
        print("PASS: new reference to an existing matching variable reloads", flush=True)

        current = instance()
        generation = lifecycle()["generation"]
        for variable, value in (("RELOAD_TEAM", "different"), ("RELOAD_MISSING", "present")):
            write_config(True, [f"github.com/${{{variable}}}/*"])
            result = subprocess.run([str(BIN), "project", "selector"],
                env={**env, variable: value}, stdin=subprocess.DEVNULL,
                capture_output=True, text=True, start_new_session=True, timeout=15)
            assert result.returncode and "environment differs" in result.stderr, result
            assert instance() == current and lifecycle()["generation"] == generation
        print("PASS: new references with different or absent daemon values cannot reload", flush=True)

        write_config(True, ["github.com/alpha/*"])
        vt("project", "selector")
        assert instance() != current and lifecycle()["generation"] == generation + 1
        print("PASS: removing environment references reloads", flush=True)
        write_config(True)
        vt("project", "selector")

        healthy = instance()
        generation = lifecycle()["generation"]

        changed_env = {**env, "RELOAD_OWNER": "beta"}
        result = subprocess.run([str(BIN), "project", "selector"], env=changed_env,
                                stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                start_new_session=True, timeout=15)
        assert result.returncode and "environment differs" in result.stderr, result
        assert instance() == healthy and lifecycle()["generation"] == generation
        alternate = work / "bin/vt-alternate"
        shutil.copy2(BIN, alternate)
        write_config(False)
        result = subprocess.run([str(alternate), "project", "selector"], env=env,
                                stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                start_new_session=True, timeout=15)
        assert result.returncode and "executable differs" in result.stderr, result
        assert instance() == healthy and lifecycle()["generation"] == generation
        write_config(True)
        print("PASS: different executable and configuration environment cannot reload", flush=True)

        write_config(False)
        window_id = tmux("display-message", "-p", "#{window_id}").stdout.strip()
        result = vt("sidebar", "prepare-layout", "--window", window_id, "--json", check=False)
        assert result.returncode and not result.stdout, result
        assert instance() == healthy and lifecycle()["generation"] == generation
        print("PASS: hook/JSON config guard does not automatically reload", flush=True)

        # Mark the first tmux query made after the child validated its config.
        # Holding the existing startup lock lets us change the file at this boundary.
        wrapper = work / "bin/tmux"
        marker = work / "validated-before-query"
        wrapper.write_text(
            "#!/bin/sh\n"
            'if [ -n "$VDE_CONFIG_RELOAD_GATE" ]; then touch "$VDE_CONFIG_RELOAD_GATE"; fi\n'
            f"exec {shlex.quote(tmux_bin)} \"$@\"\n"
        )
        wrapper.chmod(0o700)
        socket_path = next(line[8:] for line in vt("daemon", "status").stdout.splitlines()
                           if line.startswith("socket: "))
        with open(socket_path + ".start.lock", "r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            child = subprocess.Popen([str(BIN), "project", "selector"],
                env={**env, "VDE_CONFIG_RELOAD_GATE": str(marker)}, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
            deadline = time.monotonic() + 5
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            assert marker.exists(), "selector did not finish initial config validation"
            config.write_text("daemon:\n  poll_ms: [broken\n")
            fcntl.flock(lock, fcntl.LOCK_UN)
            stdout, stderr = child.communicate(timeout=15)
        wrapper.unlink()
        assert child.returncode and "invalid config" in stderr, (stdout, stderr)
        assert instance() == healthy and lifecycle()["generation"] == generation
        print("PASS: config is validated again after waiting for the reload lock", flush=True)

        config.write_text("daemon:\n  poll_ms: [broken\n")
        for args in (("project", "selector"), ("daemon", "reload")):
            result = vt(*args, check=False)
            assert result.returncode and "config" in result.stderr, result
            assert instance() == healthy and lifecycle()["generation"] == generation
        print("PASS: invalid config leaves daemon unchanged and non-TTY calls exit", flush=True)

        window, exit_file = selector_window("invalid")
        screen_until(window, "Press Enter to close.")
        assert not exit_file.exists()
        tmux("send-keys", "-t", window, "Enter")
        assert wait_exit(exit_file) == 1
        tmux("kill-window", "-t", window)
        print("PASS: interactive selector error stays until Enter", flush=True)
        assert popup_test("invalid", "Press Enter to close.", b"\r") == 1
        print("PASS: M-t popup keeps the error visible until Enter", flush=True)

        write_config(True)
        vt("daemon", "disable")
        before = lifecycle()
        write_config(False)
        result = vt("project", "selector", check=False)
        assert result.returncode and "disabled" in result.stderr, result
        assert lifecycle() == before
        assert "daemon: unavailable" in vt("daemon", "status").stdout
        print("PASS: disabled daemon remains disabled", flush=True)

        vt("daemon", "enable")
        write_config(True)
        selector = subprocess.Popen([str(BIN), "project", "selector"], env=env,
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, start_new_session=True)
        vt("daemon", "disable")
        selector.communicate(timeout=15)
        assert lifecycle()["desired_mode"] == "disabled" and lifecycle()["process"] is None
        print("PASS: concurrent disable and automatic reload finish disabled", flush=True)

        vt("daemon", "enable")
        vt("daemon", "stop")
        before = lifecycle()
        write_config(True)
        result = vt("project", "selector", check=False)
        assert result.returncode and "unavailable" in result.stderr, result
        assert lifecycle() == before
        print("PASS: unavailable daemon is not automatically started", flush=True)

        vt("daemon", "enable")
        healthy = instance()
        fzf.write_text("#!/bin/sh\ncat >/dev/null\nexit 2\n")
        result = vt("project", "selector", check=False)
        assert result.returncode and "fzf" in result.stderr, result
        assert instance() == healthy
        print("PASS: fzf startup errors are reported", flush=True)

        fzf.unlink()
        assert shutil.which("fzf", path=env["PATH"]), "real fzf required"
        write_config(False)
        window, exit_file = selector_window("real-fzf")
        screen = screen_until(window, "Project>")
        (work / "selector-screen.txt").write_text(screen)
        assert not exit_file.exists() and instance() != healthy
        tmux("send-keys", "-t", window, "Escape")
        assert wait_exit(exit_file) == 0
        print("PASS: real fzf opens after automatic reload and Escape cancels", flush=True)
        assert popup_test("real-fzf", "Project>", b"\x1b") == 0
        print("PASS: M-t popup opens real fzf and Escape closes it", flush=True)

    finally:
        # Restore valid config before disabling so cleanup cannot start new daemons.
        write_config(False)
        vt("daemon", "disable", check=False)
        tmux("kill-server", check=False)


if __name__ == "__main__":
    main()

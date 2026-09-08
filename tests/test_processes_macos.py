"""Real Darwin identity checks plus denied-access and PID-reuse regressions."""
import os
import subprocess
import sys
import time
import uuid
from unittest.mock import Mock, patch

import pytest

pytestmark = pytest.mark.skipif(sys.platform != "darwin", reason="native Darwin backend")
if sys.platform == "darwin":
    from pikamux import processes as processes
    from pikamux import processes_macos as mac


def test_native_birth_stamp_and_parent_are_fresh():
    first = processes.process_start_time(os.getpid())
    assert isinstance(first, int) and first > 0
    assert first == processes.process_start_time(os.getpid())
    assert processes.parent_pid(os.getpid()) == os.getppid()
    assert processes.process_state(os.getpid()) in {"R", "S"}
    assert not processes.can_signal_exact_process()


@pytest.mark.parametrize("interpreter", [sys.executable, "/usr/bin/python3"])
def test_native_exact_argv_tree_duplicates_and_exit(tmp_path, interpreter):
    identity = str(uuid.uuid4())
    if not os.path.isfile(interpreter):
        pytest.skip("optional Apple framework interpreter is unavailable")
    # Script identity survives framework Python's argv[0] rewrite. Exercise
    # both the test runtime and Apple's launcher without forging argv[0].
    script = tmp_path / "codex"
    script.write_text("import time; time.sleep(30)\n")
    command = [interpreter, str(script), identity, "two words"]
    children = []
    try:
        for _ in range(2):
            children.append(subprocess.Popen(command))
        deadline = time.monotonic() + 3
        expected = {p.pid for p in children}
        while time.monotonic() < deadline:
            if set(processes.find_processes_with_session_id(identity, "codex")) == expected:
                break
            time.sleep(.01)
        assert set(processes.find_processes_with_session_id(identity, "codex")) == expected
        assert processes.cmdline(children[0].pid)[-1] == "two words"
        assert not processes.find_processes_with_session_id(identity[:12], "codex")
        assert expected <= set(processes.child_pids(os.getpid()))
        assert expected <= set(processes.process_tree(os.getpid()))
        assert processes.parent_pid(children[0].pid) == os.getpid()
        children[0].terminate()
        children[0].wait(timeout=3)
        assert processes.process_start_time(children[0].pid) is None
        assert processes.find_processes_with_session_id(identity, "codex") == [children[1].pid]
    finally:
        for child in children:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=3)


def test_native_observation_rejects_reused_pid_and_other_user():
    process = Mock()
    process.uids.return_value.real = os.getuid()
    process.cmdline.return_value = ["codex", "resume", "exact"]
    process.is_running.return_value = False
    with patch.object(mac.psutil, "Process", return_value=process):
        assert mac.read(123, "cmdline", []) == []
    process.is_running.return_value = True
    process.uids.return_value.real = os.getuid() + 1
    with patch.object(mac.psutil, "Process", return_value=process):
        assert mac.read(123, "cmdline", []) == []


def test_native_denied_observation_never_fabricates_identity():
    for failure in (mac.psutil.AccessDenied(123), mac.psutil.NoSuchProcess(123)):
        with patch.object(mac.psutil, "Process", side_effect=failure):
            assert processes.cmdline(123) == []
            assert processes.process_start_time(123) is None
            assert processes.process_environment(123) == {}
            assert processes.process_tty(123) is None


def test_native_birth_stamp_is_not_cached_between_calls():
    with patch.object(mac, "read", side_effect=[1234.000001, 1234.000002]):
        assert mac.start_time(123) == 1234000001
        assert mac.start_time(123) == 1234000002

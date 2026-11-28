"""Unit tests for scripts/bench_cold_start.py (stdlib unittest).

Covers run_timed only (issue #423): the non-POSIX branch must not read
stdout/stderr twice (drain threads + communicate), which tears output.
The non-POSIX branch is forced on this POSIX host by temporarily removing
os.wait4, the same attribute run_timed probes with hasattr().

Regression tests for issue #422: blocking ``read(1)`` on the child
stdout pipe made ``REPL_TIMEOUT_S`` / ``TOKEN_TIMEOUT_S`` ineffective -
a silent-but-alive child hung the bench forever instead of failing
after the timeout. The fake "zen" binaries below are small Python
scripts, so no real ``zen`` binary is needed.
"""
import contextlib
import os
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench_cold_start as bcs  # noqa: E402

# Large enough to exceed the pipe buffer several times over, so a second
# reader competing for the same pipe is guaranteed a share of the bytes
# instead of racing for a single small write.
OUT_PAYLOAD = b"O" * 200_000
ERR_PAYLOAD = b"E" * 100_000

PRINT_BIG = (
    "import sys;"
    "sys.stdout.write('O' * 200000); sys.stdout.flush();"
    "sys.stderr.write('E' * 100000); sys.stderr.flush()"
)
SLEEP = "import time; time.sleep(30)"

# Sleeps 300 s without printing anything. On unfixed code the REPL /
# first-token reads block in read(1) until this child exits, so a test
# using it must be run under `timeout` - it never returns in time.
SILENT_CHILD = (
    "import os, time\n"
    "open(os.environ['BENCH_TEST_PID_FILE'], 'w').write(str(os.getpid()))\n"
    "time.sleep(300)\n"
)

# Prints the REPL prompt, then waits for the /quit line on stdin.
PROMPT_CHILD = (
    "import sys\n"
    "sys.stdout.write('> ')\n"
    "sys.stdout.flush()\n"
    "sys.stdin.readline()\n"
)

# Prints one byte, then exits 0.
TOKEN_CHILD = (
    "import sys\n"
    "sys.stdout.write('h')\n"
    "sys.stdout.flush()\n"
)

# Exits 1 at once (a zen binary that fails outright).
FAIL_CHILD = "import sys; sys.exit(1)\n"

# Temp-dir prefixes the bench creates (isolated HOME, temp workdir,
# fresh-copy dir). A test snapshots these before the call and asserts
# the set is unchanged afterwards, on both the success and the
# failure path.
TEMP_PREFIXES = ("zen-bench-home-", "zen-bench-cwd-", "zen-bench-cold-")


def _leaked_temp_dirs():
    tmp = Path(tempfile.gettempdir())
    return sorted(
        p.name for p in tmp.iterdir()
        if p.is_dir() and p.name.startswith(TEMP_PREFIXES)
    )

# Exits at once without printing anything (EOF, no prompt).
EXIT_CHILD = "import sys\n"

# Echoes stdin back to stdout.
ECHO_CHILD = "import sys; sys.stdout.write(sys.stdin.read())"


@contextlib.contextmanager
def force_non_posix():
    """Hide os.wait4 so run_timed takes its non-POSIX (communicate) branch."""
    had = hasattr(os, "wait4")
    saved = getattr(os, "wait4", None)
    if had:
        del os.wait4
    try:
        yield
    finally:
        if had:
            os.wait4 = saved


def _make_fake_zen(tmpdir, source):
    """Write source as an executable script, return its path."""
    path = os.path.join(tmpdir, "fake-zen")
    with open(path, "w") as f:
        f.write("#!/usr/bin/env python3\n" + source)
    os.chmod(path, 0o700)
    return path


def _child_is_dead(pid, wait_s=2.0):
    """True once pid no longer exists (polls briefly for the kill to land)."""
    deadline = time.perf_counter() + wait_s
    while time.perf_counter() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        except PermissionError:
            return False
        time.sleep(0.05)
    return False


class RunTimedNonPosix(unittest.TestCase):
    def test_non_posix_returns_complete_output(self):
        """Non-POSIX branch returns the child's full stdout/stderr (#423).

        Fails on the unfixed code: the drain threads steal part of the
        output, so communicate() returns torn out_b/err_b.
        """
        with force_non_posix():
            _, rc, out, err, rss = bcs.run_timed(
                [sys.executable, "-c", PRINT_BIG], dict(os.environ), timeout=30
            )
        self.assertEqual(rc, 0)
        self.assertIsNone(rss)
        self.assertEqual(out, OUT_PAYLOAD)
        self.assertEqual(err, ERR_PAYLOAD)

    def test_non_posix_starts_no_drain_threads(self):
        """Non-POSIX branch creates no drain threads at all (#423).

        Deterministic structural check for the same bug: communicate()
        must be the only reader of the pipes.
        """
        created = []
        real_thread = threading.Thread

        class CountingThread(real_thread):
            def __init__(self, *args, **kwargs):
                created.append((args, kwargs))
                super().__init__(*args, **kwargs)

        with force_non_posix(), mock.patch.object(
            threading, "Thread", CountingThread
        ):
            _, rc, out, err, _ = bcs.run_timed(
                [sys.executable, "-c", PRINT_BIG], dict(os.environ), timeout=30
            )
        self.assertEqual(created, [])
        self.assertEqual(rc, 0)
        self.assertEqual(out, OUT_PAYLOAD)
        self.assertEqual(err, ERR_PAYLOAD)

    def test_non_posix_timeout_still_raises(self):
        """Non-POSIX timeout behaviour is unchanged by the fix."""
        with force_non_posix():
            with self.assertRaisesRegex(RuntimeError, "timeout running"):
                bcs.run_timed(
                    [sys.executable, "-c", SLEEP], dict(os.environ), timeout=1
                )

    def test_non_posix_stdin_data_is_delivered(self):
        """Non-POSIX branch delivers stdin_data to the child (#525).

        Fails on the unfixed code: run_timed writes stdin_data and closes
        stdin itself, then communicate() with no input tries to flush the
        closed stdin and raises ValueError: flush of closed file.
        """
        with force_non_posix():
            _, rc, out, err, rss = bcs.run_timed(
                [sys.executable, "-c", ECHO_CHILD],
                dict(os.environ),
                stdin_data=b"hello\n",
                timeout=30,
            )
        self.assertEqual(rc, 0)
        self.assertIsNone(rss)
        self.assertEqual(out, b"hello\n")
        self.assertEqual(err, b"")


class RunTimedPosix(unittest.TestCase):
    def test_posix_returns_complete_output_and_rss(self):
        """POSIX branch (drain threads + wait4) is unchanged by the fix."""
        if not hasattr(os, "wait4"):
            self.skipTest("no os.wait4 on this platform")
        _, rc, out, err, rss = bcs.run_timed(
            [sys.executable, "-c", PRINT_BIG], dict(os.environ), timeout=30
        )
        self.assertEqual(rc, 0)
        self.assertEqual(out, OUT_PAYLOAD)
        self.assertEqual(err, ERR_PAYLOAD)
        self.assertIsNotNone(rss)
        self.assertGreater(rss, 0)

    def test_posix_stdin_data_is_delivered(self):
        """POSIX branch (write + close stdin, drain threads + wait4)."""
        if not hasattr(os, "wait4"):
            self.skipTest("no os.wait4 on this platform")
        _, rc, out, err, _ = bcs.run_timed(
            [sys.executable, "-c", ECHO_CHILD],
            dict(os.environ),
            stdin_data=b"hello\n",
            timeout=30,
        )
        self.assertEqual(rc, 0)
        self.assertEqual(out, b"hello\n")
        self.assertEqual(err, b"")


class PipeReaderTests(unittest.TestCase):
    """Direct tests for the deadline-aware reader (issue #422)."""

    def test_read_byte_returns_queued_byte(self):
        r, w = os.pipe()
        try:
            reader = bcs._PipeReader(os.fdopen(r, "rb", buffering=0))
            reader.start()
            os.write(w, b"x")
            self.assertEqual(reader.read_byte(timeout=5), b"x")
        finally:
            os.close(w)

    def test_read_byte_times_out_on_silent_pipe(self):
        r, w = os.pipe()
        try:
            reader = bcs._PipeReader(os.fdopen(r, "rb", buffering=0))
            reader.start()
            start = time.perf_counter()
            with self.assertRaises(TimeoutError):
                reader.read_byte(timeout=0.2)
            self.assertLess(time.perf_counter() - start, 5)
        finally:
            os.close(w)

    def test_read_byte_eof_after_drain(self):
        r, w = os.pipe()
        try:
            reader = bcs._PipeReader(os.fdopen(r, "rb", buffering=0))
            reader.start()
            os.write(w, b"y")
            os.close(w)
            w = None
            self.assertEqual(reader.read_byte(timeout=5), b"y")
            self.assertEqual(reader.read_byte(timeout=5), b"")
        finally:
            if w is not None:
                os.close(w)


class SilentChildTests(unittest.TestCase):
    """A silent-but-alive child must fail fast, not hang the bench."""

    def test_silent_child_repl_times_out(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, SILENT_CHILD)
            pid_file = os.path.join(d, "child.pid")
            with mock.patch.dict(os.environ, {"BENCH_TEST_PID_FILE": pid_file}), mock.patch.object(
                bcs, "REPL_TIMEOUT_S", 1
            ):
                start = time.perf_counter()
                with self.assertRaisesRegex(
                    RuntimeError, "REPL prompt not seen within timeout"
                ):
                    bcs.measure_repl_prompt(zen, 1)
                elapsed = time.perf_counter() - start
            # Fixed code fails after ~1 s; unfixed code blocks until the
            # child exits (300 s), so this bound is the regression tripwire.
            self.assertLess(elapsed, 20)
            with open(pid_file) as f:
                pid = int(f.read())
            self.assertTrue(
                _child_is_dead(pid), "silent child was not killed on timeout"
            )

    def test_silent_child_first_token_times_out(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, SILENT_CHILD)
            pid_file = os.path.join(d, "child.pid")
            with mock.patch.dict(os.environ, {"BENCH_TEST_PID_FILE": pid_file}), mock.patch.object(
                bcs, "TOKEN_TIMEOUT_S", 1
            ):
                start = time.perf_counter()
                with self.assertRaisesRegex(RuntimeError, "timed out"):
                    bcs.measure_first_token(zen, 1)
                elapsed = time.perf_counter() - start
            self.assertLess(elapsed, 20)
            with open(pid_file) as f:
                pid = int(f.read())
            self.assertTrue(
                _child_is_dead(pid), "silent child was not killed on timeout"
            )


class PromptChildTests(unittest.TestCase):
    """The happy paths and the EOF path must keep working after the fix."""

    def test_repl_prompt_seen(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, PROMPT_CHILD)
            samples = bcs.measure_repl_prompt(zen, 1)
            self.assertEqual(len(samples), 1)
            self.assertGreater(samples[0], 0)

    def test_repl_child_exits_without_prompt(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, EXIT_CHILD)
            with self.assertRaisesRegex(
                RuntimeError, "REPL exited without printing a prompt"
            ):
                bcs.measure_repl_prompt(zen, 1)

    def test_first_token_seen(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, TOKEN_CHILD)
            with mock.patch.object(bcs, "TOKEN_TIMEOUT_S", 10):
                samples, _reqs = bcs.measure_first_token(zen, 1)
            self.assertEqual(len(samples), 1)
            self.assertGreater(samples[0], 0)


class TempDirCleanupTests(unittest.TestCase):
    """No temp dir may leak, on the failure path or the success path (#431)."""

    def setUp(self):
        self.before = _leaked_temp_dirs()

    def assert_no_leak(self):
        self.assertEqual(_leaked_temp_dirs(), self.before)

    def test_repl_prompt_failure_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, EXIT_CHILD)
            with self.assertRaises(RuntimeError):
                bcs.measure_repl_prompt(zen, 1)
        self.assert_no_leak()

    def test_repl_prompt_success_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, PROMPT_CHILD)
            self.assertEqual(len(bcs.measure_repl_prompt(zen, 1)), 1)
        self.assert_no_leak()

    def test_first_token_failure_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, EXIT_CHILD)
            with self.assertRaises(RuntimeError):
                bcs.measure_first_token(zen, 1)
        self.assert_no_leak()

    def test_first_token_success_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, TOKEN_CHILD)
            with mock.patch.object(bcs, "TOKEN_TIMEOUT_S", 10):
                self.assertEqual(len(bcs.measure_first_token(zen, 1)[0]), 1)
        self.assert_no_leak()

    def test_cold_startup_failure_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, FAIL_CHILD)
            with self.assertRaises(AssertionError):
                bcs.measure_cold_startup(zen, 1)
        self.assert_no_leak()

    def test_cold_startup_success_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, TOKEN_CHILD)
            self.assertEqual(len(bcs.measure_cold_startup(zen, 1)), 1)
        self.assert_no_leak()

    def test_warm_failure_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, FAIL_CHILD)
            with self.assertRaises(AssertionError):
                bcs.measure_warm(zen, ["--version"], 1)
        self.assert_no_leak()

    def test_warm_success_cleans_up(self):
        with tempfile.TemporaryDirectory() as d:
            zen = _make_fake_zen(d, EXIT_CHILD)
            self.assertEqual(len(bcs.measure_warm(zen, ["--version"], 1)[0]), 1)
        self.assert_no_leak()


if __name__ == "__main__":
    unittest.main()

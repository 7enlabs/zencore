"""Unit tests for scripts/bench_cold_start.py (stdlib unittest).

Covers run_timed only (issue #423): the non-POSIX branch must not read
stdout/stderr twice (drain threads + communicate), which tears output.
The non-POSIX branch is forced on this POSIX host by temporarily removing
os.wait4, the same attribute run_timed probes with hasattr().
"""
import contextlib
import os
import sys
import threading
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


if __name__ == "__main__":
    unittest.main()

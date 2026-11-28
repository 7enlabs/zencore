#!/usr/bin/env python3
"""Unit tests for ztask.py (stdlib unittest, no network, temp SQLite files).

Acceptance tests for Task 10 (2026-10-30) are the three `test_a_`, `test_b_`,
`test_c_` cases in AcceptanceTests. Section 18 alignment tests are in
Section18Tests.
"""

import hashlib
import io
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import ztask  # noqa: E402
from ztask import (  # noqa: E402
    AhoCorasick, IdempotencyConflict, InvalidTransition, NotAllowed,
    NotFound, SecretDetected, Store, TamperEvident, ZtaskError,
)

LEADER = "ZC Leader"
DEV = "ZC Developer"
DEV2 = "ZC Platform"
QA = "ZC QA"
T0 = datetime(2026, 10, 28, 9, 0, 0, tzinfo=timezone.utc)

BLOCK = dict(blocked_by="waiting on Z Ops checklist",
             tried="pinged Z Ops in 7EN Labs, re-read checklist page",
             needs="Z Ops",
             impact="checkpoint 2026-10-28 slips by 1 day if not cleared")


# Evidence A fixture: one temp git repo with a committed artifact, shared by
# all tests (setUpModule). EV is valid evidence for it.
REPO = None
HEAD = None
EV = None
ARTIFACT = b"Ran 25 tests in 0.5s\n\nOK\n"


def git(*argv, cwd=None):
    return subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t",
         "-c", "commit.gpgsign=false", *argv], cwd=cwd or REPO,
        capture_output=True, text=True, check=True).stdout.strip()


def evidence(**over):
    doc = {"cmd": "python3 -m unittest -v", "exit": 0, "commit": HEAD,
           "artifacts": [{"path": "out.txt",
                          "sha256": hashlib.sha256(ARTIFACT).hexdigest()}]}
    doc.update(over)
    return json.dumps(doc)


def setUpModule():
    global REPO, EV, HEAD
    REPO = tempfile.mkdtemp(prefix="ztask-ev-")
    git("init", "-q", cwd=REPO)
    Path(REPO, "out.txt").write_bytes(ARTIFACT)
    git("add", "out.txt")
    git("commit", "-q", "-m", "artifact")
    HEAD = git("rev-parse", "HEAD")
    EV = evidence()


def tearDownModule():
    shutil.rmtree(REPO, ignore_errors=True)


class Clock:
    def __init__(self, start=T0):
        self.t = start

    def __call__(self):
        return self.t

    def advance(self, **kw):
        self.t += timedelta(**kw)


class Base(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "ztask.db")
        self.clock = Clock()
        self.store = Store(self.path, now=self.clock, evidence_base=REPO)
        self.store.init([LEADER])

    def tearDown(self):
        self.store.close()
        self.dir.cleanup()

    def new_task(self, title="Write DESIGN.md", **kw):
        ev = self.store.create(title=title, goal="ztask design for Task 10",
                               owner=LEADER,
                               acceptance="Leader reviews DESIGN.md", **kw)
        return ev["task_id"]

    def claim_start(self, tid, who=DEV):
        tok = self.store.claim(tid, who)["data"]["token"]
        self.store.start(tid, who, tok)
        return tok

    def to_done(self, tid, who=DEV, verifier=QA):
        tok = self.claim_start(tid, who)
        self.store.submit(tid, who, tok, evidence=EV)
        self.store.attest(tid, verifier, command="python3 -m unittest",
                          receipt="Ran 1 test, OK")
        self.store.approve(tid, LEADER)

    def raw(self):
        conn = sqlite3.connect(self.path, isolation_level=None)
        return conn


class AcceptanceTests(Base):
    def test_a_idempotent_event_log(self):
        tid = self.new_task(idem_key="create:task10:design")
        again = self.store.create(title="Write DESIGN.md",
                                  goal="ztask design for Task 10",
                                  owner=LEADER,
                                  acceptance="Leader reviews DESIGN.md",
                                  idem_key="create:task10:design")
        self.assertTrue(again.get("replayed"))
        self.assertEqual(again["task_id"], tid)

        first = self.store.claim(tid, DEV, idem_key="claim:T1:dev:1")
        retry = self.store.claim(tid, DEV, idem_key="claim:T1:dev:1")
        self.assertEqual(first["seq"], retry["seq"])
        self.assertTrue(retry.get("replayed"))

        # Exactly one created + one claimed event, one task.
        types = [e["type"] for e in self.store.events()]
        self.assertEqual(types, ["task.created", "task.claimed"])
        self.assertEqual(len(self.store.tasks()), 1)
        self.assertEqual(self.store.task(tid)["idem_key"], "claim:T1:dev:1")

        # Same key, different request: refused, nothing appended.
        with self.assertRaises(IdempotencyConflict):
            self.store.claim(tid, DEV2, idem_key="claim:T1:dev:1")
        self.assertEqual(len(self.store.events()), 2)
        self.store.verify()

    def test_b_lease_12h_expiry_returns_to_queue(self):
        tid = self.new_task()
        ev = self.store.claim(tid, DEV)
        token = ev["data"]["token"]
        self.assertEqual(ev["data"]["lease_until"], "2026-10-28T21:00:00Z")

        self.clock.advance(hours=11, minutes=59)
        self.store.sweep()
        self.assertEqual(self.store.task(tid)["state"], "CLAIMED")
        self.store.heartbeat(tid, DEV, token)  # extends to now + 12h
        self.assertEqual(self.store.task(tid)["lease_until"],
                         "2026-10-29T08:59:00Z")

        self.clock.advance(hours=12, minutes=1)  # past the extended lease
        self.store.sweep()
        self.assertEqual(self.store.task(tid)["state"], "QUEUED")
        # Another agent claims the re-queued task.
        ev2 = self.store.claim(tid, DEV2)
        types = [e["type"] for e in self.store.events()]
        self.assertIn("task.lease_expired", types)
        self.assertLess(types.index("task.lease_expired"),
                        types.index("task.claimed", 2))
        task = self.store.task(tid)
        self.assertEqual(task["assignee"], DEV2)
        self.assertEqual(task["state"], "CLAIMED")
        self.assertEqual(ev2["data"]["token"], token + 1)

        # The old holder comes back with its stale fencing token.
        with self.assertRaises(NotAllowed):
            self.store.heartbeat(tid, DEV, token)
        with self.assertRaises(NotAllowed):
            self.store.submit(tid, DEV, token, evidence="late")
        self.store.verify()

    def test_c_verify_detects_hand_edit_TamperEvident(self):
        tid = self.new_task()
        self.store.claim(tid, DEV)
        self.store.create(title="Child", goal="g", owner=LEADER,
                          acceptance="a", goal_ref=tid)
        self.assertTrue(self.store.verify()["ok"])

        # Hand-edit event 2 into different, still valid JSON.
        conn = self.raw()
        body = conn.execute("SELECT body FROM events WHERE seq=2").fetchone()[0]
        edited = json.loads(body)
        edited["data"]["assignee"] = "Mallory"
        conn.execute("UPDATE events SET body=? WHERE seq=2",
                     (ztask.canonical(edited),))
        conn.close()
        with self.assertRaises(TamperEvident) as cm:
            self.store.verify()
        self.assertEqual(cm.exception.seq, 2)
        self.assertIn("hash mismatch", cm.exception.reason)

        # CLI reports it with exit code 2.
        err = io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            rc = ztask.main(["--db", self.path, "verify"])
        self.assertEqual(rc, 2)
        self.assertIn("tamper_evident", err.getvalue())

        # Restore, then remove a middle row: seq gap.
        conn = self.raw()
        conn.execute("UPDATE events SET body=? WHERE seq=2", (body,))
        self.store.verify()
        conn.execute("DELETE FROM events WHERE seq=2")
        conn.close()
        with self.assertRaises(TamperEvident) as cm:
            self.store.verify()
        self.assertEqual(cm.exception.seq, 2)

    def test_c2_tail_truncation_detected(self):
        self.new_task()
        self.new_task(title="Second")
        conn = self.raw()
        conn.execute("DELETE FROM events WHERE seq=2")
        conn.close()
        with self.assertRaises(TamperEvident) as cm:
            self.store.verify()
        self.assertIn("chain head mismatch", cm.exception.reason)

    def append_orphan(self, etype, tid, data, idem_key):
        """Insert a hash-chain-valid orphan event (no task.created for
        tid) straight into the log, as a truncated/hand-edited log would
        hold. Returns its seq."""
        conn = self.raw()
        head = conn.execute(
            "SELECT length, hash FROM chain_head WHERE id=1").fetchone()
        seq = head[0] + 1
        ev = {"v": ztask.EVENT_VERSION, "type": etype, "task_id": tid,
              "actor": DEV, "ts": ztask.fmt_time(self.clock()),
              "idem_key": idem_key, "fingerprint": ztask.sha256_hex(idem_key),
              "data": data}
        body = ztask.canonical(ev)
        h = ztask.row_hash(head[1], seq, body)
        conn.execute(
            "INSERT INTO events(seq, idem_key, fingerprint, task_id, body, "
            "prev_hash, hash) VALUES(?, ?, ?, ?, ?, ?, ?)",
            (seq, idem_key, ztask.sha256_hex(idem_key), tid, body, head[1],
             h))
        conn.execute("UPDATE chain_head SET length=?, hash=? WHERE id=1",
                     (seq, h))
        conn.close()
        return seq

    def test_orphan_event_raises_TamperEvident_not_KeyError(self):
        # task.claimed with no prior task.created, chain still valid.
        seq = self.append_orphan(
            "task.claimed", "T99",
            {"assignee": DEV, "token": 1,
             "lease_until": ztask.fmt_time(self.clock())},
            "orphan:claimed:T99")
        with self.assertRaises(TamperEvident) as cm:
            self.store.verify()
        self.assertEqual(cm.exception.seq, seq)
        self.assertIn("T99", cm.exception.reason)
        with self.assertRaises(TamperEvident) as cm:
            self.store.rebuild()
        self.assertEqual(cm.exception.seq, seq)
        # CLI reports it as the structured JSON error, not a traceback.
        err = io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            rc = ztask.main(["--db", self.path, "verify"])
        self.assertEqual(rc, 2)
        self.assertIn("tamper_evident", err.getvalue())
        self.assertNotIn("Traceback", err.getvalue())

    def test_orphan_event_apply_event_direct(self):
        # A second orphan type, task.verified, at the apply_event level.
        ev = {"v": ztask.EVENT_VERSION, "type": "task.verified",
              "task_id": "T98", "actor": QA,
              "ts": ztask.fmt_time(self.clock()), "idem_key": "orphan:v:T98",
              "fingerprint": "x",
              "data": {"verified_by": QA, "command": "python3 -m unittest",
                       "receipt": "Ran 1 test, OK"}}
        with self.assertRaises(TamperEvident) as cm:
            ztask.apply_event({}, ev)
        self.assertIn("T98", cm.exception.reason)


class StateMachineTests(Base):
    def test_happy_path_and_done_only_by_leader(self):
        tid = self.new_task()
        self.assertEqual(self.store.task(tid)["state"], "QUEUED")
        tok = self.store.claim(tid, DEV)["data"]["token"]
        self.assertEqual(self.store.task(tid)["state"], "CLAIMED")
        with self.assertRaises(InvalidTransition):
            self.store.submit(tid, DEV, tok, evidence="e")  # CLAIMED -> REVIEW
        self.store.start(tid, DEV, tok)
        self.assertEqual(self.store.task(tid)["state"], "DOING")
        self.store.block(tid, DEV, tok, **BLOCK)
        self.assertEqual(self.store.task(tid)["state"], "BLOCKED")
        tok = self.store.unblock(tid, DEV)["data"]["token"]
        self.assertEqual(self.store.task(tid)["state"], "DOING")
        with self.assertRaises(ZtaskError):
            self.store.submit(tid, DEV, tok, evidence="  ")
        self.store.submit(tid, DEV, tok, evidence="test output + diff stat")
        self.assertEqual(self.store.task(tid)["state"], "REVIEW")
        self.store.attest(tid, QA, command="python3 -m unittest -v",
                          receipt="Ran 24 tests OK")
        with self.assertRaises(NotAllowed):
            self.store.approve(tid, DEV)
        tok = self.store.reject(tid, LEADER, reason="add tests")["data"]["token"]
        task = self.store.task(tid)
        self.assertEqual(task["state"], "DOING")
        self.assertIsNone(task["verified_by"])  # rework voids verification
        self.store.submit(tid, DEV, tok, evidence=EV)
        with self.assertRaises(InvalidTransition):
            self.store.approve(tid, LEADER)  # REVIEW but not verified
        self.store.attest(tid, QA, command="python3 -m unittest -v",
                          receipt="Ran 25 tests OK")
        self.store.approve(tid, LEADER)
        self.assertEqual(self.store.task(tid)["state"], "DONE")
        with self.assertRaises(InvalidTransition):
            self.store.claim(tid, DEV2)
        self.store.verify()

    def test_illegal_transitions(self):
        tid = self.new_task()
        with self.assertRaises(InvalidTransition):
            self.store.approve(tid, LEADER)          # QUEUED -> DONE
        with self.assertRaises(InvalidTransition):
            self.store.submit(tid, DEV, 1, evidence="x")  # QUEUED -> REVIEW
        with self.assertRaises(InvalidTransition):
            self.store.unblock(tid, LEADER)          # QUEUED -> DOING
        with self.assertRaises(InvalidTransition):
            self.store.start(tid, DEV, 1)            # QUEUED -> DOING
        with self.assertRaises(InvalidTransition):
            self.store.attest(tid, QA, command="c", receipt="r")
        tok = self.store.claim(tid, DEV)["data"]["token"]
        with self.assertRaises(InvalidTransition):
            self.store.claim(tid, DEV2)              # double claim
        with self.assertRaises(NotAllowed):
            self.store.start(tid, DEV2, tok)         # not the holder
        self.store.start(tid, DEV, tok)
        with self.assertRaises(InvalidTransition):
            self.store.start(tid, DEV, tok)          # DOING -> DOING

    def test_task_record_requires_four_elements(self):
        with self.assertRaises(ZtaskError):
            self.store.create(title="x", goal="", owner=LEADER,
                              acceptance="a")
        with self.assertRaises(ZtaskError):
            self.store.create(title="x", goal="g", owner=LEADER,
                              acceptance=" ")

    def test_goal_ref_child_done_before_parent(self):
        parent = self.new_task(title="Parent")
        child = self.new_task(title="Child", goal_ref=parent)
        with self.assertRaises(ztask.NotFound):
            self.new_task(title="Orphan", goal_ref="T99")
        ptok = self.claim_start(parent, DEV)
        self.store.submit(parent, DEV, ptok, evidence=EV)
        self.store.attest(parent, QA, command="c", receipt="r")
        with self.assertRaises(InvalidTransition):
            self.store.approve(parent, LEADER)
        self.to_done(child, DEV2)
        self.store.approve(parent, LEADER)
        self.assertEqual(self.store.task(parent)["state"], "DONE")

    def test_concurrent_claim_has_one_winner(self):
        tid = self.new_task()
        results = []

        def worker(name):
            s = Store(self.path, now=self.clock)
            try:
                s.claim(tid, name)
                results.append(("ok", name))
            except InvalidTransition:
                results.append(("lost", name))
            finally:
                s.close()

        threads = [threading.Thread(target=worker, args=(f"agent-{i}",))
                   for i in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(sum(1 for r in results if r[0] == "ok"), 1)
        self.assertEqual(len(results), 8)
        self.store.verify()


class Section18Tests(Base):
    def test_seven_states_declared(self):
        self.assertEqual(ztask.STATES, ("QUEUED", "CLAIMED", "DOING",
                                        "BLOCKED", "REVIEW", "DONE",
                                        "DROPPED"))

    def test_dropped_path(self):
        a = self.new_task(title="A")
        with self.assertRaises(ZtaskError):
            self.store.drop(a, LEADER, reason=" ")        # reason required
        with self.assertRaises(NotAllowed):
            self.store.drop(a, DEV, reason="not needed")  # leader only
        self.store.drop(a, LEADER, reason="superseded by Task 11")
        task = self.store.task(a)
        self.assertEqual(task["state"], "DROPPED")
        self.assertEqual(task["drop_reason"], "superseded by Task 11")
        with self.assertRaises(InvalidTransition):
            self.store.claim(a, DEV)                      # terminal
        with self.assertRaises(InvalidTransition):
            self.store.drop(a, LEADER, reason="again")

        # Drop from DOING clears the lease; the old holder is fenced out.
        b = self.new_task(title="B")
        tok = self.claim_start(b, DEV)
        self.store.drop(b, LEADER, reason="scope cut")
        self.assertIsNone(self.store.task(b)["lease_until"])
        with self.assertRaises(InvalidTransition):
            self.store.submit(b, DEV, tok, evidence="late")

        # DONE is terminal too.
        c = self.new_task(title="C")
        self.to_done(c)
        with self.assertRaises(InvalidTransition):
            self.store.drop(c, LEADER, reason="x")

        # A dropped child does not hold its parent open; a parent with a
        # live child cannot be dropped.
        parent = self.new_task(title="Parent")
        child = self.new_task(title="Child", goal_ref=parent)
        with self.assertRaises(InvalidTransition):
            self.store.drop(parent, LEADER, reason="x")
        self.store.drop(child, LEADER, reason="not needed")
        self.to_done(parent)
        self.assertEqual(self.store.task(parent)["state"], "DONE")
        # Nothing was deleted: every task is still in the log and table.
        self.assertEqual(len(self.store.tasks()), 5)
        self.store.verify()

    def test_blocked_requires_four_fields(self):
        tid = self.new_task()
        tok = self.claim_start(tid)
        n = len(self.store.events())
        for missing in ("blocked_by", "tried", "needs", "impact"):
            fields = dict(BLOCK)
            fields[missing] = "  "
            with self.assertRaises(ZtaskError) as cm:
                self.store.block(tid, DEV, tok, **fields)
            self.assertIn("4 fields", str(cm.exception))
        self.assertEqual(len(self.store.events()), n)
        self.assertEqual(self.store.task(tid)["state"], "DOING")

        self.store.block(tid, DEV, tok, **BLOCK)
        task = self.store.task(tid)
        self.assertEqual(task["state"], "BLOCKED")
        self.assertEqual(task["blocked_by"], BLOCK["blocked_by"])
        self.assertEqual(task["blocked_tried"], BLOCK["tried"])
        self.assertEqual(task["blocked_needs"], BLOCK["needs"])
        self.assertEqual(task["blocked_impact"], BLOCK["impact"])
        self.store.unblock(tid, LEADER)
        task = self.store.task(tid)
        self.assertEqual(task["state"], "DOING")
        self.assertIsNone(task["blocked_by"])
        self.store.verify()

    def test_self_verify_rejected(self):
        tid = self.new_task()
        tok = self.claim_start(tid, DEV)
        self.store.submit(tid, DEV, tok, evidence=EV)
        with self.assertRaises(NotAllowed) as cm:
            self.store.attest(tid, DEV, command="python3 -m unittest",
                              receipt="OK")
        self.assertIn("no self-verify", str(cm.exception))
        with self.assertRaises(ZtaskError):
            self.store.attest(tid, QA, command="python3 -m unittest",
                              receipt="")                  # triple incomplete
        with self.assertRaises(InvalidTransition):
            self.store.approve(tid, LEADER)                # REVIEW != DONE
        self.store.attest(tid, QA, command="python3 -m unittest",
                          receipt="Ran 25 tests in 1.2s OK")
        task = self.store.task(tid)
        self.assertEqual((task["verified_by"], task["verification_cmd"],
                          task["verification_receipt"]),
                         (QA, "python3 -m unittest",
                          "Ran 25 tests in 1.2s OK"))
        self.store.approve(tid, LEADER)
        self.assertEqual(self.store.task(tid)["state"], "DONE")
        self.store.verify()

    def test_dead_letter_after_three_requeues_and_recover(self):
        tid = self.new_task(lease="30m")
        agents = [DEV, DEV2, DEV]
        for i, who in enumerate(agents, start=1):
            tok = self.claim_start(tid, who)
            if i == 1:
                self.store.heartbeat(tid, who, tok,
                                     checkpoint="step 2/5 done, branch x")
            self.clock.advance(minutes=31)
            self.store.sweep()
            task = self.store.task(tid)
            self.assertEqual(task["state"], "QUEUED")
            self.assertEqual(task["requeue_count"], i)
            self.assertEqual(task["dead_letter"], 1 if i == 3 else 0)
        types = [e["type"] for e in self.store.events()]
        self.assertEqual(types.count("task.lease_expired"), 3)
        self.assertEqual(types.count("task.dead_lettered"), 1)
        self.assertEqual(types[-1], "task.dead_lettered")
        self.assertIn(LEADER, self.store.outbox()[-1]["recipient"])

        # Checkpoint survives the re-queues; the 2nd claim saw resume_from.
        self.assertEqual(self.store.task(tid)["checkpoint"],
                         "step 2/5 done, branch x")
        claims = [e for e in self.store.events() if e["type"] == "task.claimed"]
        self.assertNotIn("resume_from", claims[0]["data"])
        self.assertEqual(claims[1]["data"]["resume_from"],
                         "step 2/5 done, branch x")

        with self.assertRaises(NotAllowed):
            self.store.claim(tid, DEV2)                    # dead-lettered
        with self.assertRaises(NotAllowed):
            self.store.recover(tid, DEV2, assignee=DEV2)   # leader only
        ev = self.store.recover(tid, LEADER, assignee=DEV2)
        self.assertEqual(ev["data"]["resume_from"], "step 2/5 done, branch x")
        task = self.store.task(tid)
        self.assertEqual((task["state"], task["assignee"], task["dead_letter"],
                          task["requeue_count"]),
                         ("CLAIMED", DEV2, 0, 0))
        self.store.start(tid, DEV2, ev["data"]["token"])
        with self.assertRaises(InvalidTransition):
            self.store.recover(tid, LEADER)                # not dead-lettered
        self.store.verify()

    def test_release_counts_toward_dead_letter(self):
        tid = self.new_task()
        for _ in range(3):
            tok = self.store.claim(tid, DEV)["data"]["token"]
            self.store.release(tid, DEV, tok)
        self.assertEqual(self.store.task(tid)["dead_letter"], 1)
        self.store.recover(tid, LEADER)   # back to QUEUED, claimable again
        self.store.claim(tid, DEV2)
        self.assertEqual(self.store.task(tid)["assignee"], DEV2)

    def test_configurable_lease(self):
        default = self.new_task(title="default")
        self.assertEqual(self.store.task(default)["lease_seconds"], 12 * 3600)
        small = self.new_task(title="buffer", lease="buffer-s")
        shift = self.new_task(title="main", lease="shift")
        custom = self.new_task(title="custom", lease="45m")
        self.assertEqual(self.store.task(small)["lease_seconds"], 1800)
        self.assertEqual(self.store.task(shift)["lease_seconds"], 8 * 3600)
        self.assertEqual(self.store.task(custom)["lease_seconds"], 2700)
        for bad in ("soon", "0m", "30d"):
            with self.assertRaises(ZtaskError):
                self.new_task(title="bad", lease=bad)

        ev = self.store.claim(small, DEV)
        self.assertEqual(ev["data"]["lease_until"], "2026-10-28T09:30:00Z")
        ev = self.store.claim(shift, DEV2)
        self.assertEqual(ev["data"]["lease_until"], "2026-10-28T17:00:00Z")
        self.clock.advance(minutes=29)
        self.store.sweep()
        self.assertEqual(self.store.task(small)["state"], "CLAIMED")
        self.clock.advance(minutes=2)
        self.store.sweep()
        self.assertEqual(self.store.task(small)["state"], "QUEUED")
        self.assertEqual(self.store.task(shift)["state"], "CLAIMED")
        self.store.verify()

    def test_task_card_fields_and_level(self):
        tid = self.new_task(level="l3")
        task = self.store.task(tid)
        for f in ("state", "owner", "assignee", "evidence", "verified_by",
                  "verification_cmd", "verification_receipt", "blocked_by",
                  "lease_until", "heartbeat_at", "checkpoint", "lease_token",
                  "idem_key", "level"):
            self.assertIn(f, task)
        self.assertEqual(task["level"], "L3")
        self.assertEqual(self.store.task(self.new_task(title="d"))["level"],
                         "L2")
        with self.assertRaises(ZtaskError):
            self.new_task(title="bad", level="L5")
        tok = self.store.claim(tid, DEV)["data"]["token"]
        task = self.store.task(tid)
        self.assertEqual(task["owner"], LEADER)      # owner != executor
        self.assertEqual(task["assignee"], DEV)
        self.assertEqual(task["lease_token"], tok)   # fencing token


class SafetyAndOpsTests(Base):
    def test_secret_precheck_blocks_write(self):
        tid = self.new_task()
        tok = self.claim_start(tid)
        n = len(self.store.events())
        with self.assertRaises(SecretDetected) as cm:
            self.store.submit(tid, DEV, tok,
                              evidence="token ghp_abcdefghijklmnop1234")
        self.assertNotIn("abcdefghijklmnop", str(cm.exception))
        with self.assertRaises(SecretDetected):
            self.store.heartbeat(tid, DEV, tok,
                                 checkpoint="key sk-ant-abc123")
        self.assertEqual(len(self.store.events()), n)
        # "task-1" must not trip the scanner.
        self.store.submit(tid, DEV, tok, evidence="see task-1 and sk-learn")

    def test_aho_corasick(self):
        ac = AhoCorasick(["he", "she", "his", "hers"])
        self.assertEqual(sorted(ac.find("ushers")), ["he", "hers", "she"])

    def test_instant_notification_and_outbox(self):
        seen = []
        self.store.listeners.append(lambda ev, who: seen.append((ev["type"],
                                                                 who)))
        tid = self.new_task()
        tok = self.claim_start(tid)
        self.store.submit(tid, DEV, tok, evidence="e")
        self.assertEqual([s[0] for s in seen],
                         ["task.created", "task.claimed", "task.started",
                          "task.submitted"])
        self.assertIn(LEADER, seen[-1][1])
        pending = self.store.outbox()
        self.assertTrue(pending)
        self.store.ack(pending[0]["id"])
        self.assertEqual(len(self.store.outbox()), len(pending) - 1)

    def test_outbox_pending_only_false_lists_delivered(self):
        tid = self.new_task()
        tok = self.claim_start(tid)
        self.store.submit(tid, DEV, tok, evidence="e")
        pending = self.store.outbox()
        self.assertTrue(pending)
        self.store.ack(pending[0]["id"])
        self.assertEqual(len(self.store.outbox()), len(pending) - 1)
        self.assertEqual(len(self.store.outbox(pending_only=False)),
                         len(pending))

    def test_ack_nonexistent_id_raises(self):
        with self.assertRaises(NotFound):
            self.store.ack(9999)

    def test_ack_already_delivered_raises(self):
        tid = self.new_task()
        tok = self.claim_start(tid)
        self.store.submit(tid, DEV, tok, evidence="e")
        pending = self.store.outbox()
        self.assertTrue(pending)
        self.store.ack(pending[0]["id"])
        with self.assertRaises(NotFound):
            self.store.ack(pending[0]["id"])

    def test_cli_ack_nonexistent_id_errors(self):
        def run(*argv):
            out = io.StringIO()
            err = io.StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                rc = ztask.main(["--db", self.path, *argv])
            return rc, out.getvalue(), err.getvalue()

        rc, _out, err = run("ack", "9999")
        self.assertEqual(rc, 1)
        self.assertEqual(json.loads(err)["error"], "not_found")

    def test_deadline_and_aging_escalation(self):
        tid = self.new_task(deadline="2026-10-28T12:00:00Z")
        self.clock.advance(hours=4)
        self.assertEqual(self.store.sweep(), 1)          # deadline
        self.clock.advance(hours=21)
        self.assertEqual(self.store.sweep(), 1)          # aging (QUEUED > 24h)
        self.assertEqual(self.store.sweep(), 0)          # once per reason
        self.assertEqual(self.store.task(tid)["escalation"], "deadline,aging")

    def test_rebuild_matches_projection_and_detects_drift(self):
        tid = self.new_task()
        self.store.claim(tid, DEV)
        before = self.store.tasks()
        conn = self.raw()
        conn.execute("UPDATE tasks SET state='DONE' WHERE id=?", (tid,))
        conn.close()
        with self.assertRaises(ZtaskError):
            self.store.verify()
        self.assertEqual(self.store.rebuild(), 1)
        self.assertEqual(self.store.tasks(), before)
        self.store.verify()

    def test_backup_vacuum_into_verifies(self):
        self.new_task()
        dest = os.path.join(self.dir.name, "backup.db")
        self.store.backup(dest)
        copy = Store(dest)
        try:
            self.assertEqual(copy.verify()["events"], 1)
        finally:
            copy.close()
        with self.assertRaises(ZtaskError):
            self.store.backup(dest)

    def test_cli_roundtrip(self):
        def run(*argv):
            out = io.StringIO()
            with redirect_stdout(out), redirect_stderr(io.StringIO()):
                rc = ztask.main(["--db", self.path, *argv])
            return rc, out.getvalue()

        rc, out = run("create", "--title", "CLI", "--goal", "g", "--owner",
                      LEADER, "--acceptance", "a", "--lease", "buffer-s",
                      "--level", "L1")
        self.assertEqual(rc, 0)
        self.assertEqual(json.loads(out)["type"], "task.created")
        self.assertEqual(run("claim", "T1", "--actor", DEV)[0], 0)
        self.assertEqual(run("start", "T1", "--actor", DEV, "--token",
                             "1")[0], 0)
        # Missing --tried etc. is an argparse error (exit 2 via SystemExit).
        with self.assertRaises(SystemExit):
            run("block", "T1", "--actor", DEV, "--token", "1", "--by", "x")
        self.assertEqual(run("submit", "T1", "--actor", DEV, "--token", "1",
                             "--evidence", EV, "--base", REPO)[0], 0)
        self.assertEqual(run("attest", "T1", "--actor", DEV, "--cmd", "c",
                             "--receipt", "r")[0], 1)       # self-verify
        self.assertEqual(run("attest", "T1", "--actor", QA, "--cmd", "c",
                             "--receipt", "r")[0], 0)
        self.assertEqual(run("approve", "T1", "--actor", LEADER)[0], 1)
        # cwd is not the evidence repo: artifact missing -> refused
        self.assertEqual(run("approve", "T1", "--actor", LEADER,
                             "--base", REPO)[0], 0)
        self.assertEqual(run("verify")[0], 0)

    def test_cli_bad_deadline_returns_structured_json_error(self):
        # #425: a malformed --deadline must not leak a traceback; it must
        # come back as the structured {"ok": false, ...} JSON error.
        err = io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            rc = ztask.main(["--db", self.path, "create", "--title", "T",
                             "--goal", "g", "--owner", LEADER,
                             "--acceptance", "a",
                             "--deadline", "not-a-date"])
        self.assertEqual(rc, 1)
        self.assertNotIn("Traceback", err.getvalue())
        out = json.loads(err.getvalue())
        self.assertFalse(out["ok"])
        self.assertIn("deadline", out["message"].lower())

    def test_cli_sqlite_error_returns_structured_json_error(self):
        # #425: a database error mid-command must not leak a traceback.
        err = io.StringIO()
        with mock.patch.object(
                ztask.Store, "tasks",
                side_effect=sqlite3.Error("disk I/O error")), \
                redirect_stdout(io.StringIO()), redirect_stderr(err):
            rc = ztask.main(["--db", self.path, "list"])
        self.assertEqual(rc, 1)
        self.assertNotIn("Traceback", err.getvalue())
        out = json.loads(err.getvalue())
        self.assertFalse(out["ok"])
        self.assertEqual(out["error"], "database_error")

    def test_cli_corrupt_database_returns_structured_json_error(self):
        # #425: a corrupt database file fails while opening the Store, so
        # the structured error must cover Store construction too.
        self.store.close()
        for suffix in ("", "-wal", "-shm"):
            try:
                os.remove(self.path + suffix)
            except FileNotFoundError:
                pass
        Path(self.path).write_bytes(b"this is not a sqlite database")
        err = io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            rc = ztask.main(["--db", self.path, "list"])
        self.assertEqual(rc, 1)
        self.assertNotIn("Traceback", err.getvalue())
        out = json.loads(err.getvalue())
        self.assertFalse(out["ok"])
        self.assertEqual(out["error"], "database_error")


class EvidenceATests(Base):
    """Evidence A: `verify --evidence` and the approve gate."""

    def verdict(self, ev, **kw):
        return ztask.verify_evidence(ev, REPO, **kw)["verdict"]

    def test_fake_evidence_rejected(self):
        d = tempfile.mkdtemp()
        f = os.path.join(d, "a.txt")
        Path(f).write_bytes(b"hello")
        sha = hashlib.sha256(b"hello").hexdigest()
        # #424: artifact paths must be relative to base_dir; the old test
        # passed an absolute path, which the confinement fix now rejects.
        ev = json.dumps({"cmd": "t", "exit": 0,
                         "artifacts": [{"path": "a.txt", "sha256": sha}]})
        self.assertEqual(ztask.check_evidence(ev, d), "verified")
        self.assertEqual(ztask.check_evidence("raw text"), "reported")
        Path(f).write_bytes(b"hellp")  # 1 byte changed
        with self.assertRaises(ZtaskError):
            ztask.check_evidence(ev, d)
        os.remove(f)
        with self.assertRaises(ZtaskError):
            ztask.check_evidence(ev, d)
        shutil.rmtree(d)

    def test_verify_evidence_pass_and_each_failure(self):
        ok = ztask.verify_evidence(EV, REPO)
        self.assertEqual(ok["verdict"], "verified")
        self.assertEqual({c["result"] for c in ok["checks"]}, {"verified"})
        self.assertIn(HEAD, [c["detail"] for c in ok["checks"]])
        self.assertEqual(self.verdict(evidence(commit=HEAD[:7])), "verified")
        bad = hashlib.sha256(b"x").hexdigest()
        for ev in ("plain text", "[1]", "{not json",
                   evidence(exit=1), evidence(exit=True), evidence(exit="0"),
                   evidence(cmd=""), evidence(commit="nothex"),
                   evidence(commit="0" * 40),       # well formed, absent
                   evidence(commit=None),
                   evidence(artifacts=[]),
                   evidence(artifacts=["out.txt"]),
                   evidence(artifacts=[{"path": "out.txt", "sha256": bad}]),
                   evidence(artifacts=[{"path": "out.txt",
                                        "sha256": "ABC"}]),
                   evidence(artifacts=[{"path": "gone.txt", "sha256": bad}])):
            self.assertEqual(self.verdict(ev), "failed", ev)
        # One bad artifact among good ones still fails.
        good = json.loads(EV)["artifacts"][0]
        self.assertEqual(self.verdict(evidence(artifacts=[
            good, {"path": "gone.txt", "sha256": bad}])), "failed")

    def test_verify_evidence_undecided_never_passes(self):
        nogit = tempfile.mkdtemp()
        try:
            shutil.copy(os.path.join(REPO, "out.txt"), nogit)
            # artifact fine, but the commit cannot be checked: undecided
            self.assertEqual(ztask.verify_evidence(EV, nogit)["verdict"],
                             "undecided")
            self.assertEqual(ztask.verify_evidence(EV, nogit, repo=REPO)
                             ["verdict"], "verified")
            # a directory where a file should be: cannot read -> undecided
            # #424: the old test passed an absolute path, which the
            # confinement fix now rejects (failed); use a relative path.
            os.mkdir(os.path.join(nogit, "dir"))
            self.assertEqual(ztask.verify_evidence(evidence(artifacts=[
                {"path": "dir", "sha256": "0" * 64}]), nogit,
                repo=REPO)["verdict"], "undecided")
            # git missing from PATH: undecided, not pass
            old = os.environ.get("PATH", "")
            os.environ["PATH"] = nogit
            try:
                self.assertEqual(self.verdict(EV), "undecided")
            finally:
                os.environ["PATH"] = old
        finally:
            shutil.rmtree(nogit)

    def test_cli_verify_evidence_exit_codes(self):
        def run(*argv):
            out = io.StringIO()
            with redirect_stdout(out), redirect_stderr(io.StringIO()):
                rc = ztask.main(["--db", self.path, *argv])
            return rc, json.loads(out.getvalue())

        f = os.path.join(self.dir.name, "ev.json")
        Path(f).write_text(EV)
        rc, out = run("verify", "--evidence", f, "--base", REPO)
        self.assertEqual((rc, out["verdict"], out["ok"]),
                         (0, "verified", True))
        self.assertEqual(out["evidence_sha256"],
                         hashlib.sha256(EV.encode()).hexdigest())
        Path(f).write_text(evidence(exit=2))
        self.assertEqual(run("verify", "--evidence", f, "--base", REPO)[0], 1)
        self.assertEqual(run("verify", "--evidence", f + ".missing")[0], 1)
        Path(f).write_text(EV)
        nogit = tempfile.mkdtemp()
        try:
            shutil.copy(os.path.join(REPO, "out.txt"), nogit)
            rc, out = run("verify", "--evidence", f, "--base", nogit)
            self.assertEqual((rc, out["verdict"], out["ok"]),
                             (3, "undecided", False))
        finally:
            shutil.rmtree(nogit)
        # Plain `verify` still checks the chain.
        self.assertEqual(run("verify")[1]["ok"], True)

    def test_approve_gate_needs_verified_evidence(self):
        tid = self.new_task()
        tok = self.claim_start(tid, DEV)
        self.store.submit(tid, DEV, tok, evidence="test output, trust me")
        self.store.attest(tid, QA, command="c", receipt="r")
        with self.assertRaises(InvalidTransition) as cm:
            self.store.approve(tid, LEADER)          # reported, not verified
        self.assertIn("failed", str(cm.exception))
        tok = self.store.reject(tid, LEADER, reason="need JSON")["data"][
            "token"]
        self.store.submit(tid, DEV, tok, evidence=evidence(exit=1))
        self.store.attest(tid, QA, command="c", receipt="r")
        with self.assertRaises(InvalidTransition):
            self.store.approve(tid, LEADER)          # exit code 1
        tok = self.store.reject(tid, LEADER, reason="tests fail")["data"][
            "token"]
        self.store.submit(tid, DEV, tok, evidence=evidence(commit="0" * 40))
        self.store.attest(tid, QA, command="c", receipt="r")
        with self.assertRaises(InvalidTransition):
            self.store.approve(tid, LEADER)          # commit absent
        self.assertEqual(self.store.task(tid)["state"], "REVIEW")
        tok = self.store.reject(tid, LEADER, reason="commit")["data"]["token"]
        self.store.submit(tid, DEV, tok, evidence=EV)
        self.store.attest(tid, QA, command="c", receipt="r")
        ev = self.store.approve(tid, LEADER)
        self.assertEqual(self.store.task(tid)["state"], "DONE")
        self.assertEqual(ev["data"]["evidence_verdict"], "verified")
        self.assertEqual(ev["data"]["evidence_commit"], HEAD)
        self.store.verify()

    def test_approve_gate_artifact_tampered_after_submit(self):
        tid = self.new_task()
        tok = self.claim_start(tid, DEV)
        self.store.submit(tid, DEV, tok, evidence=EV)
        self.store.attest(tid, QA, command="c", receipt="r")
        art = Path(REPO, "out.txt")
        art.write_bytes(ARTIFACT + b"x")
        try:
            with self.assertRaises(InvalidTransition) as cm:
                self.store.approve(tid, LEADER)
            self.assertIn("sha256 mismatch", str(cm.exception))
        finally:
            art.write_bytes(ARTIFACT)
        self.store.approve(tid, LEADER)
        self.assertEqual(self.store.task(tid)["state"], "DONE")

    def test_approve_gate_verifier_never_a_claimer(self):
        # DEV claims, releases; DEV2 claims and submits; DEV (a past claimer,
        # no longer the assignee) attests. attest allows it, approve refuses.
        tid = self.new_task()
        tok = self.store.claim(tid, DEV)["data"]["token"]
        self.store.release(tid, DEV, tok)
        tok = self.claim_start(tid, DEV2)
        self.store.submit(tid, DEV2, tok, evidence=EV)
        self.store.attest(tid, DEV, command="c", receipt="r")
        with self.assertRaises(NotAllowed) as cm:
            self.store.approve(tid, LEADER)
        self.assertIn("no self-verify", str(cm.exception))
        self.assertEqual(self.store.task(tid)["state"], "REVIEW")
        self.store.attest(tid, QA, command="c", receipt="r")
        self.store.approve(tid, LEADER)
        self.assertEqual(self.store.task(tid)["state"], "DONE")


class EvidenceConfinementTests(Base):
    """Issue #424: artifact paths confined to base_dir, no hash oracle,
    special / oversized files refused before reading.

    Seven of the eight tests FAIL on the unfixed code (wrong verdict,
    leaked hash, or a hang) and PASS with the fix; the eighth
    (test_benign_dotdot_staying_inside_still_verified) is a regression
    guard that passes on both - it pins that a benign `..` staying inside
    base_dir is still accepted.
    """

    def _ev(self, artifacts, base=None):
        return json.dumps({"cmd": "t", "exit": 0, "commit": HEAD,
                           "artifacts": artifacts})

    def _art(self, path, data):
        return {"path": path,
                "sha256": hashlib.sha256(data).hexdigest()}

    def test_absolute_path_rejected_both_sites(self):
        d = tempfile.mkdtemp()
        try:
            outside = os.path.join(d, "outside.txt")
            Path(outside).write_bytes(b"secret")
            art = self._art(outside, b"secret")
            with self.assertRaises(ZtaskError):
                ztask.check_evidence(self._ev([art]), d)
            out = ztask.verify_evidence(self._ev([art]), d, repo=REPO)
            self.assertEqual(out["verdict"], "failed")
            self.assertNotIn(hashlib.sha256(b"secret").hexdigest(),
                             json.dumps(out))
        finally:
            shutil.rmtree(d, ignore_errors=True)

    def test_dotdot_path_rejected_both_sites(self):
        base = tempfile.mkdtemp()
        try:
            Path(os.path.join(base, "secret.txt")).write_bytes(b"secret")
            inner = os.path.join(base, "inner")
            os.mkdir(inner)
            art = self._art("../secret.txt", b"secret")
            with self.assertRaises(ZtaskError):
                ztask.check_evidence(self._ev([art]), inner)
            out = ztask.verify_evidence(self._ev([art]), inner, repo=REPO)
            self.assertEqual(out["verdict"], "failed")
            self.assertNotIn(hashlib.sha256(b"secret").hexdigest(),
                             json.dumps(out))
        finally:
            shutil.rmtree(base, ignore_errors=True)

    def test_symlink_inside_base_pointing_out_rejected(self):
        base = tempfile.mkdtemp()
        try:
            target = os.path.join(base, "target.txt")
            Path(target).write_bytes(b"secret")
            inner = os.path.join(base, "inner")
            os.mkdir(inner)
            os.symlink(target, os.path.join(inner, "link.txt"))
            art = self._art("link.txt", b"secret")
            with self.assertRaises(ZtaskError):
                ztask.check_evidence(self._ev([art]), inner)
            out = ztask.verify_evidence(self._ev([art]), inner, repo=REPO)
            self.assertEqual(out["verdict"], "failed")
        finally:
            shutil.rmtree(base, ignore_errors=True)

    def test_benign_dotdot_staying_inside_still_verified(self):
        # "sub/../out.txt" resolves inside base_dir: still accepted.
        sub = os.path.join(REPO, "sub")
        os.mkdir(sub)
        try:
            art = {"path": "sub/../out.txt",
                   "sha256": hashlib.sha256(ARTIFACT).hexdigest()}
            self.assertEqual(ztask.check_evidence(self._ev([art]), REPO),
                             "verified")
            self.assertEqual(ztask.verify_evidence(self._ev([art]), REPO)
                             ["verdict"], "verified")
        finally:
            shutil.rmtree(sub, ignore_errors=True)

    def test_mismatch_never_prints_recomputed_hash(self):
        true_sha = hashlib.sha256(ARTIFACT).hexdigest()
        wrong = hashlib.sha256(b"something else").hexdigest()
        ev = self._ev([{"path": "out.txt", "sha256": wrong}])
        out = ztask.verify_evidence(ev, REPO)
        self.assertEqual(out["verdict"], "failed")
        self.assertNotIn(true_sha, json.dumps(out))
        self.assertTrue(any("sha256 mismatch" in c["detail"]
                            for c in out["checks"]))
        with self.assertRaises(ZtaskError) as cm:
            ztask.check_evidence(ev, REPO)
        self.assertNotIn(true_sha, str(cm.exception))
        self.assertIn("hash mismatch", str(cm.exception))

    def _run_driver(self, path, base, want_verdict):
        # Runs both sites in a child process with a timeout: on unfixed
        # code a FIFO / /dev/zero artifact blocks forever on open/read,
        # so the child times out and this test fails instead of hanging.
        lines = [
            "import json, sys",
            "sys.path.insert(0, %r)" % str(Path(__file__).parent),
            "import ztask",
            "ev = json.dumps({'cmd': 't', 'exit': 0, 'commit': %r,"
            % HEAD,
            "  'artifacts': [{'path': %r, 'sha256': '0' * 64}]})" % path,
            "try:",
            "    ztask.check_evidence(ev, %r)" % base,
            "    sys.exit('check_evidence accepted')",
            "except ztask.ZtaskError:",
            "    pass",
            "out = ztask.verify_evidence(ev, %r, repo=%r)" % (base, REPO),
            "assert out['verdict'] == %r, out" % want_verdict,
        ]
        try:
            r = subprocess.run([sys.executable, "-c", "\n".join(lines)],
                               capture_output=True, text=True, timeout=10)
        except subprocess.TimeoutExpired:
            self.fail("verification hung on special file %r" % path)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_dev_zero_rejected_without_hanging(self):
        d = tempfile.mkdtemp()
        try:
            self._run_driver("/dev/zero", d, "failed")
        finally:
            shutil.rmtree(d, ignore_errors=True)

    def test_fifo_rejected_without_hanging(self):
        d = tempfile.mkdtemp()
        try:
            os.mkfifo(os.path.join(d, "pipe"))
            self._run_driver("pipe", d, "undecided")
        finally:
            shutil.rmtree(d, ignore_errors=True)

    def test_oversized_artifact_refused_before_reading(self):
        d = tempfile.mkdtemp()
        try:
            big = os.path.join(d, "big.bin")
            size = getattr(ztask, "EVIDENCE_MAX_ARTIFACT_BYTES",
                            64 << 20) + 1
            with open(big, "wb") as f:  # sparse: fast, no 64 MiB written
                f.truncate(size)
            with open(big, "rb") as f:
                sha = hashlib.sha256(f.read()).hexdigest()
            ev = self._ev([{"path": "big.bin", "sha256": sha}])
            out = ztask.verify_evidence(ev, d, repo=REPO)
            self.assertEqual(out["verdict"], "undecided")
            self.assertIn("too large", json.dumps(out))
            with self.assertRaises(ZtaskError) as cm:
                ztask.check_evidence(ev, d)
            self.assertIn("too large", str(cm.exception))
        finally:
            shutil.rmtree(d, ignore_errors=True)

    def test_symlink_swapped_in_after_resolve_rejected(self):
        # TOCTOU (#533): the artifact is a regular file when its path is
        # resolved, but a concurrent writer swaps it for a symlink pointing
        # outside base_dir before it is opened. The open must refuse the
        # symlink (O_NOFOLLOW), not follow it to the outside file whose
        # hash matches. The swap is injected by wrapping os.open, the way
        # the reviewer's controlled interleaving probe did it.
        d = tempfile.mkdtemp()
        try:
            base = os.path.join(d, "base")
            os.mkdir(base)
            outside = os.path.join(d, "outside.txt")
            Path(outside).write_bytes(b"secret")
            art_path = os.path.join(base, "art.txt")
            Path(art_path).write_bytes(b"secret")
            art = self._art("art.txt", b"secret")
            ev = self._ev([art])
            real_open = os.open
            swapped = []

            def swapping_open(path, *args, **kwargs):
                if not swapped and os.path.abspath(path) == art_path:
                    os.unlink(path)
                    os.symlink(outside, path)
                    swapped.append(True)
                return real_open(path, *args, **kwargs)

            with mock.patch.object(os, "open", swapping_open):
                with self.assertRaises(ZtaskError):
                    ztask.check_evidence(ev, base)
                self.assertTrue(swapped, "the swap never ran")
                out = ztask.verify_evidence(ev, base, repo=REPO)
                self.assertNotEqual(out["verdict"], "verified")
        finally:
            shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()

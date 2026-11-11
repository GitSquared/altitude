#!/usr/bin/env python3
"""Builds a throwaway altitude home with fake tasks, so the interface can be
looked at without starting any real work.

    scripts/demo.py [--root DIR]

It writes a configuration naming the operator's two projects (so the tabs are
the real ones), copies the database only to reuse its migrations, throws the
recorded tasks away, and writes a set of fabricated ones covering every section
of the task list.

Look at it through the preview, which draws the same list with no task service
behind it, so nothing is ever started:

    scripts/demo.py && ALTITUDE_PREVIEW=1 \
        ALTITUDE_CONFIG_DIR=/tmp/altitude-demo/config \
        ALTITUDE_DATA_DIR=/tmp/altitude-demo/data cargo run

Each task's project names an agent credential that does not exist, so even the
real interface cannot start one. Keep it that way when adding tasks.
"""

import argparse
import json
import sqlite3
import subprocess
import time
import uuid
from pathlib import Path

DATABASE = Path.home() / "Library/Application Support/altitude/altitude.db"

PROJECTS = {
    "altitude": "b777a587-46ba-4713-b65b-6d9d66a6b88d",
    "carbonfact": "7d60f0aa-1b6c-4ef7-bf86-497946fd2757",
}


def project(name, agent):
    return {
        "id": PROJECTS[name],
        "name": name,
        "agent": agent,
        # The credential is deliberately one that is not configured: recovery
        # fails here, before any sandbox exists.
        "agent_credential": "agents/demo",
    }


class Dataset:
    def __init__(self, now_ms):
        self.now = now_ms
        self.tasks = []
        self.events = []

    def task(self, name, project, agent, title, minutes_ago, steps):
        """Records a task. `steps` are (minutes_after_creation, event) pairs."""
        task_id = f"task_{name}"
        created = self.now - minutes_ago * 60_000
        self.tasks.append(
            {
                "id": task_id,
                "agent_kind": agent,
                "host": HOST,
                "created_at_ms": created,
            }
        )
        self.event(
            task_id,
            created,
            "intent_expressed",
            {
                "execution_status": "pending",
                "execution_mode": "autopilot",
                "operator_attention_level": "nominal",
                "sandbox_available": False,
                "snapshot_available": False,
                "intent": title,
                "project": project,
                "identity_id": "demo-operator",
                "identity_label": "demo",
                "agent_conversation_id": None,
            },
        )
        self.event(task_id, created + 30_000, "task_title_set", {"title": title})
        # Spread the work across the task's life rather than bunching it just
        # after creation, so the ages the list shows are not all the same.
        for offset, (kind, payload) in enumerate(steps):
            at = created + (self.now - created) * (offset + 1) // (len(steps) + 1)
            self.event(task_id, int(at), kind, payload)
        return task_id

    def event(self, task_id, at_ms, kind, data):
        self.events.append((task_id, at_ms, json.dumps({"type": kind, "data": data})))


# A signal is a thread, and a thread is a wait on the operator: the two kinds are the
# question the work cannot go on without and the work that is ready to read.
def signal(task_id, index, kind, title, body):
    return (
        "operator_signal_raised",
        {
            "signal": {
                "id": f"{task_id}-signal-{index}",
                "task_id": task_id,
                "kind": kind,
                "title": title,
                "body": body,
                "raised_at_ms": 0,
            }
        },
    )


def deferred():
    """A review the host is holding, which is what keeps a pending task out of
    the queue: the demo must never be picked up and started."""
    return (
        "review_deferred",
        {
            "signal": {
                "id": "held-review",
                "task_id": "",
                "kind": "ready_for_review",
                "title": "held",
                "body": "held",
                "raised_at_ms": 0,
            },
            "agent_run_id": str(uuid.uuid4()),
        },
    )


def limited(window, minutes):
    return (
        "limit_held",
        {
            "window": window,
            "clears_at_ms": int((time.time() + minutes * 60) * 1000),
        },
    )


def spent(dollars, priced=True, charged=True):
    return (
        "agent_usage_reported",
        {
            "cost_usd_micros": int(dollars * 1_000_000),
            "cost_from_provider": priced,
            "is_charged": charged,
        },
    )


def planned(done, total):
    return (
        "plan_updated",
        {
            "steps": [
                {"content": f"step {index + 1}", "status": "completed" if index < done else "pending"}
                for index in range(total)
            ]
        },
    )


def paused(reason):
    return (
        "execution_paused",
        {
            "execution_status": "paused",
            "sandbox_available": False,
            "snapshot_available": False,
            "execution_pause_reason": reason,
            "snapshot_path": "",
        },
    )


def running(mode="autopilot"):
    return (
        "agent_run_started",
        {
            "execution_status": "running",
            "agent_run_id": str(uuid.uuid4()),
            "sandbox_id": "demo-sandbox",
            "execution_mode": mode,
            "answering_signal_id": None,
        },
    )


def bound(identifier, title):
    return (
        "tracker_issue_bound",
        {
            "issue": {
                "issue": {
                    "reference": {
                        "provider": "linear",
                        "workspace_id": "demo",
                        "id": str(uuid.uuid4()),
                        "identifier": identifier,
                        "url": f"https://linear.app/demo/issue/{identifier}",
                    },
                    "title": title,
                },
                "description": "",
                "comments": [],
            },
            # No delegation: the tracker is a reference here, not a place this
            # task reports to, so nothing is ever sent to it.
            "delegation": None,
        },
    )


def contribution(reference, failing, passing):
    return (
        "code_contribution_linked",
        {
            "contribution": {
                "id": f"contribution-{reference}",
                "provider": "git_hub",
                "host": "github.com",
                "repository": "GitSquared/altitude",
                "reference": reference,
                "url": f"https://github.com/GitSquared/altitude/pull/{reference}",
            }
        },
    )


def observed(reference, failing, passing, summary):
    return (
        "code_contribution_observed",
        {
            "observation": {
                "contribution_id": f"contribution-{reference}",
                "state": {
                    "head_sha": uuid.uuid4().hex,
                    "checks_passing": passing,
                    "checks_failing": failing,
                    "checks_pending": 0,
                    "standing": "unstable" if failing else "clear",
                    "checks_settle_by_ms": 0,
                },
                "summary": summary,
            }
        },
    )


HOST = "gaby@neuromancer.local"


def dataset(now_ms):
    data = Dataset(now_ms)
    altitude = project("altitude", "pi")
    carbonfact = project("carbonfact", "claude")

    failed = data.task(
        "failed-sandbox-exit",
        altitude,
        "pi",
        "Extract the queue from the event loop",
        14,
        [
            running(),
            (
                "execution_failed",
                {
                    "execution_status": "failed",
                    "error": "the sandbox exited with 137 while linking",
                },
            ),
        ],
    )
    _ = failed

    blocked = data.task(
        "needs-you-retry-policy",
        altitude,
        "pi",
        "Choose the retry policy for delegation",
        96,
        [
            running(),
            paused("awaiting_operator"),
            signal(
                "replay-me",
                1,
                "blocking_question",
                "Which retry policy should a delegation use?",
                "A dropped link can be retried on the same call or handed back to the queue. The first is faster and can repeat a side effect; the second is safe and slower.",
            ),
            signal(
                "replay-me",
                2,
                "blocking_question",
                "Retry the relay or wait for mDNS?",
                "The relay answered twice with a stale certificate. Waiting for discovery avoids it but can take minutes.",
            ),
            signal(
                "replay-me",
                3,
                "blocking_question",
                "Should the retry budget be per task or per issue?",
                "Per task is simple. Per issue is what an operator means when they say a thing failed too often today.",
            ),
        ],
    )
    _ = blocked

    data.task(
        "needs-you-tool-names",
        carbonfact,
        "claude",
        "Two harnesses disagree on tool names",
        9,
        [
            running(),
            paused("awaiting_operator"),
            signal(
                "tool-names",
                1,
                "blocking_question",
                "Which harness is right about tool names?",
                "pi reports `tasks_signal`, claude reports `signal`. One registry with two names is a bug waiting to be read as a feature.",
            ),
        ],
    )

    data.task(
        "review-card-state",
        altitude,
        "pi",
        "A card says where it stands",
        26,
        [
            running(),
            planned(6, 8),
            spent(1.37),
            contribution("482", 2, 3),
            observed("482", 2, 3, "2 of 5 checks failing"),
            paused("awaiting_review_checks"),
            signal(
                "review",
                1,
                "ready_for_review",
                "The card rewrite is ready to read",
                "Eight sections, one line a task, and the age is now the last fact that moved.",
            ),
            bound("ALT-6", "Better task list"),
        ],
    )

    data.task(
        "decision-one-store",
        altitude,
        "pi",
        "One store per machine, and no copy",
        60,
        [running()],
    )

    data.task(
        "info-activity-lines",
        altitude,
        "pi",
        "Activity stream reads as lines",
        190,
        [running()],
    )

    data.task(
        "working-plan-pane",
        altitude,
        "pi",
        "Plan pane is half the column",
        4,
        [running()],
    )

    data.task(
        "working-watcher",
        carbonfact,
        "claude",
        "Rewrite the watcher with one channel",
        18,
        [running("operator"), planned(3, 6), spent(0.86, priced=False)],
    )

    data.task(
        "unnamed",
        altitude,
        "pi",
        "bold-otter",
        11,
        [running()],
    )
    # The title is the fallback the interface shows before the agent writes one.
    data.events = [
        (task, at, payload)
        for (task, at, payload) in data.events
        if not (task == "task_unnamed" and json.loads(payload)["type"] == "task_title_set")
    ]

    data.task(
        "queued-mirror",
        altitude,
        "pi",
        "Rename the mirror, then the store",
        12,
        [("execution_queued", {"execution_status": "pending"}), deferred()],
    )

    data.task(
        "queued-retry-guard",
        carbonfact,
        "claude",
        "Bringing the retry guard home",
        1,
        [("execution_queued", {"execution_status": "pending"}), deferred()],
    )

    data.task(
        "paused-quiet",
        altitude,
        "pi",
        "The retry arrives once",
        300,
        [paused("agent_went_quiet"), spent(0.42, priced=False)],
    )

    data.task(
        "limited-weekly",
        carbonfact,
        "claude",
        "Split the watcher into two channels",
        45,
        [
            running(),
            planned(2, 5),
            paused("awaiting_limit"),
            limited("weekly", 25),
            spent(3.18),
        ],
    )

    data.task(
        "done-delegation",
        altitude,
        "pi",
        "A delegation this machine answered is not picked up again",
        1400,
        [running(), ("task_completed", {"execution_status": "completed"})],
    )

    data.task(
        "done-shells",
        carbonfact,
        "claude",
        "Close a shell without ending the task",
        2900,
        [running(), ("task_completed", {"execution_status": "completed"})],
    )

    return data


def build(root):
    config = root / "config"
    data = root / "data"
    cache = root / "cache"
    for directory in (config, data, cache):
        directory.mkdir(parents=True, exist_ok=True)

    # Only the projects, by the ids the fabricated tasks name, so the tabs read
    # as they do in the operator's own window. Nothing else is copied: the
    # remaining settings name the operator's credentials and remotes, which do
    # not exist here and are not this script's to write into /tmp.
    projects = "\n".join(
        f'[[projects]]\nid = "{project_id}"\nname = "{name}"\n'
        for name, project_id in PROJECTS.items()
    )
    (config / "config.toml").write_text(
        "[interface]\nkeyboard_hints = true\nanimations = true\n\n" + projects
    )

    database = data / "altitude.db"
    # A database left by an earlier run brings its journal, and a journal whose head
    # does not match the file beside it is a disk error on the next fetch.
    for stale in (database, *database.glob("altitude.db-*")):
        stale.unlink(missing_ok=True)
    # Do not copy the real tasks: the copy is for the schema and the migration
    # bookkeeping sqlx keeps, which is what a hand-made database would get wrong.
    subprocess.run(
        ["sqlite3", str(DATABASE), f".backup '{database}'"], check=True
    )

    connection = sqlite3.connect(database)
    connection.execute("DELETE FROM task_event_logs")
    connection.execute("DELETE FROM tasks")
    connection.execute("DELETE FROM remote_keys")
    now = int(time.time() * 1000)
    built = dataset(now)
    for task in built.tasks:
        connection.execute(
            "INSERT INTO tasks (id, agent_kind, host, removed_at_ms, created_at_ms)"
            " VALUES (?, ?, ?, NULL, ?)",
            (task["id"], task["agent_kind"], task["host"], task["created_at_ms"]),
        )
    for task_id, at_ms, payload in built.events:
        connection.execute(
            "INSERT INTO task_event_logs (id, task_id, recorded_at_ms, payload)"
            " VALUES (?, ?, ?, ?)",
            (str(uuid.uuid4()), task_id, at_ms, payload),
        )
    connection.commit()
    connection.close()

    print(f"{len(built.tasks)} fake tasks in {database}")
    print()
    print("run it with:")
    print(
        f"  ALTITUDE_PREVIEW=1 ALTITUDE_CONFIG_DIR={config} ALTITUDE_DATA_DIR={data} "
        f"ALTITUDE_CACHE_DIR={cache} cargo run"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default="/tmp/altitude-demo")
    build(Path(parser.parse_args().root))

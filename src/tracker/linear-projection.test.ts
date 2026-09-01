import { describe, expect, test } from "bun:test";

import type {
  LinearAgentInputRequest,
  LinearIssueCommentCreate,
  LinearTrackerMutations,
} from "./linear.ts";
import {
  createLinearProjectionExecutor,
  parseLinearProjectionPayload,
  planLinearProjection,
} from "./projection.ts";
import { LinearTrackerStateStore } from "./state.ts";
import { TaskStore } from "../storage/task-store.ts";

class FakeMutations implements LinearTrackerMutations {
  readonly comments: LinearIssueCommentCreate[] = [];
  readonly activities: LinearAgentInputRequest[] = [];

  async createIssueComment(input: LinearIssueCommentCreate): Promise<{ commentId: string }> {
    this.comments.push(input);
    return { commentId: `comment-${this.comments.length}` };
  }

  async requestAgentInput(input: LinearAgentInputRequest): Promise<{ activityId: string }> {
    this.activities.push(input);
    return { activityId: `activity-${this.activities.length}` };
  }
}

function store(): TaskStore {
  const result = new TaskStore(":memory:", { clock: () => 1_000 });
  result.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Linear", intent: "Project additive attention", intentLevel: "principles" },
    actor: { kind: "operator" },
  });
  return result;
}

describe("Linear projection outbox", () => {
  test("plans nothing until the operator explicitly binds a task", () => {
    using tasks = store();
    expect(planLinearProjection(tasks, {
      taskId: "task-1",
      type: "attention.raised",
      payload: {
        attention: { id: "trace-1", class: "trace", title: "Trace", summary: "Found it" },
      },
      actor: { kind: "agent", adapter: "claude" },
    })).toEqual([]);
  });

  test("projects raises and resolutions in order and persists their correlation", async () => {
    using tasks = store();
    const state = new LinearTrackerStateStore(tasks.database, () => 1_000);
    state.bind({ taskId: "task-1", issueId: "issue-1", agentSessionId: "session-1" });
    const raised = {
      taskId: "task-1",
      type: "attention.raised" as const,
      payload: {
        attention: {
          id: "update-1",
          class: "update" as const,
          title: "Checkpoint",
          summary: "The adapter is wired.",
        },
      },
      actor: { kind: "agent" as const, adapter: "claude" },
    };
    tasks.append(raised, planLinearProjection(tasks, raised));
    const mutations = new FakeMutations();
    const execute = createLinearProjectionExecutor({ store: tasks, state, mutations });
    const first = tasks.claimOutboxByTopic("tracker.linear.projection", {
      workerId: "worker",
      now: 1_000,
    })[0]!;
    expect(await execute(first.taskId, parseLinearProjectionPayload(first.payload))).toBe("executed");
    tasks.deliverOutbox(first.id, "worker", 1_001);
    expect(state.getCorrelation("task-1", "update-1")).toMatchObject({
      issueCommentId: "comment-1",
      attentionClass: "update",
    });

    const resolved = {
      taskId: "task-1",
      type: "attention.resolved" as const,
      payload: {
        attentionId: "update-1",
        resolution: { kind: "acknowledged" as const, note: "Seen" },
      },
      actor: { kind: "operator" as const },
    };
    tasks.append(resolved, planLinearProjection(tasks, resolved));
    const second = tasks.claimOutboxByTopic("tracker.linear.projection", {
      workerId: "worker",
      now: 1_001,
    })[0]!;
    expect(await execute(second.taskId, parseLinearProjectionPayload(second.payload))).toBe("executed");
    expect(mutations.comments).toHaveLength(2);
    expect(mutations.comments[1]).toMatchObject({
      issueId: "issue-1",
      parentCommentId: "comment-1",
    });
    expect(mutations.activities).toHaveLength(0);
  });
});

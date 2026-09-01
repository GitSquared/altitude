import { describe, expect, test } from "bun:test";
import {
  LinearSdkMutations,
  type LinearAgentActivityMutationInput,
  type LinearCommentMutationInput,
  type LinearMutationClient,
} from "./linear-api.ts";
import { linearAttentionRoute } from "./linear.ts";

class FakeLinearClient implements LinearMutationClient {
  readonly comments: LinearCommentMutationInput[] = [];
  readonly activities: LinearAgentActivityMutationInput[] = [];
  fail = false;

  createComment(input: LinearCommentMutationInput) {
    this.comments.push(input);
    return Promise.resolve({
      success: !this.fail,
      comment: { id: input.id ?? "generated-comment" },
    });
  }

  createAgentActivity(input: LinearAgentActivityMutationInput) {
    this.activities.push(input);
    return Promise.resolve({
      success: !this.fail,
      agentActivity: { id: input.id ?? "generated-activity" },
    });
  }
}

describe("Linear SDK mutations", () => {
  test("uses deterministic client IDs for idempotent comments and native elicitations", async () => {
    const sdk = new FakeLinearClient();
    const client = new LinearSdkMutations({
      accessToken: "process-only-token",
      client: sdk,
    });
    const routing = linearAttentionRoute("decision", true);
    const comment = {
      issueId: "issue-1",
      body: "Decision needed",
      idempotencyKey: "linear:attention:task-1:decision-1:comment",
      routing,
    } as const;
    const activity = {
      agentSessionId: "session-1",
      body: "Which transport?",
      idempotencyKey: "linear:attention:task-1:decision-1:agent-input",
      routing,
    } as const;

    const firstComment = await client.createIssueComment(comment);
    const retriedComment = await client.createIssueComment(comment);
    const firstActivity = await client.requestAgentInput(activity);
    const retriedActivity = await client.requestAgentInput(activity);

    expect(firstComment).toEqual(retriedComment);
    expect(firstActivity).toEqual(retriedActivity);
    expect(firstComment.commentId).toMatch(/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/);
    expect(sdk.comments[0]).toMatchObject({
      issueId: "issue-1",
      body: "Decision needed",
      doNotSubscribeToIssue: true,
    });
    expect(sdk.activities[0]).toMatchObject({
      agentSessionId: "session-1",
      content: { type: "elicitation", body: "Which transport?" },
    });
  });

  test("fails closed when the SDK does not confirm a mutation", async () => {
    const sdk = new FakeLinearClient();
    sdk.fail = true;
    const client = new LinearSdkMutations({ accessToken: "token", client: sdk });
    const input = {
      issueId: "issue-1",
      body: "Update",
      idempotencyKey: "update-1",
      routing: linearAttentionRoute("update", false),
    } as const;
    expect(client.createIssueComment(input)).rejects.toThrow("did not confirm");
  });

  test("refuses non-TLS API endpoints", () => {
    expect(() => new LinearSdkMutations({
      accessToken: "token",
      endpoint: "http://linear.example.test/graphql",
      client: new FakeLinearClient(),
    })).toThrow("HTTPS or loopback");
  });
});

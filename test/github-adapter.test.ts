import { describe, expect, test } from "bun:test";

import {
  GitHubCliAdapter,
  evaluatePullRequestReadiness,
  type PullRequestSnapshot,
} from "../src/codehost/github.ts";
import type { CommandResult, CommandRunner, CommandSpec } from "../src/process/command.ts";

class FakeRunner implements CommandRunner {
  readonly commands: CommandSpec[] = [];

  constructor(private readonly response: CommandResult) {}

  async run(spec: CommandSpec): Promise<CommandResult> {
    this.commands.push(spec);
    return this.response;
  }

  async runInteractive(): Promise<number> {
    throw new Error("not used");
  }
}

const rawPullRequest = {
  url: "https://github.com/example/project/pull/42",
  number: 42,
  title: "Build the useful thing",
  state: "OPEN",
  isDraft: true,
  mergeStateStatus: "CLEAN",
  reviewDecision: "APPROVED",
  headRefName: "agent/useful-thing",
  headRefOid: "abc123",
  statusCheckRollup: [
    { __typename: "CheckRun", name: "test", status: "COMPLETED", conclusion: "SUCCESS", detailsUrl: "https://ci.test" },
    { __typename: "StatusContext", context: "preview", state: "SUCCESS", targetUrl: "https://preview.test" },
  ],
  latestReviews: [
    { author: { login: "reviewer", __typename: "User" }, state: "APPROVED", submittedAt: "2026-08-28T10:00:00Z" },
    { author: { login: "lint-bot[bot]", __typename: "Bot" }, state: "COMMENTED", body: "Looks tidy" },
  ],
};

describe("GitHub adapter", () => {
  test("normalizes pull request evidence without making it core state", async () => {
    const runner = new FakeRunner({ exitCode: 0, stdout: JSON.stringify(rawPullRequest), stderr: "" });
    const adapter = new GitHubCliAdapter(runner, () => new Date("2026-08-28T11:00:00Z"));
    const snapshot = await adapter.inspectPullRequest(rawPullRequest.url);

    expect(snapshot.checks.map((check) => check.state)).toEqual(["success", "success"]);
    expect(snapshot.reviews.map((review) => review.actor)).toEqual(["human", "bot"]);
    expect(snapshot.observedAt).toBe("2026-08-28T11:00:00.000Z");
    expect(runner.commands[0]?.args?.at(-1)).toContain("headRefOid");
  });

  test("requires evidence beyond green CI before Review", () => {
    const snapshot: PullRequestSnapshot = {
      url: rawPullRequest.url,
      number: 42,
      title: "Build",
      state: "open",
      draft: true,
      mergeState: "CLEAN",
      reviewDecision: "APPROVED",
      headRefName: "agent/build",
      headSha: "abc123",
      checks: [{ name: "test", state: "success" }],
      reviews: [],
      observedAt: "2026-08-28T11:00:00.000Z",
    };

    expect(evaluatePullRequestReadiness(snapshot, {
      worktreeClean: true,
      headPushed: true,
      pathExercised: false,
      feedbackAddressed: true,
    })).toEqual({ ready: false, blockers: ["The changed path has not been exercised"] });
  });

  test("creates draft pull requests without merge authority", async () => {
    const runner = new FakeRunner({
      exitCode: 0,
      stdout: "https://github.com/example/project/pull/43\n",
      stderr: "",
    });
    const adapter = new GitHubCliAdapter(runner);
    const url = await adapter.createDraftPullRequest({
      repository: "example/project",
      head: "agent/next",
      base: "main",
      title: "Next",
      body: "Automated draft",
    });

    expect(url).toEndWith("/43");
    expect(runner.commands[0]?.args).toContain("--draft");
    expect(runner.commands[0]?.args).not.toContain("merge");
  });
});

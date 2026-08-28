import type { CommandRunner } from "../process/command.ts";

export type CheckState = "pending" | "success" | "failure" | "neutral" | "cancelled" | "unknown";

export interface PullRequestCheck {
  readonly name: string;
  readonly state: CheckState;
  readonly url?: string;
}

export interface PullRequestReview {
  readonly author: string;
  readonly actor: "human" | "bot";
  readonly state: "approved" | "changes_requested" | "commented" | "dismissed" | "pending" | "unknown";
  readonly submittedAt?: string;
  readonly body?: string;
}

export interface PullRequestSnapshot {
  readonly url: string;
  readonly number: number;
  readonly title: string;
  readonly state: "open" | "closed" | "merged";
  readonly draft: boolean;
  readonly mergeState: string;
  readonly reviewDecision: string;
  readonly headRefName: string;
  readonly headSha: string;
  readonly checks: readonly PullRequestCheck[];
  readonly reviews: readonly PullRequestReview[];
  readonly observedAt: string;
}

export interface PullRequestReadinessEvidence {
  readonly worktreeClean: boolean;
  readonly headPushed: boolean;
  readonly pathExercised: boolean;
  readonly feedbackAddressed: boolean;
}

export interface PullRequestReadiness {
  readonly ready: boolean;
  readonly blockers: readonly string[];
}

interface RawPullRequest {
  readonly url?: unknown;
  readonly number?: unknown;
  readonly title?: unknown;
  readonly state?: unknown;
  readonly isDraft?: unknown;
  readonly mergeStateStatus?: unknown;
  readonly reviewDecision?: unknown;
  readonly headRefName?: unknown;
  readonly headRefOid?: unknown;
  readonly statusCheckRollup?: unknown;
  readonly latestReviews?: unknown;
}

function requiredString(value: unknown, field: string): string {
  if (typeof value !== "string" || value.length === 0) throw new Error(`GitHub omitted ${field}`);
  return value;
}

function requiredNumber(value: unknown, field: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) throw new Error(`GitHub omitted ${field}`);
  return value;
}

function normalizeCheckState(status: unknown, conclusion: unknown, state: unknown): CheckState {
  const normalizedStatus = typeof status === "string" ? status.toUpperCase() : "";
  const normalizedConclusion = typeof conclusion === "string" ? conclusion.toUpperCase() : "";
  const normalizedState = typeof state === "string" ? state.toUpperCase() : "";

  if (["QUEUED", "IN_PROGRESS", "PENDING", "EXPECTED"].includes(normalizedStatus || normalizedState)) {
    return "pending";
  }
  if (["SUCCESS", "NEUTRAL", "SKIPPED"].includes(normalizedConclusion || normalizedState)) {
    return normalizedConclusion === "NEUTRAL" || normalizedConclusion === "SKIPPED" ? "neutral" : "success";
  }
  if (["CANCELLED", "STALE"].includes(normalizedConclusion || normalizedState)) return "cancelled";
  if (["FAILURE", "ERROR", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE"].includes(normalizedConclusion || normalizedState)) {
    return "failure";
  }
  return "unknown";
}

function parseChecks(value: unknown): PullRequestCheck[] {
  if (!Array.isArray(value)) return [];
  return value.map((entry, index) => {
    if (typeof entry !== "object" || entry === null) throw new Error(`Invalid GitHub check at index ${index}`);
    const check = entry as Record<string, unknown>;
    const name = requiredString(check.name ?? check.context, `check[${index}].name`);
    const url = typeof check.detailsUrl === "string"
      ? check.detailsUrl
      : typeof check.targetUrl === "string"
        ? check.targetUrl
        : undefined;
    return {
      name,
      state: normalizeCheckState(check.status, check.conclusion, check.state),
      ...(url === undefined ? {} : { url }),
    };
  });
}

function normalizeReviewState(value: unknown): PullRequestReview["state"] {
  if (typeof value !== "string") return "unknown";
  switch (value.toUpperCase()) {
    case "APPROVED":
      return "approved";
    case "CHANGES_REQUESTED":
      return "changes_requested";
    case "COMMENTED":
      return "commented";
    case "DISMISSED":
      return "dismissed";
    case "PENDING":
      return "pending";
    default:
      return "unknown";
  }
}

function parseReviews(value: unknown): PullRequestReview[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((entry) => {
    if (typeof entry !== "object" || entry === null) return [];
    const review = entry as Record<string, unknown>;
    const authorValue = review.author;
    const author = typeof authorValue === "object" && authorValue !== null
      ? (authorValue as Record<string, unknown>).login
      : undefined;
    if (typeof author !== "string" || author.length === 0) return [];
    const actorType = typeof authorValue === "object" && authorValue !== null
      ? (authorValue as Record<string, unknown>).__typename
      : undefined;
    const bot = actorType === "Bot" || author.endsWith("[bot]");
    return [{
      author,
      actor: bot ? "bot" as const : "human" as const,
      state: normalizeReviewState(review.state),
      ...(typeof review.submittedAt === "string" ? { submittedAt: review.submittedAt } : {}),
      ...(typeof review.body === "string" && review.body.length > 0 ? { body: review.body } : {}),
    }];
  });
}

function parsePullRequest(value: unknown, observedAt: string): PullRequestSnapshot {
  if (typeof value !== "object" || value === null) throw new Error("GitHub returned an invalid pull request");
  const raw = value as RawPullRequest;
  const rawState = requiredString(raw.state, "state").toUpperCase();
  const state = rawState === "OPEN" ? "open" : rawState === "MERGED" ? "merged" : "closed";
  return {
    url: requiredString(raw.url, "url"),
    number: requiredNumber(raw.number, "number"),
    title: requiredString(raw.title, "title"),
    state,
    draft: raw.isDraft === true,
    mergeState: typeof raw.mergeStateStatus === "string" ? raw.mergeStateStatus : "UNKNOWN",
    reviewDecision: typeof raw.reviewDecision === "string" ? raw.reviewDecision : "",
    headRefName: requiredString(raw.headRefName, "headRefName"),
    headSha: requiredString(raw.headRefOid, "headRefOid"),
    checks: parseChecks(raw.statusCheckRollup),
    reviews: parseReviews(raw.latestReviews),
    observedAt,
  };
}

export function evaluatePullRequestReadiness(
  snapshot: PullRequestSnapshot,
  evidence: PullRequestReadinessEvidence,
): PullRequestReadiness {
  const blockers: string[] = [];
  if (snapshot.state !== "open") blockers.push("Pull request is not open");
  if (["DIRTY", "CONFLICTING", "BLOCKED"].includes(snapshot.mergeState.toUpperCase())) blockers.push("Pull request has conflicts");
  if (snapshot.checks.some((check) => check.state === "pending")) blockers.push("Checks are still running");
  if (snapshot.checks.some((check) => ["failure", "cancelled", "unknown"].includes(check.state))) {
    blockers.push("Checks are not green");
  }
  if (snapshot.reviews.some((review) => review.actor === "human" && review.state === "changes_requested")) {
    blockers.push("A human review requests changes");
  }
  if (!evidence.worktreeClean) blockers.push("Final worktree has uncommitted work");
  if (!evidence.headPushed) blockers.push("Final head is not pushed");
  if (!evidence.pathExercised) blockers.push("The changed path has not been exercised");
  if (!evidence.feedbackAddressed) blockers.push("Review feedback is not fully addressed");
  return { ready: blockers.length === 0, blockers };
}

export class GitHubCliAdapter {
  constructor(
    private readonly runner: CommandRunner,
    private readonly now: () => Date = () => new Date(),
  ) {}

  async authenticated(): Promise<boolean> {
    try {
      return (await this.runner.run({ executable: "gh", args: ["auth", "status"] })).exitCode === 0;
    } catch {
      return false;
    }
  }

  async inspectPullRequest(url: string): Promise<PullRequestSnapshot> {
    if (!/^https:\/\/github\.com\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+\/pull\/[1-9][0-9]*$/.test(url)) {
      throw new Error("Invalid GitHub pull request URL");
    }
    const result = await this.runner.run({
      executable: "gh",
      args: [
        "pr",
        "view",
        url,
        "--json",
        "url,number,title,state,isDraft,mergeStateStatus,reviewDecision,headRefName,headRefOid,statusCheckRollup,latestReviews",
      ],
    });
    if (result.exitCode !== 0) throw new Error(`GitHub inspection failed: ${result.stderr.trim()}`);
    return parsePullRequest(JSON.parse(result.stdout) as unknown, this.now().toISOString());
  }

  async createDraftPullRequest(input: {
    readonly repository: string;
    readonly head: string;
    readonly base: string;
    readonly title: string;
    readonly body: string;
  }): Promise<string> {
    if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(input.repository)) throw new Error("Invalid GitHub repository");
    const result = await this.runner.run({
      executable: "gh",
      args: [
        "pr",
        "create",
        "--repo",
        input.repository,
        "--head",
        input.head,
        "--base",
        input.base,
        "--title",
        input.title,
        "--body",
        input.body,
        "--draft",
      ],
    });
    if (result.exitCode !== 0) throw new Error(`Creating GitHub pull request failed: ${result.stderr.trim()}`);
    const url = result.stdout.trim().split("\n").find((line) => line.startsWith("https://github.com/"));
    if (url === undefined) throw new Error("GitHub did not return a pull request URL");
    return url;
  }
}

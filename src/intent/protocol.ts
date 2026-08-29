export type IntentToolKind = "redmarker";

export interface IntentArtifactEnvelope {
  readonly schema: "altitude.intent-artifact/v1";
  readonly artifactId: string;
  readonly taskId: string;
  readonly capsuleId: string;
  readonly tool: IntentToolKind;
  readonly createdAt: string;
  readonly base: {
    readonly url: string;
    readonly headSha: string;
    readonly checkpointDigest: string;
    readonly checkpointRevision: number;
  };
  readonly payloadSchema: string;
  readonly payload: unknown;
  readonly renderedMarkdown?: string;
}

export interface IntentToolLaunch {
  readonly tool: IntentToolKind;
  readonly executable: string;
  readonly args: readonly string[];
  readonly environment: Readonly<Record<string, string>>;
  readonly localUrl?: string;
}

export interface IntentToolAdapter {
  readonly kind: IntentToolKind;
  launch(input: {
    readonly taskId: string;
    readonly capsuleId: string;
    readonly targetUrl: string;
    readonly submissionUrl: string;
    readonly submissionToken: string;
  }): IntentToolLaunch;
}

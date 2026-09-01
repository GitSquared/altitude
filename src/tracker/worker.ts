import { LinearSdkMutations } from "./linear-api.ts";
import {
  LINEAR_PROJECTION_TOPIC,
  createLinearProjectionExecutor,
  parseLinearProjectionPayload,
} from "./projection.ts";
import type { TaskStore } from "../storage/task-store.ts";
import { NodeOutboxTopicWorker } from "../node/worker.ts";

export type LinearProjectionWorker = NodeOutboxTopicWorker<
  ReturnType<typeof parseLinearProjectionPayload>
>;

export function linearAccessToken(
  env: Readonly<Record<string, string | undefined>> = process.env,
): string | undefined {
  const token = env.ALTITUDE_LINEAR_ACCESS_TOKEN;
  const normalized = token?.trim();
  return normalized || undefined;
}

export function createLinearProjectionWorker(input: Readonly<{
  store: TaskStore;
  accessToken?: string;
  now?: () => number;
  onError?: (error: unknown) => void;
}>): LinearProjectionWorker | undefined {
  if (input.accessToken === undefined) return undefined;
  return new NodeOutboxTopicWorker(input.store, {
    topic: LINEAR_PROJECTION_TOPIC,
    parse: parseLinearProjectionPayload,
    execute: createLinearProjectionExecutor({
      store: input.store,
      mutations: new LinearSdkMutations({ accessToken: input.accessToken }),
    }),
  }, {
    ...(input.now === undefined ? {} : { now: input.now }),
    claimLimit: 1,
    ...(input.onError === undefined ? {} : { onError: input.onError }),
  });
}

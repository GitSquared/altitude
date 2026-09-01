import { describe, expect, test } from "bun:test";

import { TaskStore } from "../storage/task-store.ts";
import { createLinearProjectionWorker, linearAccessToken } from "./worker.ts";

describe("Linear projection worker", () => {
  test("reads only an explicitly supplied process environment", () => {
    expect(linearAccessToken({})).toBeUndefined();
    expect(linearAccessToken({ ALTITUDE_LINEAR_ACCESS_TOKEN: "  token  " })).toBe("token");
  });

  test("does not create a network worker without a token", () => {
    const store = new TaskStore(":memory:");
    expect(createLinearProjectionWorker({ store })).toBeUndefined();
    store.close();
  });
});

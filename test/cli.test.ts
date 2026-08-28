import { describe, expect, test } from "bun:test";

import { runCli } from "../src/cli/main.ts";

describe("CLI", () => {
  test("returns a usage error for an unknown command", async () => {
    expect(await runCli(["definitely-not-a-command"])).toBe(2);
  });
});

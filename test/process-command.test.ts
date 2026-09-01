import { describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import type { ReadStream } from "node:tty";

import { BunCommandRunner, withInteractiveInput } from "../src/process/command.ts";

class FakeInteractiveInput extends EventEmitter {
  readonly rawModes: boolean[] = [];
  isRaw = false;
  readonly isTTY = true;

  setRawMode(enabled: boolean): this {
    this.isRaw = enabled;
    this.rawModes.push(enabled);
    return this;
  }

  resume(): this {
    return this;
  }
}

describe("interactive command input", () => {
  test("pipes exact escape and control bytes without sharing the terminal fd", async () => {
    const input = new FakeInteractiveInput();
    const expected = "1b5b411b5b410304";
    const script = `
      const expected = "${expected}";
      let received = Buffer.alloc(0);
      process.stdin.on("data", (chunk) => {
        received = Buffer.concat([received, chunk]);
        if (received.length >= expected.length / 2) {
          process.exit(received.toString("hex") === expected ? 0 : 2);
        }
      });
      setTimeout(() => process.exit(3), 2000);
    `;

    const exitCode = await withInteractiveInput(
      input as unknown as ReadStream,
      async () => await new BunCommandRunner().runInteractive({
        executable: process.execPath,
        args: ["-e", script],
      }, {
        started: () => input.emit("data", Buffer.from(expected, "hex")),
      }),
    );

    expect(exitCode).toBe(0);
    expect(input.rawModes).toEqual([true, false]);
    expect(input.listenerCount("data")).toBe(0);
  });
});

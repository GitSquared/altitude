import { describe, expect, test } from "bun:test";

import { createTaskId } from "./task-id.ts";

describe("human task ids", () => {
  test("uses two words while they are unique", () => {
    const values = [0, 0];
    expect(createTaskId(() => false, () => values.shift()!)).toBe("amber-anchor");
  });

  test("adds a third word only after a real collision", () => {
    const values = [0, 0, 0];
    expect(createTaskId(
      (candidate) => candidate === "amber-anchor",
      () => values.shift()!,
    )).toBe("amber-anchor-apricot");
  });

  test("fits every task-id storage and capsule path constraint", () => {
    for (let index = 0; index < 100; index += 1) {
      const id = createTaskId();
      expect(id).toMatch(/^[a-z]+-[a-z]+$/u);
      expect(id.length).toBeLessThan(128);
    }
  });
});

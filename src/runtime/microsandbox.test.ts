import { describe, expect, test } from "bun:test";

import { isMicrosandboxError, sizeInMebibytes } from "./microsandbox.ts";

describe("Microsandbox runtime", () => {
  test("normalizes capsule resource limits to the SDK's MiB boundary", () => {
    expect(sizeInMebibytes("512M")).toBe(512);
    expect(sizeInMebibytes("8G")).toBe(8 * 1024);
    expect(sizeInMebibytes("1T")).toBe(1024 * 1024);
    expect(sizeInMebibytes("1K")).toBe(1);
    expect(() => sizeInMebibytes("0G")).toThrow("Invalid Microsandbox size");
  });

  test("recognizes typed errors from the raw native binding", () => {
    expect(isMicrosandboxError(
      Object.assign(new Error("[SandboxNotFound] sandbox not found"), { code: "GenericFailure" }),
      "SandboxNotFound",
    )).toBeTrue();
  });
});

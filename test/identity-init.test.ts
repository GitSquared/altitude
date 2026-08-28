import { afterEach, describe, expect, test } from "bun:test";
import { lstatSync, mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { initializeIdentityRoot } from "../src/identity/init.ts";

const directories: string[] = [];

afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

describe("identity volume initialization", () => {
  test("creates only the private node-local identity layout", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-identity-init-"));
    directories.push(root);
    await initializeIdentityRoot(root, process.getuid?.() ?? 0, process.getgid?.() ?? 0);
    for (const path of ["home", "claude", "control", "control/tasks"]) {
      const stat = lstatSync(join(root, path));
      expect(stat.isDirectory()).toBeTrue();
      expect(stat.mode & 0o777).toBe(0o700);
    }
    expect(() => lstatSync(join(root, "workspace"))).toThrow();
  });

  test("rejects a symlinked identity root", async () => {
    const parent = mkdtempSync(join(tmpdir(), "altitude-identity-init-link-"));
    directories.push(parent);
    const target = join(parent, "target");
    const link = join(parent, "identity");
    mkdirSync(target);
    symlinkSync(target, link);
    expect(initializeIdentityRoot(link, process.getuid?.() ?? 0, process.getgid?.() ?? 0)).rejects.toThrow("symbolic link");
  });
});

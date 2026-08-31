import { afterEach, describe, expect, test } from "bun:test";
import {
  chmodSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readlinkSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  TASK_ENVIRONMENT_METADATA_MEMBER,
  captureTaskEnvironment,
  restoreTaskEnvironment,
  taskEnvironmentVariables,
} from "../src/capsule/task-environment.ts";

const directories: string[] = [];

afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function workspace(label: string): string {
  const root = mkdtempSync(join(tmpdir(), `altitude-environment-${label}-`));
  directories.push(root);
  const result = Bun.spawnSync(["git", "init", "--initial-branch=main"], {
    cwd: root,
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
  return root;
}

describe("portable task environment", () => {
  test("round-trips tools, config, empty directories, and safe symlinks", async () => {
    const source = workspace("source");
    await captureTaskEnvironment(source);
    const environment = join(source, ".altitude", "environment");
    mkdirSync(join(environment, "home", ".config", "demo"), { recursive: true });
    writeFileSync(join(environment, "home", ".config", "demo", "settings.json"), "{\"theme\":\"high\"}\n");
    writeFileSync(join(environment, "bin", "hello"), "#!/bin/sh\necho hello\n");
    chmodSync(join(environment, "bin", "hello"), 0o755);
    mkdirSync(join(environment, "empty"));
    symlinkSync("../bin/hello", join(environment, "home", "hello"));

    const first = await captureTaskEnvironment(source);
    const second = await captureTaskEnvironment(source);
    expect(second).toEqual(first);
    expect(first[0]?.path).toBe(TASK_ENVIRONMENT_METADATA_MEMBER);

    const target = workspace("target");
    await restoreTaskEnvironment(target, first);
    expect(await Bun.file(join(target, ".altitude", "environment", "home", ".config", "demo", "settings.json")).text())
      .toBe("{\"theme\":\"high\"}\n");
    expect(lstatSync(join(target, ".altitude", "environment", "bin", "hello")).mode & 0o111).not.toBe(0);
    expect(lstatSync(join(target, ".altitude", "environment", "empty")).isDirectory()).toBeTrue();
    expect(readlinkSync(join(target, ".altitude", "environment", "home", "hello"))).toBe("../bin/hello");
    expect(Bun.spawnSync(["git", "status", "--porcelain"], { cwd: target }).stdout.toString()).toBe("");
  });

  test("fails closed for credential locations and escaping symlinks", async () => {
    const secret = workspace("secret");
    await captureTaskEnvironment(secret);
    const home = join(secret, ".altitude", "environment", "home");
    mkdirSync(join(home, ".ssh"));
    writeFileSync(join(home, ".ssh", "id_ed25519"), "secret");
    await expect(captureTaskEnvironment(secret)).rejects.toThrow("credential boundary");

    const escaping = workspace("escaping");
    await captureTaskEnvironment(escaping);
    symlinkSync("/etc/passwd", join(escaping, ".altitude", "environment", "home", "outside"));
    await expect(captureTaskEnvironment(escaping)).rejects.toThrow("escapes the portable workbench");
  });

  test("uses portable home and bin paths while leaving caches disposable", () => {
    const environment = taskEnvironmentVariables();
    expect(environment.HOME).toBe("/workspace/.altitude/environment/home");
    expect(environment.PATH?.startsWith("/workspace/.altitude/environment/bin:")).toBeTrue();
    expect(environment.XDG_CACHE_HOME).toBe("/tmp/altitude-cache");
  });
});

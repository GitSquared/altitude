import { afterEach, describe, expect, test } from "bun:test";
import { lstatSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { defaultLocalConfig, readLocalConfig, writeLocalConfig } from "../src/config/local.ts";

const directories: string[] = [];

afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

describe("local Altitude config", () => {
  test("round-trips only portable non-secret node settings in a private file", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-config-test-"));
    directories.push(root);
    const path = join(root, "config", "config.json");
    const config = defaultLocalConfig("docker");
    await writeLocalConfig(path, config);
    expect(await readLocalConfig(path)).toEqual(config);
    expect(lstatSync(path).mode & 0o777).toBe(0o600);
    expect(JSON.stringify(config)).not.toContain("token");
    expect(config.node.capsuleMcpEndpoint).toContain("host.docker.internal");
  });

  test("rejects a symlink config boundary", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-config-link-"));
    directories.push(root);
    const target = join(root, "target.json");
    await Bun.write(target, "{}", { mode: 0o600 });
    const link = join(root, "config.json");
    symlinkSync(target, link);
    expect(readLocalConfig(link)).rejects.toThrow("regular file");
    expect(writeLocalConfig(link, defaultLocalConfig("docker"))).rejects.toThrow("regular file");
  });

  test("uses the explicit Apple host alias without pretending it is preconfigured", () => {
    expect(defaultLocalConfig("apple-container").node.capsuleMcpEndpoint).toContain("host.container.internal");
  });
});

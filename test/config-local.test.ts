import { afterEach, describe, expect, test } from "bun:test";
import { lstatSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  capsuleMcpEndpoint,
  DEFAULT_IDENTITY_IMAGE,
  DEFAULT_TASK_IMAGE,
  defaultLocalConfig,
  readLocalConfig,
  writeLocalConfig,
} from "../src/config/local.ts";

const directories: string[] = [];

afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

describe("local Altitude config", () => {
  test("round-trips only portable non-secret node settings in a private file", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-config-test-"));
    directories.push(root);
    const path = join(root, "config", "config.json");
    const config = defaultLocalConfig();
    await writeLocalConfig(path, config);
    expect(await readLocalConfig(path)).toEqual(config);
    expect(lstatSync(path).mode & 0o777).toBe(0o600);
    expect(JSON.stringify(config)).not.toContain("token");
    expect(config.identity.image).toBe(DEFAULT_IDENTITY_IMAGE);
    expect(config.taskImage).toBe(DEFAULT_TASK_IMAGE);
    expect(config.node.bindHost).toBe("127.0.0.1");
    expect(capsuleMcpEndpoint(config)).toBe("http://172.16.0.5:43100/mcp");
  });

  test("rejects a symlink config boundary", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-config-link-"));
    directories.push(root);
    const target = join(root, "target.json");
    await Bun.write(target, "{}", { mode: 0o600 });
    const link = join(root, "config.json");
    symlinkSync(target, link);
    expect(readLocalConfig(link)).rejects.toThrow("regular file");
    expect(writeLocalConfig(link, defaultLocalConfig())).rejects.toThrow("regular file");
  });

  test("contains only explicit v1 settings", () => {
    const config = defaultLocalConfig();
    expect(Object.keys(config).sort()).toEqual(["identity", "node", "schema", "taskImage"]);
    expect(Object.keys(config.node).sort()).toEqual(["bindHost", "id", "port"]);
  });
});

import { describe, expect, test } from "bun:test";

import { appleLaunchSpec, appleVolumeCreateSpec } from "../src/runtime/apple-container.ts";
import { DEFAULT_CAPSULE_RESOURCES, type CapsuleLaunchSpec } from "../src/runtime/capsule-runtime.ts";
import { dockerLaunchSpec } from "../src/runtime/docker.ts";

const capsule: CapsuleLaunchSpec = {
  capsuleId: "task-01",
  image: "ghcr.io/example/altitude-capsule@sha256:abc",
  workspaceVolume: "altitude-task-01",
  network: "altitude-task-01",
  command: ["/opt/altitude/bin/task-runtime"],
  resources: DEFAULT_CAPSULE_RESOURCES,
  environment: { ALTITUDE_TASK_ID: "task-01", LANG: "C.UTF-8" },
};

describe("capsule runtime commands", () => {
  test("builds an isolated Apple container command", () => {
    const command = appleLaunchSpec(capsule);
    expect(command.executable).toBe("container");
    expect(command.args).toContain("--read-only");
    expect(command.args).toContain("ALL");
    expect(command.args).toContain("altitude-task-01:/workspace");
    expect(command.args?.slice(-1)).toEqual(["/opt/altitude/bin/task-runtime"]);
  });

  test("uses an explicit bounded Apple volume", () => {
    const command = appleVolumeCreateSpec(capsule);
    expect(command.args).toContain("64G");
    expect(command.args).toContain("journal=ordered");
  });

  test("builds a constrained Docker command for a Linux node", () => {
    const command = dockerLaunchSpec(capsule);
    expect(command.executable).toBe("docker");
    expect(command.args).toContain("no-new-privileges");
    expect(command.args).toContain("1024");
    expect(command.args).not.toContain("/var/run/docker.sock");
  });

  test("rejects names that can escape command arguments", () => {
    expect(() => appleLaunchSpec({ ...capsule, capsuleId: "bad capsule" })).toThrow("Invalid capsule ID");
  });
});

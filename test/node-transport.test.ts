import { describe, expect, test } from "bun:test";

import {
  SshNodeTransport,
  SshCapsuleTransferTransport,
  buildPortForwardSpec,
  buildRemoteCommandSpec,
  buildSshBaseArgs,
  type NodeConnection,
} from "../src/node/transport.ts";
import type { CommandResult, CommandRunner, CommandSpec } from "../src/process/command.ts";

const direct: NodeConnection = {
  nodeId: "straylight",
  user: "altitude",
  route: { kind: "ssh", host: "straylight.example", port: 2222 },
  identityFile: "/state/nodes/straylight/id_ed25519",
  knownHostsFile: "/state/nodes/straylight/known_hosts",
};

describe("node SSH transport", () => {
  test("uses an isolated SSH configuration and strict host keys", () => {
    expect(buildSshBaseArgs(direct)).toEqual([
      "-F",
      "/dev/null",
      "-o",
      "BatchMode=yes",
      "-o",
      "StrictHostKeyChecking=yes",
      "-o",
      "UserKnownHostsFile=/state/nodes/straylight/known_hosts",
      "-o",
      "IdentityFile=/state/nodes/straylight/id_ed25519",
      "-p",
      "2222",
      "altitude@straylight.example",
    ]);
  });

  test("uses Pigeons as a ProxyCommand without editing user SSH config", () => {
    const endpointId = "ab".repeat(32);
    const args = buildSshBaseArgs({
      ...direct,
      route: { kind: "pigeons", endpointId },
    });

    expect(args).toContain(`ProxyCommand=pigeons fly --stdio ${endpointId}`);
    expect(args.slice(0, 2)).toEqual(["-F", "/dev/null"]);
  });

  test("rejects unsafe route fields", () => {
    expect(() =>
      buildSshBaseArgs({ ...direct, route: { kind: "ssh", host: "host; touch /tmp/nope" } }),
    ).toThrow("Unsafe SSH host");
    expect(() => buildRemoteCommandSpec(
      direct,
      ["altitude", "version;touch", "/tmp/nope"],
    )).toThrow("Unsafe remote command argument");
  });

  test("builds loopback-only port forwards", () => {
    const spec = buildPortForwardSpec(direct, 43123, 3000);
    expect(spec.args).toContain("127.0.0.1:43123:127.0.0.1:3000");
  });

  test("validates request correlation", async () => {
    let captured: CommandSpec | undefined;
    const runner: CommandRunner = {
      async run(spec): Promise<CommandResult> {
        captured = spec;
        return {
          exitCode: 0,
          stdout: `${JSON.stringify({ schema: "altitude.node-rpc/v1", requestId: "r1", ok: true, result: 3 })}\n`,
          stderr: "",
        };
      },
      async runInteractive(): Promise<number> {
        throw new Error("not used");
      },
    };

    const response = await new SshNodeTransport(runner).rpc(direct, {
      schema: "altitude.node-rpc/v1",
      requestId: "r1",
      command: "status",
      payload: {},
    });

    expect(response.ok).toBe(true);
    expect(captured).toEqual(
      buildRemoteCommandSpec(direct, ["altitude-node", "rpc"], {
        stdin: `${JSON.stringify({ schema: "altitude.node-rpc/v1", requestId: "r1", command: "status", payload: {} })}\n`,
      }),
    );
  });

  test("keeps capsule bytes on SSH stdin and validates both transfer receipts", async () => {
    const captured: CommandSpec[] = [];
    const checkpoint = {
      capsuleId: "task-1",
      revision: 2,
      digest: `sha256:${"a".repeat(64)}` as const,
    };
    const runner: CommandRunner = {
      async run(spec): Promise<CommandResult> {
        captured.push(spec);
        const activate = spec.args?.at(-1) === "activate";
        return {
          exitCode: 0,
          stdout: `${JSON.stringify(activate ? {
            schema: "altitude.target-activation/v1",
            transferId: "transfer-1",
            targetNodeId: "straylight",
            checkpoint,
          } : {
            schema: "altitude.target-prepared/v1",
            transferId: "transfer-1",
            targetNodeId: "straylight",
            checkpoint,
          })}\n`,
          stderr: "",
        };
      },
      async runInteractive(): Promise<number> {
        throw new Error("not used");
      },
    };
    const transport = new SshCapsuleTransferTransport(runner, direct);
    const payload = new Uint8Array([1, 2, 3, 4]);
    expect(await transport.prepare(payload)).toMatchObject({
      schema: "altitude.target-prepared/v1",
      checkpoint,
    });
    expect(await transport.activate({ transferId: "transfer-1", checkpoint }))
      .toMatchObject({ schema: "altitude.target-activation/v1", checkpoint });
    expect(captured[0]?.stdin).toEqual(payload);
    expect(captured[0]?.args).not.toContain(payload.join(""));
    expect(captured[1]?.stdin).toBe(`${JSON.stringify({ transferId: "transfer-1", checkpoint })}\n`);
  });
});

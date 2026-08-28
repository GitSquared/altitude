import { describe, expect, test } from "bun:test";

import {
  SshNodeTransport,
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
});

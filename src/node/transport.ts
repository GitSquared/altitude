import type { CommandRunner, CommandSpec } from "../process/command.ts";
import type {
  CapsuleTransferTransport,
  TargetActivationReceipt,
  TargetPreparedReceipt,
} from "../capsule/transfer-runtime.ts";
import type { CapsuleCheckpointReference } from "../capsule/filesystem.ts";

export interface DirectSshRoute {
  readonly kind: "ssh";
  readonly host: string;
  readonly port?: number;
}
export interface PigeonsRoute {
  readonly kind: "pigeons";
  readonly endpointId: string;
  readonly pigeonsExecutable?: string;
}

export type NodeRoute = DirectSshRoute | PigeonsRoute;

export interface NodeConnection {
  readonly nodeId: string;
  readonly user: string;
  readonly route: NodeRoute;
  readonly identityFile: string;
  readonly knownHostsFile: string;
}

export interface NodeRpcRequest {
  readonly schema: "altitude.node-rpc/v1";
  readonly requestId: string;
  readonly command: string;
  readonly payload: unknown;
  readonly leaseEpoch?: number;
}

export interface NodeRpcResponse {
  readonly schema: "altitude.node-rpc/v1";
  readonly requestId: string;
  readonly ok: boolean;
  readonly result?: unknown;
  readonly error?: { readonly code: string; readonly message: string };
}

const SAFE_HOST = /^[A-Za-z0-9._:-]+$/;
const IROH_ENDPOINT = /^[a-f0-9]{64}$/i;
const SAFE_USER = /^[a-z_][a-z0-9_-]*$/i;
const SAFE_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const SAFE_ABSOLUTE_PATH = /^\/[A-Za-z0-9._/@%+,:=-]+$/;
const SAFE_EXECUTABLE = /^\/[A-Za-z0-9._/@%+,:=-]+$|^[A-Za-z0-9._-]+$/;

function assertConnection(connection: NodeConnection): void {
  if (!SAFE_ID.test(connection.nodeId)) throw new Error("Unsafe node id");
  if (!SAFE_USER.test(connection.user)) throw new Error("Unsafe SSH user");
  if (!SAFE_ABSOLUTE_PATH.test(connection.identityFile)
    || !SAFE_ABSOLUTE_PATH.test(connection.knownHostsFile)) {
    throw new Error("Unsafe SSH state path");
  }
  if (connection.route.kind === "ssh" && !SAFE_HOST.test(connection.route.host)) {
    throw new Error("Unsafe SSH host");
  }
  if (connection.route.kind === "ssh" && connection.route.port !== undefined
    && (!Number.isInteger(connection.route.port) || connection.route.port < 1 || connection.route.port > 65_535)) {
    throw new Error("Invalid SSH port");
  }
  if (connection.route.kind === "pigeons" && !IROH_ENDPOINT.test(connection.route.endpointId)) {
    throw new Error("Invalid Pigeons endpoint ID");
  }
  if (connection.route.kind === "pigeons"
    && !SAFE_EXECUTABLE.test(connection.route.pigeonsExecutable ?? "pigeons")) {
    throw new Error("Unsafe Pigeons executable");
  }
}

export function buildSshBaseArgs(
  connection: NodeConnection,
  options: { readonly acceptNewHostKey?: boolean } = {},
): readonly string[] {
  assertConnection(connection);
  const strictHostKey = options.acceptNewHostKey === true ? "accept-new" : "yes";
  const args = [
    "-F",
    "/dev/null",
    "-o",
    "BatchMode=yes",
    "-o",
    `StrictHostKeyChecking=${strictHostKey}`,
    "-o",
    `UserKnownHostsFile=${connection.knownHostsFile}`,
    "-o",
    `IdentityFile=${connection.identityFile}`,
  ];

  if (connection.route.kind === "ssh") {
    if (connection.route.port !== undefined) args.push("-p", String(connection.route.port));
    args.push(`${connection.user}@${connection.route.host}`);
  } else {
    const pigeons = connection.route.pigeonsExecutable ?? "pigeons";
    args.push("-o", `ProxyCommand=${pigeons} fly --stdio ${connection.route.endpointId}`);
    args.push(`${connection.user}@altitude-${connection.nodeId}`);
  }

  return args;
}

export function buildRemoteCommandSpec(
  connection: NodeConnection,
  remoteArgs: readonly string[],
  options: { readonly stdin?: string | Uint8Array; readonly acceptNewHostKey?: boolean } = {},
): CommandSpec {
  return {
    executable: "ssh",
    args: [...buildSshBaseArgs(connection, options), "--", ...remoteArgs],
    ...(options.stdin === undefined ? {} : { stdin: options.stdin }),
  };
}

export function buildPortForwardSpec(
  connection: NodeConnection,
  localPort: number,
  remotePort: number,
): Omit<CommandSpec, "stdin"> {
  for (const port of [localPort, remotePort]) {
    if (!Number.isInteger(port) || port < 1 || port > 65_535) throw new Error(`Invalid port: ${port}`);
  }

  return {
    executable: "ssh",
    args: [
      ...buildSshBaseArgs(connection),
      "-N",
      "-L",
      `127.0.0.1:${localPort}:127.0.0.1:${remotePort}`,
    ],
  };
}

export class SshNodeTransport {
  constructor(private readonly runner: CommandRunner) {}

  async rpc(connection: NodeConnection, request: NodeRpcRequest): Promise<NodeRpcResponse> {
    const result = await this.runner.run(
      buildRemoteCommandSpec(connection, ["altitude-node", "rpc"], {
        stdin: `${JSON.stringify(request)}\n`,
      }),
    );
    if (result.exitCode !== 0) {
      throw new Error(`Node RPC transport failed (${result.exitCode}): ${result.stderr.trim()}`);
    }

    const line = result.stdout.trim().split("\n").at(-1);
    if (line === undefined || line.length === 0) throw new Error("Node RPC returned no response");
    const parsed: unknown = JSON.parse(line);
    if (!isNodeRpcResponse(parsed) || parsed.requestId !== request.requestId) {
      throw new Error("Node RPC returned an invalid or mismatched response");
    }
    return parsed;
  }
}

function lastJsonLine(stdout: string, label: string): unknown {
  const line = stdout.trim().split("\n").at(-1);
  if (line === undefined || line.length === 0) throw new Error(`${label} returned no response`);
  try {
    return JSON.parse(line) as unknown;
  } catch (error) {
    throw new Error(`${label} returned invalid JSON`, { cause: error });
  }
}

function isCheckpointReference(value: unknown): value is CapsuleCheckpointReference {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return typeof record.capsuleId === "string"
    && Number.isSafeInteger(record.revision)
    && (record.revision as number) > 0
    && typeof record.digest === "string"
    && /^sha256:[a-f0-9]{64}$/u.test(record.digest);
}

function isTargetPreparedReceipt(value: unknown): value is TargetPreparedReceipt {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return record.schema === "altitude.target-prepared/v1"
    && typeof record.transferId === "string"
    && typeof record.targetNodeId === "string"
    && isCheckpointReference(record.checkpoint);
}

function isTargetActivationReceipt(value: unknown): value is TargetActivationReceipt {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return record.schema === "altitude.target-activation/v1"
    && typeof record.transferId === "string"
    && typeof record.targetNodeId === "string"
    && isCheckpointReference(record.checkpoint);
}

/** Streams transfer packages over the same isolated, host-key-pinned SSH route. */
export class SshCapsuleTransferTransport implements CapsuleTransferTransport {
  constructor(
    private readonly runner: CommandRunner,
    private readonly connection: NodeConnection,
    private readonly remoteExecutable = "altitude",
  ) {
    if (!SAFE_EXECUTABLE.test(remoteExecutable)) {
      throw new Error("Remote Altitude executable is invalid");
    }
  }

  async prepare(payload: Uint8Array): Promise<TargetPreparedReceipt> {
    const result = await this.runner.run(buildRemoteCommandSpec(
      this.connection,
      [this.remoteExecutable, "_transfer", "prepare"],
      { stdin: payload },
    ));
    if (result.exitCode !== 0) {
      throw new Error(`Remote capsule preparation failed (${result.exitCode}): ${result.stderr.trim()}`);
    }
    const receipt = lastJsonLine(result.stdout, "Remote capsule preparation");
    if (!isTargetPreparedReceipt(receipt)) {
      throw new Error("Remote capsule preparation returned an invalid receipt");
    }
    return receipt;
  }

  async activate(input: {
    readonly transferId: string;
    readonly checkpoint: CapsuleCheckpointReference;
  }): Promise<TargetActivationReceipt> {
    const result = await this.runner.run(buildRemoteCommandSpec(
      this.connection,
      [this.remoteExecutable, "_transfer", "activate"],
      { stdin: `${JSON.stringify(input)}\n` },
    ));
    if (result.exitCode !== 0) {
      throw new Error(`Remote capsule activation failed (${result.exitCode}): ${result.stderr.trim()}`);
    }
    const receipt = lastJsonLine(result.stdout, "Remote capsule activation");
    if (!isTargetActivationReceipt(receipt)) {
      throw new Error("Remote capsule activation returned an invalid receipt");
    }
    return receipt;
  }
}

export function isNodeRpcResponse(value: unknown): value is NodeRpcResponse {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  if (
    record.schema !== "altitude.node-rpc/v1" ||
    typeof record.requestId !== "string" ||
    typeof record.ok !== "boolean"
  ) {
    return false;
  }
  if (record.ok) return "error" in record === false || record.error === undefined;
  if (typeof record.error !== "object" || record.error === null) return false;
  const error = record.error as Record<string, unknown>;
  return typeof error.code === "string" && typeof error.message === "string";
}

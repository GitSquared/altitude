import { sha256, stableJson, utf8 } from "./canonical.ts";
import type { PortableCapsulePath } from "./paths.ts";

export type CommandArgument =
  | { readonly kind: "literal"; readonly value: string }
  | {
      readonly kind: "capsule-member";
      readonly access: "read" | "write";
      readonly path: PortableCapsulePath;
    };

export type CommandInput =
  | { readonly kind: "none" }
  | { readonly kind: "capsule-member"; readonly path: PortableCapsulePath };

export type CommandOutput =
  | { readonly kind: "capture"; readonly key: string }
  | { readonly kind: "capsule-member"; readonly path: PortableCapsulePath }
  | { readonly kind: "discard" };

export interface PlannedCommand {
  readonly id: string;
  readonly program: string;
  readonly arguments: readonly CommandArgument[];
  readonly cwd: string;
  readonly stdin: CommandInput;
  readonly stdout: CommandOutput;
}

export interface CommandPlan {
  readonly id: string;
  readonly commands: readonly PlannedCommand[];
}

export interface CommandResult {
  readonly exitCode: number;
  readonly stdout: Uint8Array;
  readonly stderr: Uint8Array;
}

export interface CommandExecutor {
  /**
   * Executors resolve capsule-member arguments against a private staging root,
   * never by interpolating them into a shell command. They should also use the
   * supplied key when the backing runtime offers stronger idempotency.
   */
  execute(
    command: PlannedCommand,
    context: { readonly idempotencyKey: string },
  ): Promise<CommandResult>;
}

export interface CommandReceipt {
  readonly fingerprint: string;
  readonly result: CommandResult;
}

export interface CommandReceiptStore {
  get(key: string): Promise<CommandReceipt | undefined>;
  put(key: string, receipt: CommandReceipt): Promise<void>;
}

export class InMemoryCommandReceiptStore implements CommandReceiptStore {
  readonly #receipts = new Map<string, CommandReceipt>();

  async get(key: string): Promise<CommandReceipt | undefined> {
    return this.#receipts.get(key);
  }

  async put(key: string, receipt: CommandReceipt): Promise<void> {
    const current = this.#receipts.get(key);
    if (current !== undefined && current.fingerprint !== receipt.fingerprint) {
      throw new CommandIdConflictError(key);
    }
    this.#receipts.set(key, receipt);
  }
}

export class CommandIdConflictError extends Error {
  override readonly name = "CommandIdConflictError";

  constructor(key: string) {
    super(`Command idempotency key ${JSON.stringify(key)} was reused with different input`);
  }
}

export class PlannedCommandError extends Error {
  override readonly name = "PlannedCommandError";

  constructor(
    readonly command: PlannedCommand,
    readonly result: CommandResult,
  ) {
    super(`Command ${command.id} exited with status ${result.exitCode}`);
  }
}

export async function executeCommandPlan(
  plan: CommandPlan,
  executor: CommandExecutor,
  receipts: CommandReceiptStore,
): Promise<ReadonlyMap<string, CommandResult>> {
  assertCommandIdentifier(plan.id, "plan id");
  const results = new Map<string, CommandResult>();
  const fingerprints = new Map<string, string>();

  for (const command of plan.commands) {
    validatePlannedCommand(command);
    const key = `${plan.id}/${command.id}`;
    const fingerprint = commandFingerprint(command);
    const earlierFingerprint = fingerprints.get(command.id);
    if (earlierFingerprint !== undefined && earlierFingerprint !== fingerprint) {
      throw new CommandIdConflictError(key);
    }
    fingerprints.set(command.id, fingerprint);

    const existing = await receipts.get(key);
    if (existing !== undefined) {
      if (existing.fingerprint !== fingerprint) {
        throw new CommandIdConflictError(key);
      }
      results.set(command.id, existing.result);
      continue;
    }

    // The receipt prevents ordinary replay. The executor receives the same key
    // so it can close the crash window between an external side effect and put().
    const result = await executor.execute(command, { idempotencyKey: key });
    if (result.exitCode !== 0) {
      throw new PlannedCommandError(command, result);
    }
    await receipts.put(key, { fingerprint, result });
    results.set(command.id, result);
  }
  return results;
}

export function commandFingerprint(command: PlannedCommand): string {
  return sha256(utf8(stableJson(command)));
}

export function literal(value: string): CommandArgument {
  return { kind: "literal", value };
}

export function readMember(path: PortableCapsulePath): CommandArgument {
  return { kind: "capsule-member", access: "read", path };
}

export function writeMember(path: PortableCapsulePath): CommandArgument {
  return { kind: "capsule-member", access: "write", path };
}

function validatePlannedCommand(command: PlannedCommand): void {
  assertCommandIdentifier(command.id, "command id");
  if (command.program.length === 0 || command.program.includes("\0")) {
    throw new TypeError("Command program is invalid");
  }
  if (command.cwd.length === 0 || command.cwd.includes("\0")) {
    throw new TypeError("Command working directory is invalid");
  }
  for (const argument of command.arguments) {
    if (argument.kind === "literal" && argument.value.includes("\0")) {
      throw new TypeError("Command arguments cannot contain NUL bytes");
    }
  }
}

function assertCommandIdentifier(value: string, label: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) {
    throw new TypeError(`${label} is invalid`);
  }
}

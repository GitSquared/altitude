import { encodeTaskBootstrapPayload, TASK_BOOTSTRAP_PROTOCOL } from "../capsule/bootstrap.ts";
import {
  encodeTaskCheckpointRequest,
  TASK_CHECKPOINT_PROTOCOL,
  type TaskCheckpointCaptureRequest,
} from "../capsule/task-checkpoint.ts";
import type { CapsuleCheckpoint } from "../capsule/manifest.ts";
import { decodeCapsuleWire, MAX_CAPSULE_WIRE_BYTES } from "../capsule/wire.ts";
import type { Sha256Digest } from "../capsule/canonical.ts";
import { DEFAULT_CAPSULE_RESOURCES, type CapsuleLaunchSpec, type CapsuleRuntime } from "./capsule-runtime.ts";
import { stableCapsuleName } from "./identity-capsule.ts";
import type { ProcessLauncher } from "./protocol.ts";
import { taskEnvironmentVariables } from "../capsule/task-environment.ts";

export interface TaskCapsuleSpec {
  readonly taskId: string;
  readonly image: string;
}

export type TaskCapsuleEnsureResult = Readonly<{
  capsuleId: string;
  volume: string;
  state: "already_running" | "resumed" | "created";
}>;

export function taskVolumeName(taskId: string): string {
  return `${stableCapsuleName("task", taskId)}-data`;
}

function launchSpec(input: TaskCapsuleSpec): CapsuleLaunchSpec {
  return {
    capsuleId: stableCapsuleName("task", input.taskId),
    image: input.image,
    volume: { name: taskVolumeName(input.taskId), mountPath: "/workspace" },
    resources: DEFAULT_CAPSULE_RESOURCES,
    environment: { ALTITUDE_TASK_ID: input.taskId, ...taskEnvironmentVariables() },
  };
}

export class TaskCapsuleManager {
  constructor(private readonly runtime: CapsuleRuntime) {}

  async ensure(input: TaskCapsuleSpec): Promise<TaskCapsuleEnsureResult> {
    const spec = launchSpec(input);
    const state = await this.runtime.ensure(spec);
    const initialized = await this.runtime.exec({
      capsuleId: spec.capsuleId,
      command: ["altitude", "_task-init"],
      environment: { ALTITUDE_TASK_INIT: "1" },
      workdir: "/workspace",
    });
    if (typeof initialized === "number" || initialized.exitCode !== 0) {
      const detail = typeof initialized === "number" ? `status ${initialized}` : initialized.stderr.trim();
      throw new Error(`Initializing task capsule failed: ${detail}`);
    }
    return { capsuleId: spec.capsuleId, volume: spec.volume.name, state };
  }
}

async function collect(stream: AsyncIterable<Uint8Array>, limit: number): Promise<string> {
  const decoder = new TextDecoder();
  let text = "";
  let size = 0;
  for await (const chunk of stream) {
    size += chunk.byteLength;
    if (size > limit) throw new Error("Task bootstrap response is too large");
    text += decoder.decode(chunk, { stream: true });
  }
  return `${text}${decoder.decode()}`;
}

async function collectBytes(stream: AsyncIterable<Uint8Array>, limit: number): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let size = 0;
  for await (const chunk of stream) {
    size += chunk.byteLength;
    if (size > limit) throw new Error("Task checkpoint response is too large");
    chunks.push(chunk);
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

export class TaskCapsuleBootstrapProvisioner {
  constructor(
    private readonly launcher: ProcessLauncher,
    private readonly executable = "altitude",
  ) {}

  async bootstrap(input: {
    readonly taskId: string;
    readonly checkpoint: CapsuleCheckpoint;
    readonly repositorySeedBundle: Uint8Array;
  }): Promise<{ readonly reused: boolean }> {
    if (input.checkpoint.manifest.taskId !== input.taskId) throw new Error("Task bootstrap checkpoint is bound to another task");
    const payload = encodeTaskBootstrapPayload(input);
    const child = await this.launcher.launch({
      target: { kind: "task-capsule", taskId: input.taskId },
      executable: this.executable,
      args: ["_task-bootstrap"],
      cwd: "/workspace",
      env: { ALTITUDE_TASK_BOOTSTRAP: "1" },
      io: "piped",
      purpose: "Materialize a verified capsule checkpoint in the disposable task workspace",
    });
    try {
      await child.writeStdin(payload);
      await child.closeStdin();
      const [exit, stdout, stderr] = await Promise.all([
        child.exited,
        collect(child.stdout, 256 * 1024),
        collect(child.stderr, 256 * 1024),
      ]);
      const response = JSON.parse(stdout) as { schema?: string; ok?: boolean; result?: { reused?: boolean }; error?: string };
      if (response.schema !== TASK_BOOTSTRAP_PROTOCOL || typeof response.ok !== "boolean") {
        throw new Error("Task bootstrap helper returned an unsupported response");
      }
      if (!response.ok || exit.exitCode !== 0) {
        throw new Error(response.error ?? (stderr.trim() || `Task bootstrap exited ${exit.exitCode}`));
      }
      return { reused: response.result?.reused === true };
    } catch (error) {
      await child.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }
}

export class TaskCapsuleCheckpointProvisioner {
  constructor(
    private readonly launcher: ProcessLauncher,
    private readonly executable = "altitude",
  ) {}

  async capture(request: TaskCheckpointCaptureRequest): Promise<CapsuleCheckpoint> {
    const process = await this.#launch(request.taskId, "capture");
    try {
      await process.writeStdin(encodeTaskCheckpointRequest(request));
      await process.closeStdin();
      const [exit, stdout, stderr] = await Promise.all([
        process.exited,
        collectBytes(process.stdout, MAX_CAPSULE_WIRE_BYTES),
        collect(process.stderr, 256 * 1024),
      ]);
      if (exit.exitCode !== 0) throw new Error(stderr.trim() || `Task checkpoint capture exited ${exit.exitCode}`);
      const checkpoint = decodeCapsuleWire(stdout);
      if (checkpoint.manifest.taskId !== request.taskId || checkpoint.manifest.capsuleId !== request.capsuleId
        || checkpoint.manifest.revision !== request.revision || checkpoint.manifest.checkpointedAt !== request.checkpointedAt
        || checkpoint.manifest.ownership.epoch !== request.ownership.epoch
        || checkpoint.manifest.ownership.leaseId !== request.ownership.leaseId
        || checkpoint.manifest.ownership.ownerNodeId !== request.ownership.ownerNodeId) {
        throw new Error("Task checkpoint helper returned a checkpoint outside its requested binding");
      }
      return checkpoint;
    } catch (error) {
      await process.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }

  async pending(taskId: string): Promise<CapsuleCheckpoint | undefined> {
    const process = await this.#launch(taskId, "pending");
    try {
      await process.closeStdin();
      const [exit, stdout, stderr] = await Promise.all([
        process.exited,
        collectBytes(process.stdout, MAX_CAPSULE_WIRE_BYTES),
        collect(process.stderr, 256 * 1024),
      ]);
      if (exit.exitCode === 3) return undefined;
      if (exit.exitCode !== 0) throw new Error(stderr.trim() || `Reading pending task checkpoint exited ${exit.exitCode}`);
      const checkpoint = decodeCapsuleWire(stdout);
      if (checkpoint.manifest.taskId !== taskId) throw new Error("Pending checkpoint is bound to another task");
      return checkpoint;
    } catch (error) {
      await process.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }

  async activate(taskId: string, checkpointDigest: Sha256Digest): Promise<void> {
    const process = await this.#launch(taskId, "activate");
    try {
      await process.writeStdin(JSON.stringify({
        schema: TASK_CHECKPOINT_PROTOCOL,
        operation: "activate",
        checkpointDigest,
      }));
      await process.closeStdin();
      const [exit, stdout, stderr] = await Promise.all([
        process.exited,
        collect(process.stdout, 64 * 1024),
        collect(process.stderr, 256 * 1024),
      ]);
      if (exit.exitCode !== 0) throw new Error(stderr.trim() || `Task checkpoint activation exited ${exit.exitCode}`);
      const response = JSON.parse(stdout) as { schema?: string; ok?: boolean };
      if (response.schema !== TASK_CHECKPOINT_PROTOCOL || response.ok !== true) {
        throw new Error("Task checkpoint helper returned an invalid activation response");
      }
    } catch (error) {
      await process.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }

  async #launch(taskId: string, operation: "capture" | "pending" | "activate") {
    return await this.launcher.launch({
      target: { kind: "task-capsule", taskId },
      executable: this.executable,
      args: ["_task-checkpoint", operation],
      cwd: "/workspace",
      env: { ALTITUDE_TASK_CHECKPOINT: "1" },
      io: "piped",
      purpose: `${operation[0]!.toUpperCase()}${operation.slice(1)} a portable task workspace checkpoint`,
    });
  }
}

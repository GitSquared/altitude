import type { CommandResult, CommandRunner, CommandSpec } from "../process/command.ts";
import { encodeTaskBootstrapPayload, TASK_BOOTSTRAP_PROTOCOL } from "../capsule/bootstrap.ts";
import type { CapsuleCheckpoint } from "../capsule/manifest.ts";
import { appleLaunchSpec, appleNetworkCreateSpec, appleVolumeCreateSpec } from "./apple-container.ts";
import { DEFAULT_CAPSULE_RESOURCES, type CapsuleLaunchSpec, type RuntimeKind } from "./capsule-runtime.ts";
import { dockerLaunchSpec, dockerNetworkCreateSpec, dockerVolumeCreateSpec } from "./docker.ts";
import { stableCapsuleName } from "./identity-capsule.ts";
import type { ProcessLauncher } from "./protocol.ts";

export interface TaskCapsuleSpec {
  readonly taskId: string;
  readonly image: string;
  readonly network: string;
}

export type TaskCapsuleEnsureResult = Readonly<{
  capsuleId: string;
  volume: string;
  state: "already_running" | "started" | "created";
}>;

export function taskVolumeName(taskId: string): string {
  return `${stableCapsuleName("task", taskId)}-data`;
}

function launchSpec(input: TaskCapsuleSpec): CapsuleLaunchSpec {
  return {
    capsuleId: stableCapsuleName("task", input.taskId),
    image: input.image,
    workspaceVolume: taskVolumeName(input.taskId),
    network: input.network,
    command: ["sleep", "infinity"],
    resources: DEFAULT_CAPSULE_RESOURCES,
    environment: { ALTITUDE_TASK_ID: input.taskId, HOME: "/tmp" },
  };
}

function alreadyExists(result: CommandResult): boolean {
  return /already exists|name is already in use|conflict.+container name/iu.test(`${result.stdout}\n${result.stderr}`);
}

function alreadyRunning(result: CommandResult): boolean {
  return /already running|is running/iu.test(`${result.stdout}\n${result.stderr}`);
}

export function taskVolumeInitializeCommand(runtime: RuntimeKind, spec: CapsuleLaunchSpec): CommandSpec {
  const common = [
    "run", "--rm", "--user", "0:0",
    ...(runtime === "apple-container"
      ? ["--volume", `${spec.workspaceVolume}:/workspace`]
      : ["--mount", `type=volume,src=${spec.workspaceVolume},dst=/workspace`]),
    "--env", "ALTITUDE_TASK_INIT=1",
    spec.image, "altitude", "_task-init",
  ];
  return { executable: runtime === "apple-container" ? "container" : "docker", args: common };
}

export class TaskCapsuleManager {
  constructor(
    private readonly runtime: RuntimeKind,
    private readonly runner: CommandRunner,
  ) {}

  async ensure(input: TaskCapsuleSpec): Promise<TaskCapsuleEnsureResult> {
    const spec = launchSpec(input);
    const volume = this.runtime === "apple-container" ? appleVolumeCreateSpec(spec) : dockerVolumeCreateSpec(spec);
    const network = this.runtime === "apple-container" ? appleNetworkCreateSpec(spec) : dockerNetworkCreateSpec(spec);
    await this.#allowExisting(volume, "Creating task volume");
    await this.#allowExisting(network, "Creating task network");
    await this.#required(taskVolumeInitializeCommand(this.runtime, spec), "Initializing task volume");
    if ((await this.runner.run(this.#health(spec.capsuleId))).exitCode === 0) {
      return { capsuleId: spec.capsuleId, volume: spec.workspaceVolume, state: "already_running" };
    }
    const launched = await this.runner.run(this.runtime === "apple-container" ? appleLaunchSpec(spec) : dockerLaunchSpec(spec));
    if (launched.exitCode === 0) {
      await this.#requireHealthy(spec.capsuleId);
      return { capsuleId: spec.capsuleId, volume: spec.workspaceVolume, state: "created" };
    }
    if (!alreadyExists(launched)) throw new Error(`Launching task capsule failed (${launched.exitCode}): ${launched.stderr.trim()}`);
    const started = await this.runner.run(this.#start(spec.capsuleId));
    if (started.exitCode !== 0 && !alreadyRunning(started)) {
      throw new Error(`Starting task capsule failed (${started.exitCode}): ${started.stderr.trim()}`);
    }
    await this.#requireHealthy(spec.capsuleId);
    return { capsuleId: spec.capsuleId, volume: spec.workspaceVolume, state: "started" };
  }

  #health(capsuleId: string): CommandSpec {
    return {
      executable: this.runtime === "apple-container" ? "container" : "docker",
      args: ["exec", "--user", "1000:1000", capsuleId, "/usr/bin/true"],
    };
  }

  #start(capsuleId: string): CommandSpec {
    return this.runtime === "apple-container"
      ? { executable: "container", args: ["start", capsuleId] }
      : { executable: "docker", args: ["container", "start", capsuleId] };
  }

  async #allowExisting(command: CommandSpec, action: string): Promise<void> {
    const result = await this.runner.run(command);
    if (result.exitCode !== 0 && !alreadyExists(result)) throw new Error(`${action} failed (${result.exitCode}): ${result.stderr.trim()}`);
  }

  async #required(command: CommandSpec, action: string): Promise<void> {
    const result = await this.runner.run(command);
    if (result.exitCode !== 0) throw new Error(`${action} failed (${result.exitCode}): ${result.stderr.trim()}`);
  }

  async #requireHealthy(capsuleId: string): Promise<void> {
    const result = await this.runner.run(this.#health(capsuleId));
    if (result.exitCode !== 0) throw new Error(`Task capsule ${capsuleId} did not become ready: ${result.stderr.trim()}`);
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

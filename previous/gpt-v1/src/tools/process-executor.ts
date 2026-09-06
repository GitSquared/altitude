import type { ProcessLauncher } from "../runtime/protocol.ts";
import {
  TASK_TOOL_PROTOCOL,
  type TaskToolExecutor,
  type TaskToolRequest,
  type TaskToolResponse,
} from "./protocol.ts";

const MAX_RESPONSE_BYTES = 3 * 1024 * 1024;

async function collect(stream: AsyncIterable<Uint8Array>, limit: number): Promise<string> {
  const decoder = new TextDecoder();
  let text = "";
  let size = 0;
  for await (const chunk of stream) {
    size += chunk.byteLength;
    if (size > limit) throw new Error("Task tool response exceeded its size limit");
    text += decoder.decode(chunk, { stream: true });
  }
  return `${text}${decoder.decode()}`;
}

function parseResponse(value: string): TaskToolResponse {
  const decoded = JSON.parse(value) as Partial<TaskToolResponse>;
  if (decoded.schema !== TASK_TOOL_PROTOCOL || typeof decoded.ok !== "boolean") {
    throw new Error("Task tool helper returned an unsupported response");
  }
  return decoded as TaskToolResponse;
}

export class ProcessTaskToolExecutor implements TaskToolExecutor {
  constructor(
    private readonly launcher: ProcessLauncher,
    private readonly executable = "altitude",
  ) {}

  async execute(taskId: string, request: TaskToolRequest): Promise<TaskToolResponse> {
    if (!taskId.trim()) throw new Error("Task id is required");
    const child = await this.launcher.launch({
      target: { kind: "task-capsule", taskId },
      executable: this.executable,
      args: ["_task-tool"],
      cwd: "/workspace",
      env: { ALTITUDE_TASK_TOOL: "1" },
      io: "piped",
      purpose: `Run ${request.operation} in the task capsule`,
    });
    try {
      await child.writeStdin(JSON.stringify(request));
      await child.closeStdin();
      const [exit, stdout, stderr] = await Promise.all([
        child.exited,
        collect(child.stdout, MAX_RESPONSE_BYTES),
        collect(child.stderr, 64 * 1024),
      ]);
      const response = parseResponse(stdout);
      if (exit.exitCode !== 0 && response.ok) {
        throw new Error(`Task tool exited ${exit.exitCode}: ${stderr.trim()}`);
      }
      return response;
    } catch (error) {
      await child.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }
}

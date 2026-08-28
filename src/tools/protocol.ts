export const TASK_TOOL_PROTOCOL = "altitude.task-tool/v1" as const;

export type TaskToolRequest =
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      operation: "read_file";
      path: string;
      startLine?: number | undefined;
      endLine?: number | undefined;
    }>
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      operation: "write_file";
      path: string;
      content: string;
      expectedSha256?: string | null | undefined;
      createDirectories?: boolean | undefined;
    }>
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      operation: "list_files";
      path?: string | undefined;
      depth?: number | undefined;
      limit?: number | undefined;
    }>
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      operation: "search_text";
      query: string;
      path?: string | undefined;
      glob?: string | undefined;
      limit?: number | undefined;
    }>
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      operation: "run_command";
      executable: string;
      args?: readonly string[] | undefined;
      cwd?: string | undefined;
      env?: Readonly<Record<string, string>> | undefined;
      timeoutMs?: number | undefined;
    }>
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      operation: "git_status";
    }>;

export type TaskToolResponse =
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      ok: true;
      result: unknown;
    }>
  | Readonly<{
      schema: typeof TASK_TOOL_PROTOCOL;
      ok: false;
      error: Readonly<{ code: string; message: string }>;
    }>;

export interface TaskToolExecutor {
  execute(taskId: string, request: TaskToolRequest): Promise<TaskToolResponse>;
}

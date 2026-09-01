import { useCallback, useEffect, useMemo, useState } from "react";
import type { ReadStream, WriteStream } from "node:tty";
import {
  Box,
  Text,
  render,
  useApp,
  useInput,
} from "ink";

import type { AttentionResolution } from "../core/events.ts";
import type { DecisionAttention, ReviewAttention, Task, TaskStatus } from "../core/model.ts";

export interface AltitudeTuiActions {
  configured(): Promise<boolean>;
  configureClaude(): Promise<void>;
  listTasks(): Promise<readonly Task[]>;
  createTask(intent: string): Promise<Task>;
  runTask(taskId: string): Promise<void>;
  takeTask(taskId: string): Promise<string | undefined>;
  openShell(taskId: string): Promise<string | undefined>;
  resolveAttention(
    taskId: string,
    attentionId: string,
    resolution: AttentionResolution,
  ): Promise<string | undefined>;
}

type TaskGroup = "Needs you" | "Working" | "Ready" | "Done";
type PromptState =
  | Readonly<{ kind: "new" }>
  | Readonly<{ kind: "decision"; task: Task; attention: DecisionAttention }>
  | Readonly<{ kind: "review"; task: Task; attention: ReviewAttention }>
  | Readonly<{ kind: "ack"; task: Task; attentionId: string; title: string; summary: string }>;

const GROUP_ORDER: readonly TaskGroup[] = ["Needs you", "Working", "Ready", "Done"];

function taskGroup(status: TaskStatus): TaskGroup {
  if (status === "waiting_decision" || status === "waiting_review") return "Needs you";
  if (status === "provisioning" || status === "running" || status === "waiting_external"
    || status === "taken" || status === "pausing" || status === "transferring") return "Working";
  if (status === "completed" || status === "archived") return "Done";
  return "Ready";
}

export function orderedTasks(tasks: readonly Task[]): readonly Task[] {
  return [...tasks].sort((left, right) => {
    const group = GROUP_ORDER.indexOf(taskGroup(left.status)) - GROUP_ORDER.indexOf(taskGroup(right.status));
    return group !== 0 ? group : right.updatedAt - left.updatedAt || left.id.localeCompare(right.id);
  });
}

function canTake(task: Task): boolean {
  return task.status === "paused" || task.status === "running"
    || task.status === "waiting_external" || task.status === "taken";
}

function Header({ subtitle }: { readonly subtitle: string }) {
  return (
    <Box flexDirection="column" marginBottom={1}>
      <Text bold color="cyan">ALTITUDE</Text>
      <Text dimColor>{subtitle}</Text>
    </Box>
  );
}

export function WelcomeScreen({ notice }: { readonly notice?: string }) {
  return (
    <Box flexDirection="column">
      <Header subtitle="delegated software factory" />
      <Text>Your agents work inside isolated capsules. They do not inherit this</Text>
      <Text>machine&apos;s files, tools, credentials, configuration, or current directory.</Text>
      <Text>You oversee the work; they operate the factory floor.</Text>
      <Box flexDirection="column" marginTop={1}>
        <Text bold>Choose a coding harness</Text>
        <Text color="cyan">  › Claude Code   <Text dimColor>managed, resumable, sandboxed</Text></Text>
      </Box>
      <Box marginTop={1}>
        <Text>Claude&apos;s native login will open inside a node-local identity capsule.</Text>
      </Box>
      {notice === undefined ? null : <Notice message={notice} />}
      <Box marginTop={1}><Text dimColor>enter continue   q quit</Text></Box>
    </Box>
  );
}

function Notice({ message }: { readonly message: string }) {
  return <Box marginTop={1}><Text color="yellow">! {message}</Text></Box>;
}

export function TaskBoardScreen(props: {
  readonly tasks: readonly Task[];
  readonly selectedIndex: number;
  readonly notice?: string;
  readonly busy?: boolean;
}) {
  const tasks = orderedTasks(props.tasks);
  return (
    <Box flexDirection="column">
      <Header subtitle="factory overview" />
      {tasks.length === 0 ? (
        <Box flexDirection="column">
          <Text>The factory is quiet.</Text>
          <Text>Press n to delegate your first task.</Text>
        </Box>
      ) : GROUP_ORDER.map((group) => {
        const grouped = tasks
          .map((task, index) => ({ task, index }))
          .filter(({ task }) => taskGroup(task.status) === group);
        if (grouped.length === 0) return null;
        return (
          <Box key={group} flexDirection="column" marginBottom={1}>
            <Text bold dimColor>{group.toUpperCase()}</Text>
            {grouped.map(({ task, index }) => (
              <Box key={task.id} flexDirection="column">
                <Text {...(index === props.selectedIndex ? { color: "cyan" as const } : {})}>
                  {index === props.selectedIndex ? "›" : " "} {task.title}
                </Text>
                <Text dimColor>
                  {"    "}{task.status}
                  {task.activeAttention === null ? "" : ` · ${task.activeAttention.class}`}
                  {task.openUpdates.length === 0 ? "" : ` · ${task.openUpdates.length} update${task.openUpdates.length === 1 ? "" : "s"}`}
                </Text>
                {task.activeAttention === null ? null : (
                  <Text color="yellow">    ↳ {task.activeAttention.title}: {task.activeAttention.summary}</Text>
                )}
              </Box>
            ))}
          </Box>
        );
      })}
      {props.notice === undefined ? null : <Notice message={props.notice} />}
      {props.busy ? <Box marginTop={1}><Text color="cyan">Working…</Text></Box> : null}
      <Box marginTop={1}>
        <Text dimColor>↑↓ select   enter open/respond   u ack update   r run   s setup   n new   q quit</Text>
      </Box>
    </Box>
  );
}

function TextEntry(props: {
  readonly label: string;
  readonly allowEmpty?: boolean;
  readonly onSubmit: (value: string) => void;
  readonly onCancel: () => void;
}) {
  const [value, setValue] = useState("");
  useInput((input, key) => {
    if (key.escape) return props.onCancel();
    if (key.return) {
      const normalized = value.trim();
      if (normalized || props.allowEmpty) props.onSubmit(normalized);
      return;
    }
    if (key.backspace || key.delete) {
      setValue((current) => [...current].slice(0, -1).join(""));
      return;
    }
    if (!key.ctrl && !key.meta && input) setValue((current) => current + input);
  });
  return (
    <Box flexDirection="column">
      <Text>{props.label}</Text>
      <Text color="cyan">› {value}<Text inverse> </Text></Text>
      <Text dimColor>enter submit   esc cancel</Text>
    </Box>
  );
}

function PromptScreen(props: {
  readonly prompt: PromptState;
  readonly onSubmit: (value: string) => void;
  readonly onCancel: () => void;
}) {
  const prompt = props.prompt;
  if (prompt.kind === "new") {
    return <TextEntry label="What would you like to delegate?" onSubmit={props.onSubmit} onCancel={props.onCancel} />;
  }
  if (prompt.kind === "ack") {
    return (
      <Box flexDirection="column">
        <Header subtitle="acknowledge update" />
        <Text bold>{prompt.title}</Text>
        <Text>{prompt.summary}</Text>
        <Box marginTop={1}>
          <TextEntry label="Optional acknowledgement note" allowEmpty onSubmit={props.onSubmit} onCancel={props.onCancel} />
        </Box>
      </Box>
    );
  }
  if (prompt.kind === "decision") {
    return (
      <Box flexDirection="column">
        <Header subtitle="decision" />
        <Text bold>{prompt.attention.title}</Text>
        <Text>{prompt.attention.summary}</Text>
        <Box marginTop={1} flexDirection="column">
          <Text>{prompt.attention.question}</Text>
          {prompt.attention.recommendation === undefined ? null : (
            <Text color="cyan">Recommendation: {prompt.attention.recommendation}</Text>
          )}
          {(prompt.attention.options ?? []).map((option) => (
            <Text key={option.value}>- {option.label}: {option.tradeoff ?? option.value}</Text>
          ))}
        </Box>
        <Box marginTop={1}>
          <TextEntry label="Your answer" onSubmit={props.onSubmit} onCancel={props.onCancel} />
        </Box>
      </Box>
    );
  }
  return (
    <Box flexDirection="column">
      <Header subtitle="review" />
      <Text bold>{prompt.attention.title}</Text>
      <Text>{prompt.attention.summary}</Text>
      <Box marginTop={1} flexDirection="column">
        <Text bold>Artifacts</Text>
        {prompt.attention.artifacts.map((artifact) => <Text key={artifact.uri}>- {artifact.label}: {artifact.uri}</Text>)}
        <Text bold>Checks</Text>
        {prompt.attention.checks.map((check) => <Text key={check}>- {check}</Text>)}
      </Box>
      <Box marginTop={1}>
        <TextEntry label={'Type "accept" or enter revision instructions'} onSubmit={props.onSubmit} onCancel={props.onCancel} />
      </Box>
    </Box>
  );
}

export function AltitudeApp({ actions }: { readonly actions: AltitudeTuiActions }) {
  const { exit, suspendTerminal } = useApp();
  const [phase, setPhase] = useState<"loading" | "welcome" | "board">("loading");
  const [tasks, setTasks] = useState<readonly Task[]>([]);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [notice, setNotice] = useState<string>();
  const [prompt, setPrompt] = useState<PromptState>();
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async (message?: string) => {
    const next = orderedTasks(await actions.listTasks());
    setTasks(next);
    setSelectedIndex((index) => next.length === 0 ? 0 : Math.min(index, next.length - 1));
    setNotice(message);
    setPrompt(undefined);
    setPhase("board");
  }, [actions]);

  useEffect(() => {
    void (async () => {
      try {
        if (!(await actions.configured())) {
          setPhase("welcome");
          return;
        }
        const restored = await actions.listTasks();
        const resumable = restored.filter((task) => task.status === "paused").length;
        const scheduled = restored.filter((task) => task.status === "waiting_external").length;
        const message = resumable > 0
          ? `Restored ${resumable} checkpointed task${resumable === 1 ? "" : "s"}. Want work to continue after Altitude closes? Add an Iroh node.`
          : scheduled > 0
            ? `Resumed local workers for ${scheduled} scheduled task${scheduled === 1 ? "" : "s"}. Keep Altitude open, or add an Iroh node.`
            : undefined;
        await refresh(message);
      } catch (error) {
        setNotice(error instanceof Error ? error.message : String(error));
        setPhase("welcome");
      }
    })();
  }, [actions, refresh]);

  const runExternal = useCallback(async (action: () => Promise<string | undefined>) => {
    if (busy) return;
    setBusy(true);
    let message: string | undefined;
    let failed = false;
    try {
      await suspendTerminal(async () => {
        message = await action();
      });
    } catch (error) {
      failed = true;
      message = error instanceof Error ? error.message : String(error);
    } finally {
      setBusy(false);
    }
    if (failed && phase === "welcome") {
      setNotice(message);
      return;
    }
    await refresh(message);
  }, [busy, phase, refresh, suspendTerminal]);

  const selected = useMemo(() => orderedTasks(tasks)[selectedIndex], [selectedIndex, tasks]);

  useInput((input, key) => {
    if (prompt !== undefined || busy || phase === "loading") return;
    if (input === "q" || key.escape) return exit();
    if (phase === "welcome") {
      if (key.return) {
        void runExternal(async () => {
          await actions.configureClaude();
          return undefined;
        });
      }
      return;
    }
    if ((key.upArrow || input === "k") && tasks.length > 0) {
      setSelectedIndex((index) => (index + tasks.length - 1) % tasks.length);
      return;
    }
    if ((key.downArrow || input === "j") && tasks.length > 0) {
      setSelectedIndex((index) => (index + 1) % tasks.length);
      return;
    }
    if (input === "n") return setPrompt({ kind: "new" });
    if (selected === undefined) return;
    if (input === "s") {
      void runExternal(async () => await actions.openShell(selected.id));
      return;
    }
    if (input === "u") {
      const update = selected.openUpdates[0];
      setNotice(undefined);
      if (update === undefined) setNotice("This task has no open Update.");
      else setPrompt({
        kind: "ack",
        task: selected,
        attentionId: update.id,
        title: update.title,
        summary: update.summary,
      });
      return;
    }
    if (key.return && selected.activeAttention?.class === "decision") {
      setPrompt({ kind: "decision", task: selected, attention: selected.activeAttention });
      return;
    }
    if (key.return && selected.activeAttention?.class === "review") {
      setPrompt({ kind: "review", task: selected, attention: selected.activeAttention });
      return;
    }
    if (key.return || input === "r") {
      if (selected.status === "waiting_decision" || selected.status === "waiting_review") {
        setNotice("Respond with enter before starting more work.");
        return;
      }
      void runExternal(async () => {
        if (key.return && canTake(selected)) return await actions.takeTask(selected.id);
        await actions.runTask(selected.id);
        return undefined;
      });
    }
  });

  const submitPrompt = useCallback((value: string) => {
    const active = prompt;
    if (active === undefined) return;
    void runExternal(async () => {
      if (active.kind === "new") {
        const created = await actions.createTask(value);
        return await actions.takeTask(created.id);
      }
      if (active.kind === "decision") {
        return await actions.resolveAttention(
          active.task.id,
          active.attention.id,
          { kind: "answered", answer: value },
        );
      }
      if (active.kind === "review") {
        const resolution: AttentionResolution = value.toLowerCase() === "accept"
          ? { kind: "accepted" }
          : { kind: "revision_requested", instructions: value };
        return await actions.resolveAttention(active.task.id, active.attention.id, resolution);
      }
      return await actions.resolveAttention(active.task.id, active.attentionId, {
        kind: "acknowledged",
        ...(value ? { note: value } : {}),
      });
    });
  }, [actions, prompt, runExternal]);

  if (phase === "loading") return <Text color="cyan">Starting Altitude…</Text>;
  if (prompt !== undefined) {
    return <PromptScreen prompt={prompt} onSubmit={submitPrompt} onCancel={() => setPrompt(undefined)} />;
  }
  if (phase === "welcome") return <WelcomeScreen {...(notice === undefined ? {} : { notice })} />;
  return (
    <TaskBoardScreen
      tasks={tasks}
      selectedIndex={selectedIndex}
      busy={busy}
      {...(notice === undefined ? {} : { notice })}
    />
  );
}

export async function runAltitudeTui(
  actions: AltitudeTuiActions,
  io: {
    readonly stdin?: ReadStream;
    readonly stdout?: WriteStream;
    readonly stderr?: WriteStream;
  } = {},
): Promise<number> {
  const stdin = io.stdin ?? process.stdin;
  const stdout = io.stdout ?? process.stdout;
  const stderr = io.stderr ?? process.stderr;
  if (stdin.isTTY !== true || stdout.isTTY !== true) {
    throw new Error("Altitude's TUI requires an interactive terminal");
  }
  stdin.resume();
  const instance = render(<AltitudeApp actions={actions} />, {
    stdin,
    stdout,
    stderr,
    exitOnCtrlC: true,
  });
  await instance.waitUntilExit();
  return 0;
}

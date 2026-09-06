import { useCallback, useEffect, useMemo, useState } from "react";
import type { ReadStream, WriteStream } from "node:tty";
import {
  Box,
  Text,
  render,
  useApp,
  useInput,
  useStdin,
  useWindowSize,
} from "ink";

import type { AttentionResolution } from "../core/events.ts";
import type { DecisionAttention, ReviewAttention, Task, TaskStatus } from "../core/model.ts";
import { withInteractiveInput } from "../process/command.ts";

export interface AltitudeTuiActions {
  configured(): Promise<boolean>;
  configureClaude(): Promise<void>;
  resumeInterruptedTasks(): Promise<Readonly<{ resumed: number; deferred: number }>>;
  listTasks(): Promise<readonly Task[]>;
  createTask(intent: string): Promise<Task>;
  delegateTask(taskId: string): Promise<string | undefined>;
  runTask(taskId: string): Promise<void>;
  takeTask(taskId: string): Promise<string | undefined>;
  openShell(taskId: string): Promise<string | undefined>;
  reviewTask(taskId: string, targetUrl: string): Promise<string | undefined>;
  resolveAttention(
    taskId: string,
    attentionId: string,
    resolution: AttentionResolution,
  ): Promise<string | undefined>;
}

export async function delegateNewTask(
  actions: Pick<AltitudeTuiActions, "createTask" | "delegateTask">,
  intent: string,
): Promise<string | undefined> {
  const created = await actions.createTask(intent);
  return await actions.delegateTask(created.id);
}

type TaskGroup = "Needs you" | "Working" | "Ready" | "Done";
type PromptState =
  | Readonly<{ kind: "new" }>
  | Readonly<{ kind: "visual"; task: Task }>
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

export function startupNotice(
  restart: Readonly<{ resumed: number; deferred: number }>,
  tasks: readonly Task[],
): string | undefined {
  if (restart.resumed > 0) {
    return `Resumed ${restart.resumed} task${restart.resumed === 1 ? "" : "s"} interrupted when Altitude closed. Want them to keep working while it is closed? Add an Iroh node.`;
  }
  if (restart.deferred > 0) {
    return `${restart.deferred} interrupted task${restart.deferred === 1 ? "" : "s"} could not resume yet; Altitude will retry next time it opens.`;
  }
  const scheduled = tasks.filter((task) => task.status === "waiting_external").length;
  return scheduled > 0
    ? `Resumed local workers for ${scheduled} scheduled task${scheduled === 1 ? "" : "s"}. Keep Altitude open, or add an Iroh node.`
    : undefined;
}

function Header(props: {
  readonly subtitle: string;
  readonly meta?: string;
}) {
  return (
    <Box justifyContent="space-between" marginBottom={1}>
      <Text><Text bold color="cyan">ALTITUDE</Text> <Text dimColor>{props.subtitle}</Text></Text>
      {props.meta === undefined ? null : <Text dimColor>{props.meta}</Text>}
    </Box>
  );
}

type ScreenDimensions = Readonly<{
  width?: number;
  height?: number;
}>;

function screenBox(dimensions: ScreenDimensions): Readonly<{ width?: number; height?: number }> {
  return {
    ...(dimensions.width === undefined ? {} : { width: dimensions.width }),
    ...(dimensions.height === undefined ? {} : { height: dimensions.height }),
  };
}

export function WelcomeScreen(props: ScreenDimensions & { readonly notice?: string }) {
  return (
    <Box flexDirection="column" paddingX={1} paddingY={1} {...screenBox(props)}>
      <Header subtitle="delegated software factory" />
      <Box borderStyle="round" borderColor="cyan" flexDirection="column" paddingX={2} paddingY={1}>
        <Text bold>Welcome above the factory floor.</Text>
        <Box marginTop={1} flexDirection="column">
          <Text>Your agents work inside isolated capsules. They do not inherit this</Text>
          <Text>machine&apos;s files, tools, credentials, configuration, or current directory.</Text>
          <Text>You oversee the work; they operate the factory floor.</Text>
        </Box>
        <Box flexDirection="column" marginTop={1}>
          <Text bold>Choose a coding harness</Text>
          <Text color="cyan">  › Claude Code   <Text dimColor>managed, resumable, sandboxed</Text></Text>
        </Box>
        <Box marginTop={1} flexDirection="column">
          <Text>Your first real task opens interactively inside the persistent identity capsule.</Text>
          <Text>Claude can sign in there; later tasks delegate in the background.</Text>
        </Box>
      </Box>
      {props.notice === undefined ? null : <Notice message={props.notice} />}
      <Box flexGrow={1} />
      <Text dimColor>enter continue   q quit</Text>
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
} & ScreenDimensions) {
  const tasks = orderedTasks(props.tasks);
  const selected = tasks[props.selectedIndex];
  const needsYou = tasks.filter((task) => taskGroup(task.status) === "Needs you").length;
  const working = tasks.filter((task) => taskGroup(task.status) === "Working").length;
  const availableRows = Math.max(3, (props.height ?? 24) - 10);
  const firstVisible = Math.max(
    0,
    Math.min(props.selectedIndex - Math.floor(availableRows / 2), tasks.length - availableRows),
  );
  const visible = tasks.slice(firstVisible, firstVisible + availableRows);
  const wide = (props.width ?? 100) >= 88;

  return (
    <Box flexDirection="column" paddingX={1} {...screenBox(props)}>
      <Header
        subtitle="factory overview"
        meta={`${tasks.length} capsule${tasks.length === 1 ? "" : "s"} · ${working} working · ${needsYou} need you`}
      />
      <Box flexDirection={wide ? "row" : "column"} flexGrow={1} overflow="hidden">
        <TaskList
          tasks={tasks}
          visible={visible}
          selectedIndex={props.selectedIndex}
          firstVisible={firstVisible}
          wide={wide}
        />
        <TaskDetails task={selected} wide={wide} />
      </Box>
      {props.notice === undefined ? null : <Notice message={props.notice} />}
      <BoardFooter busy={props.busy === true} wide={wide} />
    </Box>
  );
}

function TaskList(props: {
  readonly tasks: readonly Task[];
  readonly visible: readonly Task[];
  readonly selectedIndex: number;
  readonly firstVisible: number;
  readonly wide: boolean;
}) {
  return (
    <Box
      borderStyle="round"
      borderColor="gray"
      flexDirection="column"
      flexGrow={1}
      paddingX={1}
      width={props.wide ? "44%" : "100%"}
      {...(props.wide ? { marginRight: 1 } : {})}
      overflow="hidden"
    >
      <Text bold>CAPSULES</Text>
      {props.tasks.length === 0 ? (
        <Box flexDirection="column" marginTop={1}>
          <Text>The factory is quiet.</Text>
          <Text dimColor>Press n to delegate your first task.</Text>
        </Box>
      ) : (
        <>
          {props.firstVisible > 0 ? <Text dimColor>↑ {props.firstVisible} more</Text> : null}
          {GROUP_ORDER.map((group) => {
            const grouped = props.visible.filter((task) => taskGroup(task.status) === group);
            if (grouped.length === 0) return null;
            const total = props.tasks.filter((task) => taskGroup(task.status) === group).length;
            return (
              <Box key={group} flexDirection="column" marginTop={1}>
                <Text bold dimColor>{group.toUpperCase()}  {total}</Text>
                {grouped.map((task) => {
                  const index = props.tasks.indexOf(task);
                  const selected = index === props.selectedIndex;
                  return (
                    <Text
                      key={task.id}
                      {...(selected ? { color: "cyan" as const } : {})}
                      bold={selected}
                      wrap="truncate-end"
                    >
                      {selected ? "›" : " "} {task.id}  {task.title}
                    </Text>
                  );
                })}
              </Box>
            );
          })}
          {props.firstVisible + props.visible.length < props.tasks.length ? (
            <Text dimColor>↓ {props.tasks.length - props.firstVisible - props.visible.length} more</Text>
          ) : null}
        </>
      )}
    </Box>
  );
}

function BoardFooter({ busy, wide }: { readonly busy: boolean; readonly wide: boolean }) {
  const state = <Text {...(busy ? { color: "cyan" as const } : {})}>{busy ? "● working" : "● ready"}</Text>;
  if (wide) {
    return (
      <>
        <Box justifyContent="space-between">
          {state}
          <Text dimColor>↑↓ select   enter open/respond   n new   s setup   v visual review   q quit</Text>
        </Box>
        <Text dimColor>inside a capsule: ctrl-a returns to Altitude without stopping the work</Text>
      </>
    );
  }
  return (
    <Box flexDirection="column">
      <Box>{state}<Text dimColor>   ↑↓ select   enter open/respond   n new</Text></Box>
      <Text dimColor>s setup   v visual review   q quit   ctrl-a back from capsule</Text>
    </Box>
  );
}

function statusColor(status: TaskStatus): "cyan" | "green" | "red" | "yellow" | "gray" {
  if (status === "running" || status === "taken" || status === "provisioning") return "cyan";
  if (status === "completed") return "green";
  if (status === "failed" || status === "stopped") return "red";
  if (status === "waiting_decision" || status === "waiting_review") return "yellow";
  return "gray";
}

function TaskDetails({ task, wide }: { readonly task: Task | undefined; readonly wide: boolean }) {
  if (!wide) return task === undefined ? null : (
    <Box borderStyle="round" borderColor="cyan" flexDirection="column" paddingX={1} marginTop={1} height={7}>
      <Text bold color="cyan">{task.id}</Text>
      <Text wrap="truncate-end">{task.title}</Text>
      <Text color={statusColor(task.status)}>{task.status} · {task.intentLevel}</Text>
      {task.activeAttention === null
        ? <Text dimColor>No blocking attention.</Text>
        : <Text color="yellow" wrap="truncate-end">{task.activeAttention.class}: {task.activeAttention.title}</Text>}
    </Box>
  );
  return (
    <Box borderStyle="round" borderColor="cyan" flexDirection="column" paddingX={2} flexGrow={1} overflow="hidden">
      {task === undefined ? (
        <Box flexDirection="column">
          <Text bold>NO CAPSULE SELECTED</Text>
          <Text dimColor>Delegate a task to start the factory.</Text>
        </Box>
      ) : (
        <>
          <Box justifyContent="space-between">
            <Text bold color="cyan">{task.id}</Text>
            <Text color={statusColor(task.status)}>{task.status}</Text>
          </Box>
          <Box marginTop={1} flexDirection="column">
            <Text bold>{task.title}</Text>
            <Text dimColor>intent level · {task.intentLevel.replaceAll("_", " ")}</Text>
          </Box>
          <Box marginTop={1} flexDirection="column">
            <Text bold>INTENT</Text>
            <Text>{task.intent}</Text>
          </Box>
          <Box marginTop={1} flexDirection="column">
            <Text bold>ATTENTION</Text>
            {task.activeAttention === null ? (
              <Text dimColor>No blocking attention. The agent can keep moving.</Text>
            ) : (
              <>
                <Text color="yellow" bold>{task.activeAttention.class.toUpperCase()} · {task.activeAttention.title}</Text>
                <Text>{task.activeAttention.summary}</Text>
              </>
            )}
            {task.openUpdates.length === 0 ? null : (
              <Text color="cyan">{task.openUpdates.length} open update{task.openUpdates.length === 1 ? "" : "s"}</Text>
            )}
          </Box>
          <Box flexGrow={1} />
          <Text dimColor>{task.activeAttention === null ? "enter look in · r run headless" : "enter respond"}</Text>
        </>
      )}
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
} & ScreenDimensions) {
  const prompt = props.prompt;
  if (prompt.kind === "new") {
    return (
      <Box flexDirection="column" paddingX={1} paddingY={1} {...screenBox(props)}>
        <Header subtitle="new delegation" />
        <Box borderStyle="round" borderColor="cyan" flexDirection="column" paddingX={2} paddingY={1}>
          <TextEntry label="What would you like to delegate?" onSubmit={props.onSubmit} onCancel={props.onCancel} />
        </Box>
      </Box>
    );
  }
  if (prompt.kind === "visual") {
    return (
      <Box flexDirection="column">
        <Header subtitle="visual review" />
        <Text bold>{prompt.task.title}</Text>
        <Text>Redmarker will open over an already-running local preview.</Text>
        <Box marginTop={1}>
          <TextEntry
            label="Preview origin (for example http://localhost:4173)"
            onSubmit={props.onSubmit}
            onCancel={props.onCancel}
          />
        </Box>
      </Box>
    );
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
  const { stdin } = useStdin();
  const { columns, rows } = useWindowSize();
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
      let configured = false;
      try {
        configured = await actions.configured();
        if (!configured) {
          setPhase("welcome");
          return;
        }
        const restart = await actions.resumeInterruptedTasks();
        const restored = await actions.listTasks();
        await refresh(startupNotice(restart, restored));
      } catch (error) {
        setNotice(error instanceof Error ? error.message : String(error));
        setPhase(configured ? "board" : "welcome");
      }
    })();
  }, [actions, refresh]);

  useEffect(() => {
    if (phase !== "board" || busy || prompt !== undefined) return;
    const timer = setInterval(() => {
      void actions.listTasks().then((next) => {
        const ordered = orderedTasks(next);
        setTasks(ordered);
        setSelectedIndex((index) => ordered.length === 0 ? 0 : Math.min(index, ordered.length - 1));
      }).catch(() => undefined);
    }, 1_000);
    return () => clearInterval(timer);
  }, [actions, busy, phase, prompt]);

  const runExternal = useCallback(async (action: () => Promise<string | undefined>) => {
    if (busy) return false;
    setBusy(true);
    let message: string | undefined;
    let failed = false;
    try {
      await suspendTerminal(async () => {
        message = await withInteractiveInput(stdin, action);
      });
    } catch (error) {
      failed = true;
      message = error instanceof Error ? error.message : String(error);
    } finally {
      setBusy(false);
    }
    if (failed && phase === "welcome") {
      setNotice(message);
      return false;
    }
    await refresh(message);
    return !failed;
  }, [busy, phase, refresh, stdin, suspendTerminal]);

  const runInFactory = useCallback(async (action: () => Promise<string | undefined>) => {
    if (busy) return false;
    setBusy(true);
    let message: string | undefined;
    let failed = false;
    try {
      message = await action();
    } catch (error) {
      failed = true;
      message = error instanceof Error ? error.message : String(error);
    } finally {
      setBusy(false);
    }
    await refresh(message);
    return !failed;
  }, [busy, refresh]);

  const selected = useMemo(() => orderedTasks(tasks)[selectedIndex], [selectedIndex, tasks]);

  useInput((input, key) => {
    if (prompt !== undefined || busy || phase === "loading") return;
    if (input === "q" || key.escape) return exit();
    if (phase === "welcome") {
      if (key.return) {
        void (async () => {
          const configured = await runExternal(async () => {
            await actions.configureClaude();
            return undefined;
          });
          if (configured) setPrompt({ kind: "new" });
        })();
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
    if (input === "v") {
      setNotice(undefined);
      setPrompt({ kind: "visual", task: selected });
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
    if (key.return) {
      if (selected.status === "waiting_decision" || selected.status === "waiting_review") {
        setNotice("Respond with enter before starting more work.");
        return;
      }
      void runExternal(async () => await actions.takeTask(selected.id));
      return;
    }
    if (input === "r") {
      void runInFactory(async () => {
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
        return await delegateNewTask(actions, value);
      }
      if (active.kind === "visual") {
        return await actions.reviewTask(active.task.id, value);
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
    return (
      <PromptScreen
        prompt={prompt}
        onSubmit={submitPrompt}
        onCancel={() => setPrompt(undefined)}
        width={columns}
        height={rows}
      />
    );
  }
  if (phase === "welcome") {
    return <WelcomeScreen width={columns} height={rows} {...(notice === undefined ? {} : { notice })} />;
  }
  return (
    <TaskBoardScreen
      tasks={tasks}
      selectedIndex={selectedIndex}
      busy={busy}
      width={columns}
      height={rows}
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
    alternateScreen: true,
    exitOnCtrlC: true,
  });
  await instance.waitUntilExit();
  return 0;
}

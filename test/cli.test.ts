import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EndpointAddr, EndpointTicket, SecretKey } from "@number0/iroh";

import type { CliOptions } from "../src/cli/main.ts";
import { runCli } from "../src/cli/main.ts";
import { TaskStore } from "../src/storage/task-store.ts";
import { defaultLocalConfig, writeLocalConfig } from "../src/config/local.ts";
import { TerminalSessionManager } from "../src/terminal/session.ts";
import {
  ALTITUDE_NODE_TICKET_PROTOCOL,
  encodeAltitudeNodeTicket,
  type AltitudeNodeClient,
} from "../src/node/iroh.ts";

const temporaryDirectories: string[] = [];

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

function harness(): {
  readonly root: string;
  readonly stdout: { value: string; write(chunk: string | Uint8Array): boolean };
  readonly stderr: { value: string; write(chunk: string | Uint8Array): boolean };
  readonly options: CliOptions;
} {
  const root = mkdtempSync(join(tmpdir(), "altitude-cli-test-"));
  temporaryDirectories.push(root);
  const output = () => ({
    value: "",
    write(this: { value: string }, chunk: string | Uint8Array) {
      this.value += typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk);
      return true;
    },
  });
  const stdout = output();
  const stderr = output();
  let id = 0;
  return {
    root,
    stdout,
    stderr,
    options: {
      env: { ALTITUDE_HOME: root },
      stdout,
      stderr,
      clock: () => Date.parse("2026-08-28T12:00:00.000Z"),
      createId: () => `id-${++id}`,
    },
  };
}

function nodeTicket(nodeId: string): string {
  const endpointId = SecretKey.generate().public();
  return encodeAltitudeNodeTicket({
    schema: ALTITUDE_NODE_TICKET_PROTOCOL,
    nodeId,
    endpointId: endpointId.toString(),
    endpointTicket: EndpointTicket.fromAddr(new EndpointAddr(endpointId)).toString(),
    pairingToken: "a".repeat(43),
  });
}

describe("CLI", () => {
  test("bare altitude opens the capsule-first factory overview", async () => {
    const cli = harness();
    let configured: boolean | undefined;
    expect(await runCli([], {
      ...cli.options,
      startTui: async (actions) => {
        configured = await actions.configured();
        return 0;
      },
    })).toBe(0);
    expect(configured).toBeFalse();
  });

  test("bare altitude fails clearly instead of hanging without a terminal", async () => {
    const cli = harness();
    expect(await runCli([], cli.options)).toBe(2);
    expect(cli.stdout.value).toContain("requires an interactive terminal");
    expect(cli.stdout.value).toContain("task create <intent>");
  });

  test("configured bare altitude owns and closes the local control plane", async () => {
    const cli = harness();
    const base = defaultLocalConfig();
    await writeLocalConfig(join(cli.root, "config.json"), base);
    let starts = 0;
    let closes = 0;
    expect(await runCli([], {
      ...cli.options,
      startTui: async (actions) => {
        expect(await actions.resumeInterruptedTasks()).toEqual({ resumed: 0, deferred: 0 });
        await actions.listTasks();
        return 0;
      },
      startTuiControlPlane: async () => {
        starts += 1;
        return { close: async () => { closes += 1; } };
      },
    })).toBe(0);
    expect(starts).toBe(1);
    expect(closes).toBe(1);
  });

  test("documents sandboxed first-run setup without touching a runtime", async () => {
    const cli = harness();
    expect(await runCli(["setup", "help"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("Microsandbox");
    expect(cli.stdout.value).toContain("private\nidentity capsule");
    expect(cli.stdout.value).toContain("never\nimports credentials");
  });

  test("reads and changes only explicit non-secret configuration", async () => {
    const cli = harness();
    expect(await runCli(["config", "help"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("identity.id");
    expect(cli.stdout.value).toContain("credentials stay inside");
    expect(cli.stdout.value).not.toContain("apiKey");

    cli.stdout.value = "";
    expect(await runCli(["config", "path"], cli.options)).toBe(0);
    expect(cli.stdout.value.trim()).toBe(join(cli.root, "config.json"));
    await writeLocalConfig(join(cli.root, "config.json"), defaultLocalConfig());

    cli.stdout.value = "";
    expect(await runCli(["config", "set", "node.port", "43210", "--json"], cli.options)).toBe(0);
    expect(JSON.parse(cli.stdout.value)).toEqual({ key: "node.port", value: 43210 });

    cli.stdout.value = "";
    expect(await runCli(["config", "get", "node.port"], cli.options)).toBe(0);
    expect(cli.stdout.value).toBe("node.port = 43210\n");

    cli.stderr.value = "";
    expect(await runCli(["config", "set", "claude.apiKey", "secret"], cli.options)).toBe(2);
    expect(cli.stderr.value).toContain("Unknown config key: claude.apiKey");
    expect(cli.stderr.value).not.toContain("claude.apiKey = secret");
  });

  test("documents the headless node listener without starting it", async () => {
    const cli = harness();
    expect(await runCli(["node", "help"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("altitude node serve");
    expect(cli.stdout.value).toContain("loopback");
    expect(cli.stdout.value).toContain("node add <id>");
  });

  test("binds a task to Linear without putting webhook secrets in config", async () => {
    const cli = harness();
    expect(await runCli(["task", "create", "Project intent through Linear", "--json"], cli.options)).toBe(0);
    const taskId = JSON.parse(cli.stdout.value).id as string;

    cli.stdout.value = "";
    expect(await runCli([
      "tracker",
      "linear",
      "bind",
      taskId,
      "--issue-id",
      "issue-1",
      "--issue-identifier",
      "ALT-1",
      "--agent-session-id",
      "session-1",
      "--app-user-id",
      "app-user-1",
      "--json",
    ], {
      ...cli.options,
      env: {
        ...cli.options.env,
        ALTITUDE_LINEAR_WEBHOOK_SECRET: "must-stay-in-process-memory",
      },
    })).toBe(0);
    expect(JSON.parse(cli.stdout.value)).toMatchObject({
      taskId,
      issueId: "issue-1",
      issueIdentifier: "ALT-1",
      agentSessionId: "session-1",
      appUserId: "app-user-1",
      mentionTokens: ["@altitude"],
    });

    cli.stdout.value = "";
    expect(await runCli(["tracker", "linear", "show", taskId], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("ALT-1 via Agent Session session-1");
    expect(() => readFileSync(join(cli.root, "config.json"), "utf8")).toThrow();
    expect(readFileSync(join(cli.root, "altitude.db")).includes(
      Buffer.from("must-stay-in-process-memory"),
    )).toBeFalse();
  });

  test("remembers and checks a remote node through its Iroh pairing ticket", async () => {
    const cli = harness();
    const ticket = nodeTicket("straylight");
    expect(await runCli([
      "node",
      "add",
      "straylight",
      "--ticket",
      ticket,
      "--json",
    ], cli.options)).toBe(0);
    expect(JSON.parse(cli.stdout.value)).toMatchObject({
      connection: {
        nodeId: "straylight",
        ticket,
      },
    });

    cli.stdout.value = "";
    expect(await runCli(["node", "list"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("straylight  iroh:");

    let closed = false;
    const client: AltitudeNodeClient = {
      async health() {
        return { nodeId: "straylight", version: "0.0.0" };
      },
      async prepare() { throw new Error("not used"); },
      async activate() { throw new Error("not used"); },
      async close() { closed = true; },
    };
    cli.stdout.value = "";
    expect(await runCli([
      "node",
      "check",
      "straylight",
      "--json",
    ], { ...cli.options, createNodeClient: async () => client })).toBe(0);
    expect(JSON.parse(cli.stdout.value)).toEqual({
      nodeId: "straylight",
      version: "0.0.0",
    });
    expect(closed).toBeTrue();
  });

  test("documents the capsule-first run and native take flow", async () => {
    const cli = harness();
    expect(await runCli(["task", "help"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("task run <task-id> [--repository <path>]");
    expect(cli.stdout.value).toContain("clean Altitude-created Git capsule");
    expect(cli.stdout.value).toContain("never\ninfers or mounts the current directory");
    expect(cli.stdout.value).toContain("task take <task-id>");
    expect(cli.stdout.value).toContain("task shell <task-id>");
    expect(cli.stdout.value).toContain("task review <task-id> --url <loopback-url>");
    expect(cli.stdout.value).toContain("task send <task-id> --node <registered-id>");
  });

  test("persists create, queue, list, show, and status across invocations", async () => {
    const cli = harness();
    expect(await runCli([
      "task",
      "create",
      "Build",
      "the",
      "smallest",
      "useful",
      "harness",
      "--title",
      "Useful Altitude",
      "--level",
      "architecture_sketch",
      "--json",
    ], cli.options)).toBe(0);
    const created = JSON.parse(cli.stdout.value) as { id: string; title: string; intentLevel: string };
    expect(created).toMatchObject({
      id: "id-1",
      title: "Useful Altitude",
      intentLevel: "architecture_sketch",
    });

    cli.stdout.value = "";
    expect(await runCli(["task", "queue", created.id], cli.options)).toBe(0);
    expect(await runCli(["task", "queue", created.id], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("Queued id-1");

    cli.stdout.value = "";
    expect(await runCli(["task", "list", "--status", "queued", "--json"], cli.options)).toBe(0);
    expect(JSON.parse(cli.stdout.value)).toHaveLength(1);

    cli.stdout.value = "";
    expect(await runCli(["task", "show", created.id, "--json"], cli.options)).toBe(0);
    const shown = JSON.parse(cli.stdout.value) as { task: { status: string }; events: unknown[] };
    expect(shown.task.status).toBe("queued");
    expect(shown.events).toHaveLength(2);

    cli.stdout.value = "";
    expect(await runCli(["status", "--json"], cli.options)).toBe(0);
    expect(JSON.parse(cli.stdout.value)).toMatchObject({ total: 1, byStatus: { queued: 1 } });
  });

  test("resolves Decisions and Reviews through their additive attention semantics", async () => {
    const cli = harness();
    await runCli(["task", "create", "Build it", "--json"], cli.options);
    const taskId = (JSON.parse(cli.stdout.value) as { id: string }).id;
    const store = new TaskStore(join(cli.root, "altitude.db"), {
      clock: () => 1_000,
      createId: (() => {
        let id = 100;
        return () => `event-${++id}`;
      })(),
    });
    for (const to of ["queued", "provisioning", "running"] as const) {
      store.append({
        taskId,
        type: "task.state_changed",
        payload: { to },
        actor: { kind: "system", component: "test" },
      });
    }
    store.append({
      taskId,
      type: "attention.raised",
      payload: {
        attention: {
          id: "decision-1",
          class: "decision",
          title: "Choose a runtime",
          summary: "The portable boundary needs one local default.",
          question: "Which capsule image should we use?",
        },
      },
      actor: { kind: "agent", adapter: "test" },
    });
    store.close();

    cli.stdout.value = "";
    expect(await runCli(["attention", "answer", taskId, "Use", "Apple", "container"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("task is running");

    const reviewStore = new TaskStore(join(cli.root, "altitude.db"), {
      clock: () => 2_000,
      createId: () => crypto.randomUUID(),
    });
    reviewStore.append({
      taskId,
      type: "attention.raised",
      payload: {
        attention: {
          id: "review-1",
          class: "review",
          title: "Review the slice",
          summary: "The vertical slice is ready.",
          artifacts: [{ kind: "pull_request", label: "PR", uri: "https://example.test/pr/1" }],
          checks: ["bun test"],
        },
      },
      actor: { kind: "agent", adapter: "test" },
    });
    reviewStore.close();

    cli.stdout.value = "";
    expect(await runCli(["attention", "accept", taskId, "Looks", "good"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("task is completed");
  });

  test("routes a resolved Decision into an already-running native Claude terminal", async () => {
    const cli = harness();
    await writeLocalConfig(join(cli.root, "config.json"), defaultLocalConfig());
    await runCli(["task", "create", "Route an answer", "--json"], cli.options);
    const taskId = (JSON.parse(cli.stdout.value) as { id: string }).id;
    using store = new TaskStore(join(cli.root, "altitude.db"), {
      clock: () => 3_000,
      createId: () => crypto.randomUUID(),
    });
    for (const to of ["queued", "provisioning", "running"] as const) {
      store.append({
        taskId,
        type: "task.state_changed",
        payload: { to },
        actor: { kind: "system", component: "test" },
      });
    }
    store.append({
      taskId,
      type: "attention.raised",
      payload: {
        attention: {
          id: "decision-live",
          class: "decision",
          title: "Choose",
          summary: "Claude is waiting",
          question: "Continue?",
        },
      },
      actor: { kind: "agent", adapter: "test" },
    });

    const terminals = new TerminalSessionManager(join(cli.root, "terminals"), [
      process.execPath,
      join(import.meta.dir, "..", "src", "cli.ts"),
    ]);
    expect(await terminals.start({
      taskId,
      kind: "agent",
      command: {
        executable: "/bin/sh",
        args: ["-c", "IFS= read -r line; case \"$line\" in 'Altitude recorded'*) exit 0;; *) exit 1;; esac"],
      },
    })).toMatchObject({ state: "running" });

    cli.stdout.value = "";
    expect(await runCli(["attention", "answer", taskId, "Yes"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("routed into the live Claude session");
    let status = await terminals.status(taskId, "agent");
    for (let attempt = 0; attempt < 100 && status?.state === "running"; attempt += 1) {
      await Bun.sleep(10);
      status = await terminals.status(taskId, "agent");
    }
    expect(status).toMatchObject({ state: "exited", exitCode: 0 });
    await terminals.finalize(taskId, "agent");
  });

  test("returns a captured usage error for unknown commands and invalid levels", async () => {
    const cli = harness();
    expect(await runCli(["definitely-not-a-command"], cli.options)).toBe(2);
    expect(cli.stderr.value).toContain("Unknown command");
    cli.stderr.value = "";
    expect(await runCli(["task", "create", "Something", "--level", "vibes"], cli.options)).toBe(2);
    expect(cli.stderr.value).toContain("Unknown intent level: vibes");
  });
});

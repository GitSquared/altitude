import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { CliOptions } from "../src/cli/main.ts";
import { runCli } from "../src/cli/main.ts";
import { TaskStore } from "../src/storage/task-store.ts";

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

describe("CLI", () => {
  test("documents sandboxed first-run setup without touching a runtime", async () => {
    const cli = harness();
    expect(await runCli(["setup", "help"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("official Claude Code login");
    expect(cli.stdout.value).toContain("source-free identity capsule");
  });

  test("documents the headless node listener without starting it", async () => {
    const cli = harness();
    expect(await runCli(["node", "help"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("altitude node serve");
    expect(cli.stdout.value).toContain("loopback");
  });

  test("documents the first-run repository capsule and native take flow", async () => {
    const cli = harness();
    expect(await runCli(["task", "help"], cli.options)).toBe(0);
    expect(cli.stdout.value).toContain("task run <task-id> [--repository <path>]");
    expect(cli.stdout.value).toContain("task take <task-id>");
    expect(cli.stdout.value).toContain("task review <task-id> --url <loopback-url>");
    expect(cli.stdout.value).toContain("task send <task-id> --node <id>");
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
          question: "Apple container or Docker?",
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

  test("returns a captured usage error for unknown commands and invalid levels", async () => {
    const cli = harness();
    expect(await runCli(["definitely-not-a-command"], cli.options)).toBe(2);
    expect(cli.stderr.value).toContain("Unknown command");
    cli.stderr.value = "";
    expect(await runCli(["task", "create", "Something", "--level", "vibes"], cli.options)).toBe(2);
    expect(cli.stderr.value).toContain("Unknown intent level: vibes");
  });
});

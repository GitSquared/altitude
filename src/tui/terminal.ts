import { createInterface } from "node:readline/promises";
import type { ReadStream, WriteStream } from "node:tty";

export type TuiKey =
  | "enter"
  | "up"
  | "down"
  | "new"
  | "run"
  | "shell"
  | "ack"
  | "quit"
  | "unknown";

export interface TuiTerminal {
  readonly interactive: boolean;
  draw(content: string): void;
  readKey(): Promise<TuiKey>;
  readLine(prompt: string): Promise<string | undefined>;
  beforeExternal(): void;
  afterExternal(): void;
}

export function decodeTuiKey(value: string): TuiKey {
  if (value === "\r" || value === "\n") return "enter";
  if (value === "\u001b[A" || value === "k") return "up";
  if (value === "\u001b[B" || value === "j") return "down";
  if (value === "n") return "new";
  if (value === "r") return "run";
  if (value === "s") return "shell";
  if (value === "u") return "ack";
  if (value === "q" || value === "\u0003" || value === "\u001b") return "quit";
  return "unknown";
}

export class ProcessTuiTerminal implements TuiTerminal {
  readonly interactive: boolean;

  constructor(
    private readonly input: ReadStream = process.stdin,
    private readonly output: WriteStream = process.stdout,
  ) {
    this.interactive = input.isTTY === true && output.isTTY === true;
  }

  draw(content: string): void {
    this.output.write(`\u001b[2J\u001b[H${content}`);
  }

  async readKey(): Promise<TuiKey> {
    if (!this.interactive) throw new Error("Altitude's TUI requires an interactive terminal");
    const wasRaw = this.input.isRaw;
    this.input.setRawMode(true);
    this.input.resume();
    try {
      const value = await new Promise<string>((resolve, reject) => {
        const onData = (chunk: Buffer | string) => {
          cleanup();
          resolve(typeof chunk === "string" ? chunk : chunk.toString("utf8"));
        };
        const onError = (error: Error) => {
          cleanup();
          reject(error);
        };
        const cleanup = () => {
          this.input.off("data", onData);
          this.input.off("error", onError);
        };
        this.input.once("data", onData);
        this.input.once("error", onError);
      });
      return decodeTuiKey(value);
    } finally {
      this.input.setRawMode(wasRaw);
      if (!wasRaw) this.input.pause();
    }
  }

  async readLine(prompt: string): Promise<string | undefined> {
    if (!this.interactive) throw new Error("Altitude's TUI requires an interactive terminal");
    const wasRaw = this.input.isRaw;
    if (wasRaw) this.input.setRawMode(false);
    const reader = createInterface({ input: this.input, output: this.output, terminal: true });
    try {
      return await reader.question(prompt);
    } catch (error) {
      if (typeof error === "object" && error !== null && "code" in error && error.code === "ERR_USE_AFTER_CLOSE") {
        return undefined;
      }
      throw error;
    } finally {
      reader.close();
      if (wasRaw) this.input.setRawMode(true);
    }
  }

  beforeExternal(): void {
    this.output.write("\u001b[2J\u001b[H");
  }

  afterExternal(): void {
    // The next draw owns the screen. This hook keeps the terminal seam explicit
    // for a future PTY multiplexer without leaking it into the app state machine.
  }
}

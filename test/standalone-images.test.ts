import { afterEach, expect, test } from "bun:test";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  isAltitudeSourceCheckout,
  prepareStandaloneImageContext,
} from "../src/runtime/standalone-images.ts";

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

test("source checkout detection distinguishes development from installed-binary setup", async () => {
  const root = mkdtempSync(join(tmpdir(), "altitude-source-checkout-"));
  roots.push(root);
  for (const directory of [
    join(root, "src"),
    join(root, "images", "identity"),
    join(root, "images", "task"),
  ]) mkdirSync(directory, { recursive: true });
  for (const file of [
    "bun.lock",
    "tsconfig.json",
    join("src", "cli.ts"),
    join("images", "identity", "Containerfile"),
    join("images", "task", "Containerfile"),
  ]) writeFileSync(join(root, file), "fixture\n");
  writeFileSync(join(root, "package.json"), JSON.stringify({ name: "altitude" }));

  expect(await isAltitudeSourceCheckout(root)).toBeTrue();
  writeFileSync(join(root, "package.json"), JSON.stringify({ name: "another-app" }));
  expect(await isAltitudeSourceCheckout(root)).toBeFalse();
});

test("standalone image context carries only the installed binary and capsule recipes", async () => {
  const root = mkdtempSync(join(tmpdir(), "altitude-standalone-images-"));
  roots.push(root);
  const executable = join(root, "altitude");
  writeFileSync(executable, "compiled-altitude", { mode: 0o700 });
  const context = await prepareStandaloneImageContext({
    parentDirectory: join(root, "cache"),
    executable,
    platform: "linux",
  });

  expect(readFileSync(join(context.directory, "altitude"), "utf8"))
    .toBe("compiled-altitude");
  expect(statSync(join(context.directory, "altitude")).mode & 0o777)
    .toBe(0o700);
  expect(readFileSync(
    join(context.directory, "images", "identity", "Containerfile"),
    "utf8",
  )).toContain("@anthropic-ai/claude-code");
  expect(readFileSync(
    join(context.directory, "images", "task", "Containerfile"),
    "utf8",
  )).toContain("COPY --chown=1000:1000 --chmod=0755 altitude");
  expect(readFileSync(
    join(context.directory, "images", "task", "Containerfile"),
    "utf8",
  )).toContain("ca-certificates gh git openssh-client ripgrep");
  expect(readFileSync(join(context.directory, ".dockerignore"), "utf8"))
    .not.toContain("identity/claude");

  await context.cleanup();
  expect(() => statSync(context.directory)).toThrow();
  await context.cleanup();
});

test("installed binary image setup refuses a non-Linux host", async () => {
  const root = mkdtempSync(join(tmpdir(), "altitude-standalone-images-"));
  roots.push(root);
  const executable = join(root, "altitude");
  writeFileSync(executable, "compiled-altitude", { mode: 0o700 });
  await expect(prepareStandaloneImageContext({
    parentDirectory: join(root, "cache"),
    executable,
    platform: "darwin",
  })).rejects.toThrow("use --source");
});

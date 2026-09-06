import { chmod, chown, lstat } from "node:fs/promises";
import { resolve } from "node:path";

import { IDENTITY_GID, IDENTITY_UID } from "../identity/init.ts";

export async function initializeTaskRoot(
  inputRoot: string,
  uid = IDENTITY_UID,
  gid = IDENTITY_GID,
): Promise<void> {
  if (!Number.isInteger(uid) || uid < 0 || !Number.isInteger(gid) || gid < 0) {
    throw new Error("Task uid and gid must be non-negative integers");
  }
  const root = resolve(inputRoot);
  const stat = await lstat(root);
  if (stat.isSymbolicLink()) throw new Error("Task root is a symbolic link");
  if (!stat.isDirectory()) throw new Error("Task root is not a regular directory");
  await chown(root, uid, gid);
  await chmod(root, 0o700);
}

export async function runTaskInit(root = "/workspace"): Promise<number> {
  if (process.env.ALTITUDE_TASK_INIT !== "1") {
    process.stderr.write("Task initialization may only run through Altitude capsule setup.\n");
    return 1;
  }
  try {
    await initializeTaskRoot(root);
    return 0;
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    return 1;
  }
}

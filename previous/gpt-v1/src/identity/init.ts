import { chmod, chown, lstat, mkdir, realpath } from "node:fs/promises";
import { join, resolve } from "node:path";

export const IDENTITY_UID = 1_000;
export const IDENTITY_GID = 1_000;

async function requireDirectory(path: string): Promise<void> {
  const stat = await lstat(path);
  if (stat.isSymbolicLink()) throw new Error(`${path} is a symbolic link`);
  if (!stat.isDirectory()) throw new Error(`${path} is not a regular identity directory`);
}

/** Initialize only the named identity directories; never recursively chown arbitrary material. */
export async function initializeIdentityRoot(
  inputRoot: string,
  uid = IDENTITY_UID,
  gid = IDENTITY_GID,
): Promise<void> {
  if (!Number.isInteger(uid) || uid < 0 || !Number.isInteger(gid) || gid < 0) {
    throw new Error("Identity uid and gid must be non-negative integers");
  }
  const requested = resolve(inputRoot);
  await requireDirectory(requested);
  const root = await realpath(requested);
  const directories = [
    root,
    join(root, "home"),
    join(root, "claude"),
    join(root, "control"),
    join(root, "control", "tasks"),
  ];
  for (const directory of directories.slice(1)) {
    await mkdir(directory, { recursive: false, mode: 0o700 }).catch(async (error: unknown) => {
      if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "EEXIST") throw error;
      await requireDirectory(directory);
    });
  }
  for (const directory of directories) {
    await chown(directory, uid, gid);
    await chmod(directory, 0o700);
  }
}

export async function runIdentityInit(root = "/identity"): Promise<number> {
  if (process.env.ALTITUDE_IDENTITY_INIT !== "1") {
    process.stderr.write("Identity initialization may only run through Altitude capsule setup.\n");
    return 1;
  }
  try {
    await initializeIdentityRoot(root);
    return 0;
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    return 1;
  }
}

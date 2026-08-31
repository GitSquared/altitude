import { homedir } from "node:os";
import { join } from "node:path";

export interface AltitudePaths {
  readonly root: string;
  readonly config: string;
  readonly state: string;
  readonly cache: string;
  readonly database: string;
  readonly capsules: string;
  readonly identities: string;
  readonly nodes: string;
  readonly terminals: string;
}
export function resolveAltitudePaths(
  env: Readonly<Record<string, string | undefined>> = process.env,
): AltitudePaths {
  const explicitRoot = env.ALTITUDE_HOME?.trim();
  const userHome = homedir();
  const stateRoot = explicitRoot ?? join(env.XDG_STATE_HOME ?? join(userHome, ".local", "state"), "altitude");
  const configRoot = explicitRoot ?? join(env.XDG_CONFIG_HOME ?? join(userHome, ".config"), "altitude");
  const cacheRoot = explicitRoot ?? join(env.XDG_CACHE_HOME ?? join(userHome, ".cache"), "altitude");

  return {
    root: stateRoot,
    config: join(configRoot, "config.json"),
    state: stateRoot,
    cache: cacheRoot,
    database: join(stateRoot, "altitude.db"),
    capsules: join(stateRoot, "capsules"),
    identities: join(stateRoot, "identities"),
    nodes: join(stateRoot, "nodes"),
    terminals: join(stateRoot, "terminals"),
  };
}

export async function ensureAltitudePaths(paths: AltitudePaths): Promise<void> {
  const directories = [
    paths.state,
    paths.cache,
    paths.capsules,
    paths.identities,
    paths.nodes,
    paths.terminals,
  ];

  await Promise.all(
    directories.map((directory) => Bun.write(join(directory, ".keep"), "", { createPath: true, mode: 0o600 })),
  );
}

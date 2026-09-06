import { compareCanonicalStrings, stableJson, utf8 } from "./canonical.ts";
import {
  parseCapsuleManifest,
  verifyCapsuleMembers,
  type CapsuleCheckpoint,
  type CapsuleMemberContent,
} from "./manifest.ts";
import { portableCapsulePath } from "./paths.ts";

export const DIRECTORY_CHECKPOINT_FORMAT =
  "altitude.capsule-directory/v1" as const;
export const CAPSULE_MANIFEST_ENTRY = "capsule.json" as const;

export interface DirectoryCheckpointEntry {
  readonly path: string;
  readonly bytes: Uint8Array;
}

export interface CapsuleArchiveCodec<Encoded> {
  readonly format: string;
  encode(checkpoint: CapsuleCheckpoint): Encoded;
  decode(encoded: Encoded): CapsuleCheckpoint;
}

export class InvalidCapsuleArchiveError extends Error {
  override readonly name = "InvalidCapsuleArchiveError";
}

function cloneEntry(entry: DirectoryCheckpointEntry): DirectoryCheckpointEntry {
  return { path: entry.path, bytes: entry.bytes.slice() };
}

export const directoryCheckpointCodec: CapsuleArchiveCodec<
  readonly DirectoryCheckpointEntry[]
> = {
  format: DIRECTORY_CHECKPOINT_FORMAT,
  encode(checkpoint) {
    verifyCapsuleMembers(checkpoint.manifest, checkpoint.members);
    const entries: DirectoryCheckpointEntry[] = [
      {
        path: CAPSULE_MANIFEST_ENTRY,
        bytes: utf8(`${stableJson(checkpoint.manifest)}\n`),
      },
      ...checkpoint.members.map((member) => ({
        path: `members/${member.path}`,
        bytes: member.bytes.slice(),
      })),
    ];
    return entries.sort((left, right) =>
      compareCanonicalStrings(left.path, right.path)
    );
  },
  decode(encoded) {
    const entries = encoded.map(cloneEntry);
    const seen = new Set<string>();
    for (const entry of entries) {
      if (seen.has(entry.path)) {
        throw new InvalidCapsuleArchiveError(`Duplicate archive entry ${entry.path}`);
      }
      seen.add(entry.path);
    }
    const manifestEntry = entries.find(
      (entry) => entry.path === CAPSULE_MANIFEST_ENTRY,
    );
    if (manifestEntry === undefined) {
      throw new InvalidCapsuleArchiveError("Archive is missing capsule.json");
    }
    let manifestValue: unknown;
    try {
      manifestValue = JSON.parse(new TextDecoder().decode(manifestEntry.bytes));
    } catch (error) {
      throw new InvalidCapsuleArchiveError("capsule.json is not valid JSON", {
        cause: error,
      });
    }
    const manifest = parseCapsuleManifest(manifestValue);
    const members: CapsuleMemberContent[] = [];
    for (const entry of entries) {
      if (entry.path === CAPSULE_MANIFEST_ENTRY) continue;
      if (!entry.path.startsWith("members/")) {
        throw new InvalidCapsuleArchiveError(
          `Unexpected archive entry ${entry.path}`,
        );
      }
      members.push({
        path: portableCapsulePath(entry.path.slice("members/".length)),
        bytes: entry.bytes.slice(),
      });
    }
    verifyCapsuleMembers(manifest, members);
    members.sort((left, right) =>
      compareCanonicalStrings(left.path, right.path)
    );
    return { manifest, members };
  },
};

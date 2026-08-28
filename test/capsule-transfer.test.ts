import { describe, expect, test } from "bun:test";

import {
  DuplicateTransferCommandError,
  StaleOwnershipEpochError,
  TransferProtocolError,
  applyTransferCommand,
  createOwnedTransferState,
  ownsRunnableLease,
  sha256,
  utf8,
  type BeginTransferCommand,
  type CapsuleTransferState,
  type TransferCommand,
} from "../src/capsule/index.ts";

const DIGEST = sha256(utf8("checkpoint"));

function initial(): CapsuleTransferState {
  return createOwnedTransferState({
    capsuleId: "capsule-1",
    ownerNodeId: "macbook",
    leaseId: "lease-local",
    epoch: 8,
  });
}

function begin(): BeginTransferCommand {
  return {
    type: "begin-transfer",
    commandId: "command-begin",
    expectedEpoch: 8,
    transferId: "transfer-1",
    sourceNodeId: "macbook",
    sourceLeaseId: "lease-local",
    targetNodeId: "straylight",
    targetLeaseId: "lease-remote",
    checkpointDigest: DIGEST,
  };
}

function apply(
  state: CapsuleTransferState,
  command: TransferCommand,
): CapsuleTransferState {
  return applyTransferCommand(state, command);
}

describe("capsule ownership transfer", () => {
  test("performs prepare, target verification, commit, and activation ACK", () => {
    let state = initial();
    expect(
      ownsRunnableLease(state, {
        nodeId: "macbook",
        leaseId: "lease-local",
        epoch: 8,
      }),
    ).toBe(true);

    state = apply(state, begin());
    expect(state.phase).toBe("prepared");
    expect(
      ownsRunnableLease(state, {
        nodeId: "macbook",
        leaseId: "lease-local",
        epoch: 8,
      }),
    ).toBe(false);

    state = apply(state, {
      type: "record-target-prepared",
      commandId: "command-prepared",
      expectedEpoch: 8,
      transferId: "transfer-1",
      checkpointDigest: DIGEST,
    });
    state = apply(state, {
      type: "commit-transfer",
      commandId: "command-commit",
      expectedEpoch: 8,
      transferId: "transfer-1",
    });
    expect(state.phase).toBe("commit-issued");
    expect(state.ownership).toEqual({
      epoch: 9,
      ownerNodeId: "straylight",
      leaseId: "lease-remote",
      status: "pending-activation",
    });

    state = apply(state, {
      type: "record-activation-ack",
      commandId: "command-ack",
      expectedEpoch: 9,
      transferId: "transfer-1",
      targetNodeId: "straylight",
      targetLeaseId: "lease-remote",
    });
    expect(state.phase).toBe("owned");
    expect(
      ownsRunnableLease(state, {
        nodeId: "straylight",
        leaseId: "lease-remote",
        epoch: 9,
      }),
    ).toBe(true);
  });

  test("rejects stale ownership epochs", () => {
    const stale = { ...begin(), expectedEpoch: 7 };
    expect(() => apply(initial(), stale)).toThrow(StaleOwnershipEpochError);
  });

  test("treats an exact duplicate command as an idempotent no-op", () => {
    const state = apply(initial(), begin());
    const duplicate = apply(state, begin());
    expect(duplicate).toBe(state);
    expect(duplicate.appliedCommands).toHaveLength(1);
  });

  test("rejects a duplicate command id carrying different intent", () => {
    const state = apply(initial(), begin());
    expect(() =>
      apply(state, {
        ...begin(),
        targetNodeId: "somewhere-else",
      })
    ).toThrow(DuplicateTransferCommandError);
  });

  test("can abort safely before commit, but never rolls ownership back after commit", () => {
    let state = apply(initial(), begin());
    state = apply(state, {
      type: "abort-transfer",
      commandId: "command-abort",
      expectedEpoch: 8,
      transferId: "transfer-1",
    });
    expect(state.phase).toBe("owned");
    expect(state.ownership.ownerNodeId).toBe("macbook");

    state = apply(state, { ...begin(), commandId: "command-begin-2" });
    state = apply(state, {
      type: "record-target-prepared",
      commandId: "command-prepared",
      expectedEpoch: 8,
      transferId: "transfer-1",
      checkpointDigest: DIGEST,
    });
    state = apply(state, {
      type: "commit-transfer",
      commandId: "command-commit",
      expectedEpoch: 8,
      transferId: "transfer-1",
    });
    expect(() =>
      apply(state, {
        type: "abort-transfer",
        commandId: "too-late",
        expectedEpoch: 9,
        transferId: "transfer-1",
      })
    ).toThrow(TransferProtocolError);
  });

  test("fail-stops instead of reviving the source when activation ACK is ambiguous", () => {
    let state = apply(initial(), begin());
    state = apply(state, {
      type: "record-target-prepared",
      commandId: "command-prepared",
      expectedEpoch: 8,
      transferId: "transfer-1",
      checkpointDigest: DIGEST,
    });
    state = apply(state, {
      type: "commit-transfer",
      commandId: "command-commit",
      expectedEpoch: 8,
      transferId: "transfer-1",
    });
    state = apply(state, {
      type: "mark-ack-ambiguous",
      commandId: "command-timeout",
      expectedEpoch: 9,
      transferId: "transfer-1",
    });
    expect(state.phase).toBe("fail-stopped");
    expect(state.failureReason).toBe("ambiguous-activation-ack");
    expect(state.ownership.status).toBe("uncertain");
    expect(
      ownsRunnableLease(state, {
        nodeId: "macbook",
        leaseId: "lease-local",
        epoch: 8,
      }),
    ).toBe(false);
    expect(
      ownsRunnableLease(state, {
        nodeId: "straylight",
        leaseId: "lease-remote",
        epoch: 9,
      }),
    ).toBe(false);
  });
});

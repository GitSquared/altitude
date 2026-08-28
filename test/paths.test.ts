import { describe, expect, test } from "bun:test";

import { resolveAltitudePaths } from "../src/config/paths.ts";

describe("Altitude paths", () => {
  test("keeps an explicit Altitude home self-contained", () => {
    const paths = resolveAltitudePaths({ ALTITUDE_HOME: "/tmp/altitude-test" });

    expect(paths.database).toBe("/tmp/altitude-test/altitude.db");
    expect(paths.identities).toBe("/tmp/altitude-test/identities");
    expect(paths.config).toBe("/tmp/altitude-test/config.json");
  });
});

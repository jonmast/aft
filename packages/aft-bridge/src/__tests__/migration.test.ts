/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ensureStorageMigrated,
  resolveCortexKitStorageRoot,
  resolveLegacyStorageRoot,
} from "../migration.js";
import { type AftFixtureBehavior, writeAftFixture } from "./test-utils/aft-executable-fixture.js";
import { acquireEnv } from "./test-utils/env-guard.js";

describe("storage migration bootstrap", () => {
  let tempDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-migration-test-"));
    releaseEnv = await acquireEnv({
      XDG_DATA_HOME: tempDir,
      HOME: tempDir,
    });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tempDir, { recursive: true, force: true });
  });

  function binary(behavior: AftFixtureBehavior): string {
    const path = join(tempDir, `aft-${Math.random().toString(16).slice(2)}`);
    return writeAftFixture(path, behavior);
  }

  test("ensureStorageMigrated_no_legacy_is_noop", async () => {
    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: "/missing/aft" }),
    ).resolves.toBeUndefined();
  });

  test("ensureStorageMigrated_with_source_marker_backfills_target_marker", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, ".migrated_to_cortexkit"), "{}", "utf8");
    const aft = binary({ exitCode: 0 });

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft }),
    ).resolves.toBeUndefined();
  });

  test("ensureStorageMigrated_spawns_and_succeeds", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary({ exitCode: 0 });

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft }),
    ).resolves.toBeUndefined();
  });

  test("ensureStorageMigrated_throws_on_nonzero_exit", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary({ stderr: "failed\n", exitCode: 5 });

    await expect(ensureStorageMigrated({ harness: "opencode", binaryPath: aft })).rejects.toThrow(
      /exit 5.*logs\/migration\/opencode-/,
    );
  });

  test("ensureStorageMigrated_throws_on_timeout", async () => {
    const legacyRoot = resolveLegacyStorageRoot("opencode");
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    const aft = binary({ sleepMs: 1_000 });

    await expect(
      ensureStorageMigrated({ harness: "opencode", binaryPath: aft, timeoutMs: 10 }),
    ).rejects.toThrow(/ETIMEDOUT|timed out|spawn error/i);
  });

  test("resolveLegacyStorageRoot_returns_pi_fixed_path", () => {
    expect(resolveLegacyStorageRoot("pi")).toBe(
      join(process.env.HOME as string, ".pi", "agent", "aft"),
    );
  });

  test("resolveLegacyStorageRoot_returns_opencode_xdg_path", () => {
    expect(resolveLegacyStorageRoot("opencode")).toBe(
      join(tempDir, "opencode", "storage", "plugin", "aft"),
    );
  });

  test("resolveCortexKitStorageRoot_uses_new_xdg_path", () => {
    expect(resolveCortexKitStorageRoot()).toBe(join(tempDir, "cortexkit", "aft"));
  });
});

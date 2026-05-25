/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import type { DiagnosticReport, HarnessDiagnostic } from "../lib/diagnostics.js";
import {
  buildDoctorFixPlan,
  type DoctorFixPlanItem,
  shouldSkipDoctorFixConfirmation,
} from "./doctor.js";

function configPaths(kind: "opencode" | "pi" = "opencode"): HarnessConfigPaths {
  return {
    configDir: "/tmp/aft-test",
    harnessConfig: kind === "pi" ? "/tmp/aft-test/settings.json" : "/tmp/aft-test/opencode.jsonc",
    harnessConfigFormat: "jsonc",
    aftConfig: "/tmp/aft-test/aft.jsonc",
    aftConfigFormat: "jsonc",
  };
}

function makeAdapter(kind: "opencode" | "pi" = "opencode"): HarnessAdapter {
  const paths = configPaths(kind);
  return {
    kind,
    displayName: kind === "pi" ? "Pi" : "OpenCode",
    pluginPackageName: kind === "pi" ? "@cortexkit/aft-pi" : "@cortexkit/aft-opencode",
    pluginEntryWithVersion:
      kind === "pi" ? "npm:@cortexkit/aft-pi" : "@cortexkit/aft-opencode@latest",
    isInstalled: () => true,
    getHostVersion: () => "test",
    detectConfigPaths: () => paths,
    hasPluginEntry: () => false,
    ensurePluginEntry: async () => ({
      ok: true,
      action: "added",
      message: "registered",
      configPath: paths.harnessConfig,
    }),
    getPluginCacheInfo: () => ({ path: "/tmp/aft-test/plugin-cache", exists: false }),
    getStorageDir: () => "/tmp/aft-test/storage",
    getLogFile: () => "/tmp/aft-test/aft.log",
    getInstallHint: () => "install harness",
    clearPluginCache: async () => ({ action: "not_found", path: "/tmp/aft-test/plugin-cache" }),
  };
}

function makeHarness(overrides: Partial<HarnessDiagnostic> = {}): HarnessDiagnostic {
  const kind = (overrides.kind as "opencode" | "pi" | undefined) ?? "opencode";
  return {
    kind,
    displayName: kind === "pi" ? "Pi" : "OpenCode",
    hostInstalled: true,
    hostVersion: "test",
    pluginRegistered: true,
    configPaths: configPaths(kind),
    aftConfig: { exists: true, flags: {} },
    pluginCache: { path: "/tmp/aft-test/plugin-cache", exists: false },
    storageDir: { path: "/tmp/aft-test/storage", exists: false, sizesByKey: {} },
    onnxRuntime: {
      required: false,
      systemPath: null,
      systemVersion: null,
      systemCompatible: null,
      cachedPath: null,
      cachedVersion: null,
      cachedCompatible: null,
      platform: "test-test",
      installHint: "install onnx",
      requirement: ">=1.20",
    },
    logFile: { path: "/tmp/aft-test/aft.log", exists: false, sizeKb: 0 },
    ...overrides,
  };
}

function makeReport(
  harnesses: HarnessDiagnostic[],
  binaryVersion: string | null,
): DiagnosticReport {
  return {
    timestamp: "2026-01-01T00:00:00.000Z",
    platform: "darwin",
    arch: "arm64",
    nodeVersion: "v24.0.0",
    cliVersion: "0.30.1",
    binaryVersion,
    harnesses,
    binaryCache: { path: "/tmp/aft-test/bin", versions: [], totalSize: 0 },
    lspCache: {
      npm: { path: "/tmp/aft-test/npm", entries: [], totalSize: 0 },
      github: { path: "/tmp/aft-test/gh", entries: [], totalSize: 0 },
      totalSize: 0,
    },
  };
}

function messages(plan: DoctorFixPlanItem[]): string[] {
  return plan.map((item) => item.message);
}

describe("doctor --fix planning", () => {
  test("lists plugin and binary mutations before applying fixes", () => {
    const report = makeReport([makeHarness({ pluginRegistered: false })], null);

    const plan = buildDoctorFixPlan([makeAdapter()], report);

    expect(messages(plan)).toEqual([
      "Will add @cortexkit/aft-opencode@latest to /tmp/aft-test/opencode.jsonc",
      "Will download/cache the aft binary matching CLI v0.30.1",
    ]);
  });

  test("describes Pi registration as a pi install mutation", () => {
    const report = makeReport(
      [makeHarness({ kind: "pi", displayName: "Pi", pluginRegistered: false })],
      "0.30.1",
    );

    const plan = buildDoctorFixPlan([makeAdapter("pi")], report);

    expect(messages(plan)).toEqual(["Will run `pi install npm:@cortexkit/aft-pi` to register Pi"]);
  });

  test("skips the confirmation prompt for explicit automation flags", () => {
    expect(shouldSkipDoctorFixConfirmation(["--yes"])).toBe(true);
    expect(shouldSkipDoctorFixConfirmation(["--ci"])).toBe(true);
  });
});

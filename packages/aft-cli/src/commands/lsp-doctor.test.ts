/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import type { AftRequest } from "../lib/aft-bridge.js";
import { findProjectRootForFile, runLspDoctor } from "./lsp.js";

function makeAdapter(configDir: string): HarnessAdapter {
  const configPaths: HarnessConfigPaths = {
    configDir,
    harnessConfig: join(configDir, "opencode.jsonc"),
    harnessConfigFormat: "jsonc",
    aftConfig: join(configDir, "aft.jsonc"),
    aftConfigFormat: "jsonc",
  };

  return {
    kind: "opencode",
    displayName: "OpenCode",
    pluginPackageName: "@cortexkit/aft-opencode",
    pluginEntryWithVersion: "@cortexkit/aft-opencode@latest",
    isInstalled: () => true,
    getHostVersion: () => "test",
    detectConfigPaths: () => configPaths,
    hasPluginEntry: () => true,
    ensurePluginEntry: async () => ({
      ok: true,
      action: "already_present",
      message: "already registered",
      configPath: configPaths.harnessConfig,
    }),
    getPluginCacheInfo: () => ({ path: join(configDir, "plugin-cache"), exists: false }),
    getStorageDir: () => join(configDir, "storage"),
    getLogFile: () => join(configDir, "aft.log"),
    getInstallHint: () => "Install OpenCode",
    clearPluginCache: async () => ({ action: "not_found", path: join(configDir, "plugin-cache") }),
  };
}

const tempRoots = new Set<string>();
const originalCwd = process.cwd();

function tempRoot(prefix: string): string {
  const root = mkdtempSync(join(tmpdir(), prefix));
  tempRoots.add(root);
  return root;
}

afterEach(() => {
  process.chdir(originalCwd);
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
});

describe("doctor lsp project root detection", () => {
  test("walks up from inspected file and loads project config from that root", async () => {
    const outside = tempRoot("aft-lsp-outside-");
    const project = tempRoot("aft-lsp-project-");
    const userConfig = tempRoot("aft-lsp-user-config-");
    const srcDir = join(project, "src", "pkg");
    mkdirSync(srcDir, { recursive: true });
    mkdirSync(join(project, ".opencode"), { recursive: true });
    writeFileSync(join(project, "package.json"), JSON.stringify({ name: "sample" }));
    writeFileSync(join(srcDir, "main.py"), "print('hello')\n");
    writeFileSync(
      join(project, ".opencode", "aft.json"),
      JSON.stringify({ lsp: { python: "ty", disabled: ["lua"] } }),
    );
    process.chdir(outside);

    let configure: AftRequest | undefined;
    const file = join(srcDir, "main.py");
    const code = await runLspDoctor({
      argv: [file, "--harness", "opencode"],
      findBinary: () => "/tmp/aft-bin",
      resolveAdapters: async () => [makeAdapter(userConfig)],
      sendRequests: async (_binary, batch) => {
        configure = batch[0];
        return [
          { id: "doctor-lsp-configure", success: true },
          {
            id: "doctor-lsp-inspect",
            success: true,
            file,
            extension: "py",
            project_root: project,
            matching_servers: [],
            diagnostics_count: 0,
            diagnostics: [],
          },
        ];
      },
    });

    expect(code).toBe(0);
    expect(configure?.project_root).toBe(project);
    expect(configure?.experimental_lsp_ty).toBe(true);
    expect(configure?.disabled_lsp).toEqual(expect.arrayContaining(["lua", "python"]));
  });

  test("falls back to cwd when no project marker is found", () => {
    const outside = tempRoot("aft-lsp-fallback-");
    const looseFile = join(tempRoot("aft-lsp-loose-"), "src", "main.py");
    expect(findProjectRootForFile(looseFile, outside)).toBe(outside);
  });
});

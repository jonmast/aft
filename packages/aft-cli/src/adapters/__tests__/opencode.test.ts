/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { fixPluginEntries } from "../../commands/doctor.js";
import { OpenCodeAdapter } from "../opencode.js";

class InstalledOpenCodeAdapter extends OpenCodeAdapter {
  override isInstalled(): boolean {
    return true;
  }
}

const JSONC_WITH_COMMENTS = `{
  // keep top-level comment
  "theme": "dark",
  // keep plugin comment
  "plugin": [
    // keep existing plugin comment
    "some-other-plugin"
  ]
}
`;

describe("OpenCodeAdapter JSONC preservation", () => {
  let configDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    configDir = mkdtempSync(join(tmpdir(), "aft-opencode-jsonc-"));
    mkdirSync(configDir, { recursive: true });
    releaseEnv = await acquireEnv({ OPENCODE_CONFIG_DIR: configDir });
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
  });

  test("ensurePluginEntry preserves opencode.jsonc comments used by setup", async () => {
    const configPath = join(configDir, "opencode.jsonc");
    writeFileSync(configPath, JSONC_WITH_COMMENTS);

    const result = await new OpenCodeAdapter().ensurePluginEntry();

    expect(result.ok).toBe(true);
    const written = readFileSync(configPath, "utf-8");
    expect(written).toContain("// keep top-level comment");
    expect(written).toContain("// keep plugin comment");
    expect(written).toContain("// keep existing plugin comment");
    expect(written).toContain("@cortexkit/aft-opencode@latest");
  });

  test("doctor fix path preserves opencode.jsonc comments when registering plugin", async () => {
    const configPath = join(configDir, "opencode.jsonc");
    writeFileSync(configPath, JSONC_WITH_COMMENTS);

    await fixPluginEntries([new InstalledOpenCodeAdapter()]);

    const written = readFileSync(configPath, "utf-8");
    expect(written).toContain("// keep top-level comment");
    expect(written).toContain("// keep plugin comment");
    expect(written).toContain("// keep existing plugin comment");
    expect(written).toContain("@cortexkit/aft-opencode@latest");
  });
});

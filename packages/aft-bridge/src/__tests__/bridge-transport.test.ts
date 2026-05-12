/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BinaryBridge } from "../bridge.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bridge-transport-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function writeExecutable(name: string, source: string): string {
  const path = join(workDir, name);
  writeFileSync(path, source);
  chmodSync(path, 0o755);
  return path;
}

describe("BinaryBridge transport regressions", () => {
  test("stdout NDJSON decoder preserves multibyte UTF-8 split across chunks", async () => {
    const script = writeExecutable(
      "split-emoji.js",
      `#!/usr/bin/env node
process.stdin.setEncoding("utf8");
let input = "";
process.stdin.on("data", (chunk) => {
  input += chunk;
  const newline = input.indexOf("\\n");
  if (newline === -1) return;
  const line = input.slice(0, newline);
  const req = JSON.parse(line);
  const out = Buffer.from(JSON.stringify({ id: req.id, success: true, version: "1.2.3 🚀" }) + "\\n");
  const emoji = Buffer.from("🚀");
  const splitAt = out.indexOf(emoji) + 1;
  process.stdout.write(out.subarray(0, splitAt));
  setTimeout(() => process.stdout.write(out.subarray(splitAt)), 5);
});
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 500, maxRestarts: 0 });

    try {
      const response = await bridge.send("version");
      expect(response.version).toBe("1.2.3 🚀");
    } finally {
      await bridge.shutdown();
    }
  });

  test("timeout-killed bridge aborts sibling requests immediately", async () => {
    const script = writeExecutable(
      "silent.js",
      `#!/usr/bin/env node
process.stdin.resume();
`,
    );
    const bridge = new BinaryBridge(script, workDir, { timeoutMs: 1_000, maxRestarts: 0 });

    try {
      const first = bridge.send("version", {}, { timeoutMs: 20 });
      const sibling = bridge.send("version", {}, { timeoutMs: 1_000 });

      await expect(first).rejects.toThrow(/timed out/);
      const siblingResult = await Promise.race([
        sibling.then(
          () => "resolved",
          (err) => String(err instanceof Error ? err.message : err),
        ),
        new Promise<string>((resolve) => setTimeout(() => resolve("still pending"), 100)),
      ]);

      expect(siblingResult).toContain("sibling timeout");
    } finally {
      await bridge.shutdown();
    }
  });

  test("version RPC success:false rejects when minVersion is set", async () => {
    const bridge = new BinaryBridge("/fake/aft", workDir, { minVersion: "1.0.0" });
    const testBridge = bridge as unknown as {
      send(command: string): Promise<Record<string, unknown>>;
      checkVersion(): Promise<void>;
    };
    testBridge.send = async () => ({ success: false, code: "unknown-command" });

    await expect(testBridge.checkVersion()).rejects.toThrow(/Binary version check failed/);
  });

  test("version RPC missing version rejects when minVersion is set", async () => {
    const bridge = new BinaryBridge("/fake/aft", workDir, { minVersion: "1.0.0" });
    const testBridge = bridge as unknown as {
      send(command: string): Promise<Record<string, unknown>>;
      checkVersion(): Promise<void>;
    };
    testBridge.send = async () => ({ success: true });

    await expect(testBridge.checkVersion()).rejects.toThrow(/did not report a version/);
  });

  test("configureWarningClients evicts entries after delivery and clears on shutdown", async () => {
    const delivered: unknown[] = [];
    const bridge = new BinaryBridge("/fake/aft", workDir, {
      onConfigureWarnings: (context) => {
        delivered.push(context.client);
      },
    });
    const testBridge = bridge as unknown as {
      configureWarningClients: Map<string, unknown>;
      handleConfigureWarningsFrame(frame: Record<string, unknown>): Promise<void>;
      shutdown(): Promise<void>;
    };
    testBridge.configureWarningClients.set("s1", { name: "client-1" });
    testBridge.configureWarningClients.set("s2", { name: "client-2" });
    testBridge.configureWarningClients.set("s3", { name: "client-3" });

    for (const session_id of ["s1", "s2", "s3"]) {
      await testBridge.handleConfigureWarningsFrame({
        type: "configure_warnings",
        session_id,
        warnings: [{ code: "large_repo", message: session_id }],
      });
    }

    expect(delivered).toHaveLength(3);
    expect(testBridge.configureWarningClients.size).toBe(0);

    testBridge.configureWarningClients.set("stale", { name: "stale-client" });
    await testBridge.shutdown();
    expect(testBridge.configureWarningClients.size).toBe(0);
  });
});

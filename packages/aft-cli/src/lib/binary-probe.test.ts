/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { normalizeBinaryVersion, probeAftBinary, probeBinaryVersion } from "./binary-probe.js";
import { getAftBinaryName } from "./paths.js";

function writeFakeAft(path: string, body: string): void {
  writeFileSync(path, `#!/bin/sh\n${body}\n`);
  chmodSync(path, 0o755);
}

describe("binary probe version validation", () => {
  test("accepts only semver-shaped aft version output", () => {
    expect(normalizeBinaryVersion("aft 1.2.3\n")).toBe("1.2.3");
    expect(normalizeBinaryVersion("1.2.3-beta.1\n")).toBe("1.2.3-beta.1");
    expect(normalizeBinaryVersion("not-aft 1.2.3\n")).toBeNull();
    expect(normalizeBinaryVersion("hello from another binary\n")).toBeNull();
  });

  test("skips mismatched candidates and reports them as unmatched", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cli-binary-probe-audit-"));
    const pathDir = join(root, "path");
    mkdirSync(pathDir, { recursive: true });

    const cacheDir = join(root, "cache", "bin", "v9.8.7");
    mkdirSync(cacheDir, { recursive: true });
    writeFakeAft(join(cacheDir, getAftBinaryName()), 'printf "aft 8.0.0\\n"');
    writeFakeAft(join(pathDir, getAftBinaryName()), 'printf "aft 9.8.1\\n"');

    await withEnv(
      {
        AFT_CACHE_DIR: join(root, "cache"),
        PATH: `${pathDir}${delimiter}${process.env.PATH ?? ""}`,
      },
      () => {
        const probe = probeAftBinary("9.8.7");
        expect(probe.version).toBe("9.8.1");
        expect(probe.candidates).toContainEqual(
          expect.objectContaining({ status: "unmatched", version: "8.0.0" }),
        );
      },
    );
  });

  test("rejects random PATH garbage instead of treating it as healthy", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cli-binary-probe-invalid-"));
    const pathDir = join(root, "path");
    mkdirSync(pathDir, { recursive: true });
    writeFakeAft(join(pathDir, getAftBinaryName()), 'printf "definitely not aft\\n"');

    await withEnv(
      {
        AFT_CACHE_DIR: join(root, "cache"),
        PATH: `${pathDir}${delimiter}${process.env.PATH ?? ""}`,
      },
      () => {
        expect(probeBinaryVersion("7.7.7")).toBeNull();
        const probe = probeAftBinary("7.7.7");
        expect(probe.candidates).toContainEqual(expect.objectContaining({ status: "invalid" }));
      },
    );
  });
});

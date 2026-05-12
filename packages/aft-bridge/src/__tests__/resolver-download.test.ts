/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, mock, test } from "bun:test";

describe("findBinary async download", () => {
  afterEach(() => {
    mock.restore();
  });

  test("honors expectedVersion when falling through to ensureBinary", async () => {
    const seenVersions: Array<string | undefined> = [];

    mock.module("node:child_process", () => ({
      execSync: () => {
        throw new Error("not found");
      },
      spawnSync: () => ({ stdout: "", stderr: "", status: 1 }),
    }));
    mock.module("node:fs", () => ({
      chmodSync: () => undefined,
      copyFileSync: () => undefined,
      existsSync: () => false,
      mkdirSync: () => undefined,
      renameSync: () => undefined,
    }));
    mock.module("node:module", () => ({
      createRequire: () => {
        const req = () => ({ version: "0.0.0" });
        req.resolve = () => {
          throw new Error("package missing");
        };
        return req;
      },
    }));
    mock.module("../downloader.js", () => ({
      ensureBinary: async (version?: string) => {
        seenVersions.push(version);
        return "/downloaded/aft";
      },
      getCacheDir: () => "/cache/aft/bin",
      getCachedBinaryPath: () => null,
    }));

    const { findBinary } = await import(`../resolver.js?expected-version-${Date.now()}`);

    await expect(findBinary("0.99.0-test")).resolves.toBe("/downloaded/aft");
    expect(seenVersions).toEqual(["0.99.0-test"]);
  });
});

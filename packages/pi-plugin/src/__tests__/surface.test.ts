/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { __test__ } from "../index.js";

describe("Pi tool surface", () => {
  test("bash hoisting is independent of read hoisting", () => {
    const surface = __test__.resolveToolSurface({
      disabled_tools: ["read"],
    });

    expect(surface.hoistRead).toBe(false);
    expect(surface.hoistBash).toBe(true);
  });

  test("restrictToProjectRoot defaults to false for parity with Pi built-ins", () => {
    const surface = __test__.resolveToolSurface({});
    expect(surface.restrictToProjectRoot).toBe(false);
  });

  test("restrictToProjectRoot honors explicit opt-in", () => {
    const surface = __test__.resolveToolSurface({ restrict_to_project_root: true });
    expect(surface.restrictToProjectRoot).toBe(true);
  });

  test("restrictToProjectRoot honors explicit opt-out", () => {
    const surface = __test__.resolveToolSurface({ restrict_to_project_root: false });
    expect(surface.restrictToProjectRoot).toBe(false);
  });
});

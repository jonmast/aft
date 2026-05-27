/**
 * Unit tests for hoisted read/write/edit/grep argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { TSchema } from "typebox";
import { Value } from "typebox/value";
import { formatReadFooter, registerHoistedTools } from "../tools/hoisted.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

const roots: string[] = [];

async function tempRoot(): Promise<string> {
  const root = join(tmpdir(), `aft-pi-hoisted-${process.pid}-${roots.length}-${Date.now()}`);
  roots.push(root);
  await mkdir(root, { recursive: true });
  return root;
}

function schemaAccepts(schema: unknown, value: unknown): boolean {
  return Value.Check(schema as TSchema, value);
}

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("hoisted tool adapters", () => {
  test("read maps offset/limit to inclusive start_line/end_line and appends footer", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      content: "1: a\n2: b",
      truncated: true,
      start_line: 1,
      end_line: 2,
      total_lines: 10,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const ranged = (await executeTool(tools.get("read")!, {
      path: "src/app.ts",
      offset: 5,
      limit: 3,
    })) as { content: Array<{ text: string }> };

    expect(calls[0].params).toEqual({ file: "src/app.ts", start_line: 5, end_line: 7 });
    expect(ranged.content[0].text).not.toContain("Use offset/limit");

    const unbounded = (await executeTool(tools.get("read")!, { path: "src/app.ts" })) as {
      content: Array<{ text: string }>;
    };
    expect(unbounded.content[0].text).toContain("Showing lines 1-2 of 10");
  });

  test("edit appendContent uses append op instead of match/replacement fields", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    await executeTool(tools.get("edit")!, {
      filePath: "README.md",
      oldString: "ignored",
      newString: "ignored",
      appendContent: "\nnext",
    });

    expect(calls[0].command).toBe("edit_match");
    expect(calls[0].params).toEqual({
      op: "append",
      file: "README.md",
      append_content: "\nnext",
      diagnostics: false,
      include_diff: true,
    });
  });

  test("edit defaults diagnostics off and omits LSP payload", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      replacements: 1,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("edit")!, {
      filePath: "src/app.ts",
      oldString: "before",
      newString: "after",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].command).toBe("edit_match");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      match: "before",
      replacement: "after",
      diagnostics: false,
      include_diff: true,
    });
    expect(result.content[0].text).not.toContain("LSP diagnostics");
    expect(result.details.diagnostics).toBeUndefined();
  });

  test("edit honors diagnostics true and includes LSP payload", async () => {
    const diagnostics = [{ severity: "error", line: 5, message: "Broken edit" }];
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      replacements: 1,
      lsp_diagnostics: diagnostics,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("edit")!, {
      filePath: "src/app.ts",
      oldString: "before",
      newString: "after",
      diagnostics: true,
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].params.diagnostics).toBe(true);
    expect(result.details.diagnostics).toEqual(diagnostics);
    expect(result.content[0].text).toContain("LSP diagnostics");
    expect(result.content[0].text).toContain("Broken edit");
  });

  test("grep resolves existing path args and preserves brace-aware include globs", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "src"));
    await writeFile(join(root, "src", "app.ts"), "console.log('x');\n");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: "src", include: "*.ts,**/*.{tsx,jsx}", contextLines: 2 },
      { cwd: root } as never,
    );

    expect(calls[0].command).toBe("grep");
    expect(calls[0].params).toEqual({
      pattern: "console",
      path: join(root, "src"),
      include: ["*.ts", "**/*.{tsx,jsx}"],
      context_lines: 2,
    });
  });

  test("grep expands ~ in path arg to the user's home directory", async () => {
    // Agents commonly type `~/Work/...` paths. Without expansion, Node's
    // path.resolve treats `~` as a literal directory, the existence check
    // fails, and Rust receives the unresolved path. Expansion must happen
    // before stat() so absolute tilde paths resolve like the shell would.
    const home = (await import("node:os")).homedir();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    await executeTool(tools.get("grep")!, { pattern: "oauth", path: "~/" }, { cwd: home } as never);

    expect(calls[0].command).toBe("grep");
    // When the expanded path equals the home directory itself, stat()
    // succeeds and resolvePathArg returns the absolute form.
    expect(calls[0].params).toEqual({ pattern: "oauth", path: home });
  });

  test("write defaults diagnostics off and asks Rust for a diff", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].command).toBe("write");
    expect(calls[0].params).toEqual({
      file: "src/app.ts",
      content: "export {};\n",
      diagnostics: false,
      include_diff: true,
    });
    expect(result.content[0].text).not.toContain("LSP diagnostics");
    expect(result.details.diagnostics).toBeUndefined();
  });

  test("write honors diagnostics true and includes LSP payload", async () => {
    const diagnostics = [{ severity: "error", line: 11, message: "Broken write" }];
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      lsp_diagnostics: diagnostics,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
      diagnostics: true,
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].command).toBe("write");
    expect(calls[0].params.diagnostics).toBe(true);
    expect(result.details.diagnostics).toEqual(diagnostics);
    expect(result.content[0].text).toContain("LSP diagnostics");
    expect(result.content[0].text).toContain("Broken write");
  });

  test("mutation schemas reject non-boolean diagnostics", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    expect(
      schemaAccepts(tools.get("write")!.parameters, {
        filePath: "src/app.ts",
        content: "export {};\n",
        diagnostics: "yes",
      }),
    ).toBe(false);
    expect(
      schemaAccepts(tools.get("edit")!.parameters, {
        filePath: "src/app.ts",
        oldString: "before",
        newString: "after",
        diagnostics: "yes",
      }),
    ).toBe(false);
  });

  test("write to external path triggers ui.confirm; denial rejects, approval calls bridge", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    // The ui.confirm prompt fires unconditionally for external paths, matching
    // OpenCode's `external_directory` permission rule. Pi users who want to
    // skip the prompt should rely on Pi's own `extension.permissions` allow-
    // list, not on AFT's `restrict_to_project_root` flag.
    let confirmCallCount = 0;
    const externalPath = join(tmpdir(), `aft-external-${process.pid}-${Date.now()}.txt`);
    let confirmResponse = false;
    const extCtx = {
      cwd: root,
      hasUI: true,
      ui: {
        confirm: (_title: string, _message: string) => {
          confirmCallCount += 1;
          return Promise.resolve(confirmResponse);
        },
      },
    };

    await expect(
      executeTool(tools.get("write")!, { filePath: externalPath, content: "x" }, extCtx as never),
    ).rejects.toThrow("Permission denied");
    expect(confirmCallCount).toBe(1);
    expect(calls).toEqual([]);

    confirmResponse = true;
    await executeTool(
      tools.get("write")!,
      { filePath: externalPath, content: "x" },
      extCtx as never,
    );
    expect(confirmCallCount).toBe(2);
    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("write");
    expect(calls[0].params).toMatchObject({ file: externalPath, content: "x" });
  });

  test("external path skips ui.confirm when restrictToProjectRoot is false", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      // User opted in to "no restriction" — Pi has no host-level allow-list
      // to consult, so AFT must defer to Rust without nagging with a prompt.
      restrictToProjectRoot: false,
    });

    let confirmCallCount = 0;
    const externalPath = join(tmpdir(), `aft-external-norestrict-${process.pid}-${Date.now()}.txt`);
    const extCtx = {
      cwd: root,
      hasUI: true,
      ui: {
        confirm: (_title: string, _message: string) => {
          confirmCallCount += 1;
          return Promise.resolve(false);
        },
      },
    };

    await executeTool(
      tools.get("write")!,
      { filePath: externalPath, content: "x" },
      extCtx as never,
    );

    // Plugin must NOT prompt; Rust accepts the path because the flag forwards
    // to its own `restrict_to_project_root: false`.
    expect(confirmCallCount).toBe(0);
    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("write");
    expect(calls[0].params).toMatchObject({ file: externalPath, content: "x" });
  });

  test("external path denies immediately when hasUI is false (no confirm hang)", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    // Without a UI to surface ui.confirm, we MUST deny synchronously rather
    // than wait on a prompt that nothing will answer — that's the hang the
    // user reported for grep against ~/Work/... in agent-driven mode.
    const externalPath = join(tmpdir(), `aft-external-noui-${process.pid}-${Date.now()}.txt`);
    const extCtx = { cwd: root, hasUI: false };

    await expect(
      executeTool(tools.get("write")!, { filePath: externalPath, content: "x" }, extCtx as never),
    ).rejects.toThrow("Permission denied");
    expect(calls).toEqual([]);
  });

  test("external path denies on confirm timeout (no bridge wedge)", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    // confirm returns a Promise that never resolves — exactly the failure mode
    // observed when Pi can't surface the prompt mid-agent-tool-call. The
    // hard timeout in assertExternalDirectoryPermission must take over and
    // throw a deterministic Permission denied so the agent unblocks. We
    // shrink the prod 30s timeout to 50ms via env override for this test.
    const previous = process.env.AFT_PI_EXTERNAL_PROMPT_TIMEOUT_MS;
    process.env.AFT_PI_EXTERNAL_PROMPT_TIMEOUT_MS = "50";
    try {
      const externalPath = join(tmpdir(), `aft-external-stuck-${process.pid}-${Date.now()}.txt`);
      let confirmCallCount = 0;
      const extCtx = {
        cwd: root,
        hasUI: true,
        ui: {
          confirm: (_title: string, _message: string) => {
            confirmCallCount += 1;
            return new Promise<boolean>(() => {
              /* never resolves */
            });
          },
        },
      };

      await expect(
        executeTool(tools.get("write")!, { filePath: externalPath, content: "x" }, extCtx as never),
      ).rejects.toThrow(/Permission denied.*timed out/);
      expect(confirmCallCount).toBe(1);
      expect(calls).toEqual([]);
    } finally {
      if (previous === undefined) {
        delete process.env.AFT_PI_EXTERNAL_PROMPT_TIMEOUT_MS;
      } else {
        process.env.AFT_PI_EXTERNAL_PROMPT_TIMEOUT_MS = previous;
      }
    }
  });

  test("formatReadFooter only hints when Rust clamped an unbounded read", () => {
    expect(
      formatReadFooter(false, { truncated: true, start_line: 1, end_line: 100, total_lines: 500 }),
    ).toBe("\n(Showing lines 1-100 of 500. Use offset/limit to read other sections.)");
    expect(
      formatReadFooter(true, { truncated: true, start_line: 1, end_line: 100, total_lines: 500 }),
    ).toBe("");
    expect(formatReadFooter(false, { truncated: true })).toBe("");
  });
});

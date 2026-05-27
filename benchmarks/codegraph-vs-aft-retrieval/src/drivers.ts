import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { BinaryBridge, ensureOnnxRuntime } from "../../../packages/aft-bridge/src/index.ts";
import type { DriverContext, RankedResult, RetrievalCase, RetrievalDriver } from "./types";
import { HARNESS_DIR, normalizePath, runCommand } from "./util";

type BridgeResponse = Record<string, unknown>;

type PlainObject = Record<string, unknown>;

export function createDriver(name: string, context: DriverContext): RetrievalDriver {
  if (name === "aft") return new AftRetrievalDriver(context);
  if (name === "codegraph") return new CodeGraphRetrievalDriver(context);
  throw new Error(`Unknown retrieval driver: ${name}`);
}

class AftRetrievalDriver implements RetrievalDriver {
  readonly name = "aft" as const;
  private bridge: BinaryBridge | null = null;

  constructor(private readonly context: DriverContext) {}

  async prepare(): Promise<void> {
    if (!existsSync(this.context.aftBinary)) {
      throw new Error(`AFT binary not found: ${this.context.aftBinary}`);
    }
    const storageDir = process.env.AFT_STORAGE_DIR ?? join(homedir(), ".cache", "aft");
    const ortDir = process.env.ORT_DYLIB_PATH
      ? dirname(process.env.ORT_DYLIB_PATH)
      : await ensureOnnxRuntime(storageDir);
    const warmup = await runCommand(
      [
        this.context.aftBinary,
        "warmup",
        "--root",
        this.context.codebasePath,
        "--timeout",
        String(this.context.timeoutMs),
        "--quiet",
      ],
      HARNESS_DIR,
      this.context.timeoutMs,
      {
        AFT_STORAGE_DIR: storageDir,
        FASTEMBED_CACHE_DIR: join(storageDir, "semantic", "models"),
      },
    );
    if (warmup.exitCode !== 0) {
      throw new Error(`aft warmup failed: ${warmup.stderr || warmup.stdout}`);
    }

    this.bridge = new BinaryBridge(
      this.context.aftBinary,
      this.context.codebasePath,
      { timeoutMs: this.context.timeoutMs, errorPrefix: "[aft-vs-codegraph-retrieval]" },
      {
        harness: "opencode",
        storage_dir: storageDir,
        search_index: true,
        semantic_search: true,
        experimental_search_index: true,
        experimental_semantic_search: true,
        ...(ortDir ? { _ort_dylib_dir: ortDir } : {}),
      },
    );
    const response = await this.bridge.send(
      "configure",
      {
        project_root: this.context.codebasePath,
        harness: "opencode",
        storage_dir: storageDir,
        search_index: true,
        semantic_search: true,
        experimental_search_index: true,
        experimental_semantic_search: true,
        ...(ortDir ? { _ort_dylib_dir: ortDir } : {}),
      },
      { timeoutMs: this.context.timeoutMs },
    );
    assertBridgeSuccess("configure", response);
    await this.waitForReady();
  }

  async run(testCase: RetrievalCase): Promise<{ items: RankedResult[] }> {
    const response = await this.send("semantic_search", {
      query: testCase.query,
      top_k: this.context.topK,
      hint: testCase.hint ?? (testCase.mode === "search" ? "semantic" : "auto"),
    });
    assertBridgeSuccess("semantic_search", response);
    const results = Array.isArray(response.results) ? response.results : [];
    return {
      items: results.slice(0, this.context.topK).map((result, index) =>
        aftResultToItem(result, index + 1, this.context.codebasePath),
      ),
    };
  }

  async close(): Promise<void> {
    await this.bridge?.shutdown();
    this.bridge = null;
  }

  private async waitForReady(): Promise<void> {
    const deadline = performance.now() + this.context.timeoutMs;
    let lastStatus: BridgeResponse = {};
    while (performance.now() < deadline) {
      const response = await this.send("status", {});
      assertBridgeSuccess("status", response);
      lastStatus = response;
      const searchStatus = nestedStatus(response.search_index);
      const semanticStatus = nestedStatus(response.semantic_index);
      if (searchStatus === "failed" || semanticStatus === "failed") {
        throw new Error(`AFT index failed: ${JSON.stringify(response)}`);
      }
      if ((searchStatus === "ready" || searchStatus === undefined) && semanticStatus === "ready") return;
      await Bun.sleep(500);
    }
    throw new Error(`AFT indexes did not become ready: ${JSON.stringify(lastStatus)}`);
  }

  private async send(command: string, params: Record<string, unknown>): Promise<BridgeResponse> {
    if (!this.bridge) throw new Error("AFT bridge is not prepared");
    return this.bridge.send(command, params, { timeoutMs: this.context.timeoutMs });
  }
}

class CodeGraphRetrievalDriver implements RetrievalDriver {
  readonly name = "codegraph" as const;

  constructor(private readonly context: DriverContext) {}

  async prepare(): Promise<void> {
    const status = await runCommand(
      ["codegraph", "status", this.context.codebasePath, "--json"],
      HARNESS_DIR,
      this.context.timeoutMs,
      codegraphEnv(),
    );
    const initialized = status.exitCode === 0 && statusReportsInitialized(status.stdout);
    if (!initialized) {
      const init = await runCommand(
        ["codegraph", "init", this.context.codebasePath],
        HARNESS_DIR,
        this.context.timeoutMs,
        codegraphEnv(),
      );
      if (init.exitCode !== 0) throw new Error(`codegraph init failed: ${init.stderr || init.stdout}`);
    }
    const index = await runCommand(
      ["codegraph", "index", this.context.codebasePath, "--quiet"],
      HARNESS_DIR,
      this.context.timeoutMs,
      codegraphEnv(),
    );
    if (index.exitCode !== 0) throw new Error(`codegraph index failed: ${index.stderr || index.stdout}`);
  }

  async run(testCase: RetrievalCase): Promise<{ items: RankedResult[] }> {
    if (testCase.mode === "search") return this.runSearch(testCase);
    return this.runContext(testCase);
  }

  private async runSearch(testCase: RetrievalCase): Promise<{ items: RankedResult[] }> {
    const args = [
      "codegraph",
      "query",
      testCase.query,
      "--path",
      this.context.codebasePath,
      "--limit",
      String(this.context.topK),
      "--json",
    ];
    if (testCase.kind) args.push("--kind", normalizeCodeGraphKind(testCase.kind));
    const result = await runCommand(args, HARNESS_DIR, this.context.timeoutMs, codegraphEnv());
    if (result.exitCode !== 0) throw new Error(result.stderr || result.stdout || "codegraph query failed");
    const parsed = JSON.parse(result.stdout || "[]") as unknown[];
    return {
      items: parsed.slice(0, this.context.topK).map((row, index) =>
        codegraphSearchRowToItem(row, index + 1, this.context.codebasePath),
      ),
    };
  }

  private async runContext(testCase: RetrievalCase): Promise<{ items: RankedResult[] }> {
    const result = await runCommand(
      [
        "codegraph",
        "context",
        testCase.query,
        "--path",
        this.context.codebasePath,
        "--max-nodes",
        String(Math.max(this.context.topK, 20)),
        "--max-code",
        "8",
        "--format",
        "markdown",
      ],
      HARNESS_DIR,
      this.context.timeoutMs,
      codegraphEnv(),
    );
    if (result.exitCode !== 0) throw new Error(result.stderr || result.stdout || "codegraph context failed");
    return { items: markdownToItems(result.stdout, this.context.topK, this.context.codebasePath) };
  }
}

function statusReportsInitialized(stdout: string): boolean {
  try {
    const parsed = JSON.parse(stdout) as { initialized?: boolean };
    return parsed.initialized === true;
  } catch {
    return false;
  }
}

function aftResultToItem(result: unknown, rank: number, codebasePath: string): RankedResult {
  const row = plainObject(result);
  return {
    rank,
    file: normalizePath(stringValue(row.file) ?? stringValue(row.file_path), codebasePath),
    symbol: stringValue(row.name),
    kind: stringValue(row.kind),
    startLine: numberValue(row.start_line) ?? numberValue(row.line_start),
    endLine: numberValue(row.end_line) ?? numberValue(row.line_end),
    score: numberValue(row.score),
    text: stringValue(row.snippet) ?? stringValue(row.text),
  };
}

function codegraphSearchRowToItem(row: unknown, rank: number, codebasePath: string): RankedResult {
  const outer = plainObject(row);
  const node = plainObject(outer.node);
  return {
    rank,
    file: normalizePath(stringValue(node.filePath) ?? stringValue(node.file_path), codebasePath),
    symbol: stringValue(node.name),
    kind: stringValue(node.kind),
    startLine: numberValue(node.startLine) ?? numberValue(node.start_line),
    endLine: numberValue(node.endLine) ?? numberValue(node.end_line),
    score: numberValue(outer.score),
    text: [stringValue(node.signature), stringValue(node.name)].filter(Boolean).join("\n"),
  };
}

function markdownToItems(markdown: string, topK: number, codebasePath: string): RankedResult[] {
  const items: RankedResult[] = [];
  const seen = new Set<string>();
  const pathLine = /([A-Za-z0-9_.@+\-/]+\.(?:ts|tsx|js|jsx|rs|py|go|java|kt|swift|rb|php|c|cc|cpp|h|hpp))(?:[:#](\d+))?/g;
  for (const match of markdown.matchAll(pathLine)) {
    const file = normalizePath(resolve(codebasePath, match[1]!), codebasePath);
    const line = match[2] ? Number.parseInt(match[2], 10) : undefined;
    const key = `${file}:${line ?? ""}`;
    if (!file || seen.has(key)) continue;
    seen.add(key);
    items.push({ rank: items.length + 1, file, startLine: line, text: surroundingLine(markdown, match.index ?? 0) });
    if (items.length >= topK) return items;
  }

  const symbolLine = /^#{2,5}\s+(.+)$|^[-*]\s+`?([A-Za-z_$][\w$:.-]*)`?/gm;
  for (const match of markdown.matchAll(symbolLine)) {
    const symbol = cleanSymbol(match[1] ?? match[2] ?? "");
    if (!symbol) continue;
    const key = `symbol:${symbol}`;
    if (seen.has(key)) continue;
    seen.add(key);
    items.push({ rank: items.length + 1, symbol, text: surroundingLine(markdown, match.index ?? 0) });
    if (items.length >= topK) return items;
  }
  return items;
}

function surroundingLine(text: string, index: number): string {
  const start = text.lastIndexOf("\n", index) + 1;
  const end = text.indexOf("\n", index);
  return text.slice(start, end === -1 ? undefined : end).trim();
}

function cleanSymbol(value: string): string | undefined {
  const cleaned = value.replace(/[`*_#]/g, "").trim();
  const match = /([A-Za-z_$][\w$]*(?:::[A-Za-z_$][\w$]*)?)$/.exec(cleaned);
  return match?.[1];
}

function normalizeCodeGraphKind(kind: string): string {
  if (kind === "interface") return "interface";
  if (kind === "enum") return "enum";
  return kind;
}

function codegraphEnv(): Record<string, string> {
  return {
    CI: "1",
    CODEGRAPH_NO_WATCH: "1",
    CODEGRAPH_NO_DAEMON: "1",
    CODEGRAPH_WATCH_DEBOUNCE_MS: "100",
  };
}

function assertBridgeSuccess(command: string, response: BridgeResponse): void {
  if (response.success === false) {
    throw new Error(`${command} failed: ${stringValue(response.message) ?? JSON.stringify(response)}`);
  }
}

function nestedStatus(value: unknown): string | undefined {
  return stringValue(plainObject(value).status);
}

function plainObject(value: unknown): PlainObject {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as PlainObject) : {};
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

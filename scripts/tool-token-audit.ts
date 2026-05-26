#!/usr/bin/env bun
/**
 * Audit token costs of agent-facing tool/param descriptions.
 *
 * Pipes extracted strings through `cargo --example count_stdin` (Claude
 * lookup-encoding tokenizer in `crates/aft-tokenizer`), so the numbers
 * here are bit-exact for the model most agents are.
 *
 * Usage: bun run scripts/tool-token-audit.ts
 *
 * One-shot dev helper — not part of the published surface.
 */

import { readFileSync, readdirSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = dirname(SCRIPT_DIR);

interface ToolFile {
  path: string;
  source: string;
}

interface Description {
  tool: string;
  param: string | null; // null for tool description, otherwise param name
  text: string;
}

/**
 * Recursively walk a directory and return all .ts files (skipping __tests__).
 */
function walkTsFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name.startsWith(".") || entry.name === "__tests__") continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...walkTsFiles(full));
    } else if (entry.isFile() && entry.name.endsWith(".ts")) {
      out.push(full);
    }
  }
  return out;
}

/**
 * Extract every multi-line backtick string from source. Robust to nested
 * `${...}` interpolations by tracking ${ } depth so we don't terminate
 * the string early at a `${"...string..."}` inner backtick.
 */
function extractBacktickStrings(source: string): string[] {
  const out: string[] = [];
  let i = 0;
  while (i < source.length) {
    const ch = source[i];
    if (ch === "`") {
      let j = i + 1;
      let braceDepth = 0;
      let buf = "";
      while (j < source.length) {
        const c = source[j];
        if (c === "\\") {
          buf += source.slice(j, j + 2);
          j += 2;
          continue;
        }
        if (c === "$" && source[j + 1] === "{") {
          braceDepth++;
          buf += "${";
          j += 2;
          continue;
        }
        if (c === "}" && braceDepth > 0) {
          braceDepth--;
          buf += "}";
          j += 1;
          continue;
        }
        if (c === "`" && braceDepth === 0) {
          out.push(buf);
          i = j + 1;
          break;
        }
        buf += c;
        j += 1;
      }
      if (j >= source.length) {
        i = j;
        break;
      }
    } else {
      i += 1;
    }
  }
  return out;
}

/**
 * For a captured string, unescape common JS string escapes back to the
 * plain text the agent ultimately sees. We don't unwind `${...}`
 * interpolations — those become the literal interpolation source, which
 * is good enough for token estimation (interpolated values like
 * `${grepName}` are short).
 */
function unescapeJsString(s: string): string {
  return s
    .replace(/\\n/g, "\n")
    .replace(/\\t/g, "\t")
    .replace(/\\r/g, "\r")
    .replace(/\\"/g, '"')
    .replace(/\\'/g, "'")
    .replace(/\\`/g, "`")
    .replace(/\\\\/g, "\\");
}

/**
 * Extract the contents of a `description: "..."` or `description: \`...\``
 * field at the top level of an object literal. Returns the raw string
 * contents (not the surrounding quotes).
 *
 * Supports string concatenation via `+` for the OpenCode/Pi pattern:
 *   description: "...\\n" + "...\\n" + ...
 */
function findDescriptions(
  source: string,
): { startIndex: number; text: string }[] {
  const out: { startIndex: number; text: string }[] = [];
  const re = /\bdescription\s*:\s*/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(source)) !== null) {
    const start = m.index + m[0].length;
    const parsed = parseStringExpression(source, start);
    if (parsed) {
      out.push({ startIndex: m.index, text: parsed.text });
    }
  }
  return out;
}

/**
 * Parse a JS string expression starting at index `start`. Handles:
 *   - "double-quoted"
 *   - 'single-quoted'
 *   - `backtick`
 *   - any of the above joined by ` + ` for multi-line literal concat
 *
 * Returns the concatenated raw text and the index after the expression.
 */
function parseStringExpression(
  source: string,
  start: number,
): { text: string; end: number } | null {
  let i = start;
  let combined = "";
  while (i < source.length) {
    // Skip whitespace and `+` continuation tokens
    while (i < source.length && /[\s+]/.test(source[i] ?? "")) i++;
    if (i >= source.length) break;
    const ch = source[i];
    if (ch === '"' || ch === "'") {
      const { text, end } = parseQuotedString(source, i, ch);
      combined += unescapeJsString(text);
      i = end;
    } else if (ch === "`") {
      const { text, end } = parseBacktickString(source, i);
      combined += unescapeJsString(text);
      i = end;
    } else {
      break;
    }
    // After a string literal, peek ahead: if next non-whitespace is `+` followed
    // by a string literal, continue; otherwise stop.
    let look = i;
    while (look < source.length && /\s/.test(source[look] ?? "")) look++;
    if (source[look] === "+") {
      let look2 = look + 1;
      while (look2 < source.length && /\s/.test(source[look2] ?? "")) look2++;
      const next = source[look2];
      if (next === '"' || next === "'" || next === "`") {
        i = look + 1;
        continue;
      }
    }
    break;
  }
  return combined ? { text: combined, end: i } : null;
}

function parseQuotedString(
  source: string,
  start: number,
  quote: string,
): { text: string; end: number } {
  let i = start + 1;
  let buf = "";
  while (i < source.length) {
    const c = source[i];
    if (c === "\\") {
      buf += source.slice(i, i + 2);
      i += 2;
      continue;
    }
    if (c === quote) {
      return { text: buf, end: i + 1 };
    }
    buf += c;
    i += 1;
  }
  return { text: buf, end: i };
}

function parseBacktickString(
  source: string,
  start: number,
): { text: string; end: number } {
  let i = start + 1;
  let buf = "";
  let braceDepth = 0;
  while (i < source.length) {
    const c = source[i];
    if (c === "\\") {
      buf += source.slice(i, i + 2);
      i += 2;
      continue;
    }
    if (c === "$" && source[i + 1] === "{") {
      braceDepth++;
      buf += "${";
      i += 2;
      continue;
    }
    if (c === "}" && braceDepth > 0) {
      braceDepth--;
      buf += "}";
      i += 1;
      continue;
    }
    if (c === "`" && braceDepth === 0) {
      return { text: buf, end: i + 1 };
    }
    buf += c;
    i += 1;
  }
  return { text: buf, end: i };
}

/**
 * Find `.describe("...")` calls. We don't try to associate each one with
 * a specific param name — that would require a real AST parser. Instead
 * we attempt to peek backward at the surrounding context for a heuristic
 * label (the line above usually has the param name).
 */
function findDescribeCalls(
  source: string,
): { startIndex: number; text: string; paramHint: string }[] {
  const out: { startIndex: number; text: string; paramHint: string }[] = [];
  const re = /\.describe\s*\(\s*/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(source)) !== null) {
    const start = m.index + m[0].length;
    const parsed = parseStringExpression(source, start);
    if (parsed) {
      // Look back ~200 chars for a param name. Pattern is usually
      //   paramName: z.something()...describe("text")
      // or
      //   paramName: arg(...describe("text"))
      const before = source.slice(Math.max(0, m.index - 400), m.index);
      const paramMatch = before.match(
        /(?:^|[\s,{(])([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(?:arg\s*\(\s*)?[zT]\.[\s\S]*$/,
      );
      const paramHint = paramMatch?.[1] ?? "?";
      out.push({ startIndex: m.index, text: parsed.text, paramHint });
    }
  }
  return out;
}

/**
 * Find TypeBox `Type.String({ description: "..." })` and StringEnum forms
 * for Pi. Returns label + extracted text.
 */
function findTypeboxDescriptions(
  source: string,
): { startIndex: number; text: string; paramHint: string }[] {
  const out: { startIndex: number; text: string; paramHint: string }[] = [];
  // Match `description:` only inside Type.*( ... ) calls. The greedy way
  // is to find any description: inside a Type.X( call. Looser than the
  // Zod path but works for Pi.
  const re = /\bdescription\s*:\s*/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(source)) !== null) {
    const before = source.slice(Math.max(0, m.index - 400), m.index);
    if (!/Type\.\w+\s*\(/.test(before) && !/StringEnum\s*\(/.test(before)) continue;
    const start = m.index + m[0].length;
    const parsed = parseStringExpression(source, start);
    if (!parsed) continue;
    const paramMatch = before.match(
      /(?:^|[\s,{(])([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(?:Type\.Optional\s*\(\s*)?(?:Type\.\w+|StringEnum)\s*\(/,
    );
    out.push({
      startIndex: m.index,
      text: parsed.text,
      paramHint: paramMatch?.[1] ?? "?",
    });
  }
  return out;
}

/**
 * Detect the nearest preceding tool name. Heuristic:
 *  - look for `aft_<name>:` (OpenCode plugin tools object)
 *  - look for `read:`, `write:`, `edit:`, `grep:`, `glob:`, `bash:`,
 *    `bash_status:`, etc. (hoisted tools)
 *  - look for `name: "aft_zoom"` (Pi pi.registerTool)
 *  - fall back to constant names like `BASH_DESCRIPTION` / `MOVE_DESCRIPTION`
 */
function detectTool(source: string, position: number, fileHint: string): string {
  const before = source.slice(0, position);

  // Pi pattern: pi.registerTool({ name: "...", ... description: ... })
  const piMatches = [
    ...before.matchAll(/pi\.registerTool\s*\(\s*\{[^}]*?name\s*:\s*["']([^"']+)["']/g),
  ];
  const piLast = piMatches[piMatches.length - 1];

  // OpenCode pattern: aft_xxx: { description: ... } or `ast_grep_xxx:`
  const ocMatches = [...before.matchAll(/\b(aft_[a-zA-Z_]+|ast_grep_search|ast_grep_replace|lsp_diagnostics)\s*:\s*\{/g)];
  const ocLast = ocMatches[ocMatches.length - 1];

  // XXX_DESCRIPTION constants (hoisted tools store description in a const)
  const hoistConst = [...before.matchAll(/\bconst\s+(\w+_DESCRIPTION)\s*=/g)];
  const hoistLast = hoistConst[hoistConst.length - 1];

  // getEditDescription / getXxxDescription functions
  const fnConst = [...before.matchAll(/\bfunction\s+(get\w+Description)\s*\(/g)];
  const fnLast = fnConst[fnConst.length - 1];

  // Hoisted tool object literals: bareword tool keys or [hoisting ? "name" : "..."]
  const hoistedKey = [
    ...before.matchAll(
      /(?:^|[\s,{])(read|write|edit|apply_patch|grep|glob|bash|bash_status|bash_kill|bash_watch|bash_write|aft_read|aft_write|aft_edit|aft_apply_patch|aft_grep|aft_glob|aft_bash)\s*:\s*\{/g,
    ),
  ];
  const hoistedLast = hoistedKey[hoistedKey.length - 1];

  // [hoisting ? "name" : "..."] ternary pattern from search.ts
  const ternaryKey = [
    ...before.matchAll(/\[hoisting\s*\?\s*["'](\w+)["']\s*:\s*["']aft_\w+["']\s*\]\s*:\s*\w+Tool/g),
  ];
  const ternaryLast = ternaryKey[ternaryKey.length - 1];

  // `function createXxxTool` — gives the tool a synthetic key when nothing else matched
  const createFn = [...before.matchAll(/\bfunction\s+create(\w+)Tool/g)];
  const createLast = createFn[createFn.length - 1];

  // Bash second/third tool: function createBashStatusTool / createBashKillTool
  // are factory functions that produce a single tool definition — when we're
  // inside one and haven't matched a tool literal, use the function name.

  const candidates: Array<{ name: string; idx: number }> = [];
  if (piLast) candidates.push({ name: piLast[1] ?? "?", idx: piLast.index ?? 0 });
  if (ocLast) candidates.push({ name: ocLast[1] ?? "?", idx: ocLast.index ?? 0 });
  if (hoistedLast)
    candidates.push({ name: hoistedLast[1] ?? "?", idx: hoistedLast.index ?? 0 });
  if (ternaryLast)
    candidates.push({ name: ternaryLast[1] ?? "?", idx: ternaryLast.index ?? 0 });
  if (hoistLast) {
    const constName = hoistLast[1] ?? "?";
    const mapped: Record<string, string> = {
      READ_DESCRIPTION: "read",
      APPLY_PATCH_DESCRIPTION: "apply_patch",
      DELETE_DESCRIPTION: "aft_delete",
      MOVE_DESCRIPTION: "aft_move",
      BASH_DESCRIPTION: "bash",
    };
    candidates.push({
      name: mapped[constName] ?? constName.toLowerCase(),
      idx: hoistLast.index ?? 0,
    });
  }
  if (fnLast) {
    const fnName = fnLast[1] ?? "?";
    const stripped = fnName.replace(/^get/, "").replace(/Description$/, "");
    const mapped: Record<string, string> = { Edit: "edit", Read: "read", Write: "write" };
    candidates.push({
      name: mapped[stripped] ?? stripped.toLowerCase(),
      idx: fnLast.index ?? 0,
    });
  }
  if (createLast) {
    // createBashStatusTool → bash_status, createBashKillTool → bash_kill, etc.
    const camel = createLast[1] ?? "?";
    const snake = camel.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
    candidates.push({ name: snake, idx: createLast.index ?? 0 });
  }

  // Workflow hints — special-case the file
  if (fileHint.endsWith("workflow-hints.ts")) {
    return "workflow-hints";
  }

  // Filename fallback — for files that register tools via `const xxxTool: ToolDefinition = {}`
  // patterns my regex doesn't easily attribute. Picks based on what each
  // file is known to register.
  const fileFallback: Record<string, string[]> = {
    "search.ts": ["grep", "glob"],
    "ast.ts": ["ast_grep_search", "ast_grep_replace"],
    "lsp.ts": ["lsp_diagnostics"],
    "semantic.ts": ["aft_search"],
    "conflicts.ts": ["aft_conflicts"],
    "safety.ts": ["aft_safety"],
  };
  const fileName = fileHint.split("/").pop() ?? "";
  const fallbackList = fileFallback[fileName];
  if (fallbackList && fallbackList.length > 0) {
    // For single-tool files, all descriptions belong to that tool.
    // For multi-tool files (search.ts has grep+glob, ast.ts has ast_grep_search+ast_grep_replace),
    // pick based on position within file — first half vs second half — using sloppy heuristic.
    if (fallbackList.length === 1) {
      return fallbackList[0] ?? "?";
    }
    // For 2-tool files, split on position: first half → first tool, second → second
    const fileLength = source.length;
    const idx = position < fileLength / 2 ? 0 : 1;
    return fallbackList[idx] ?? "?";
  }

  if (candidates.length === 0) return "?";
  candidates.sort((a, b) => b.idx - a.idx);
  return candidates[0]?.name ?? "?";
}

function describeFile(file: string): Description[] {
  const source = readFileSync(file, "utf8");
  const out: Description[] = [];

  for (const { startIndex, text } of findDescriptions(source)) {
    const tool = detectTool(source, startIndex, file);
    out.push({ tool, param: null, text: unescapeJsString(text) });
  }

  for (const { startIndex, text, paramHint } of findDescribeCalls(source)) {
    const tool = detectTool(source, startIndex, file);
    out.push({ tool, param: paramHint, text: unescapeJsString(text) });
  }

  for (const { startIndex, text, paramHint } of findTypeboxDescriptions(source)) {
    const tool = detectTool(source, startIndex, file);
    out.push({ tool, param: paramHint, text: unescapeJsString(text) });
  }

  return out;
}

interface TokenResult {
  label: string;
  tokens: number;
}

function countTokensBatch(items: { label: string; text: string }[]): TokenResult[] {
  if (items.length === 0) return [];
  const binPath = join(
    REPO_ROOT,
    "target/release/examples/count_stdin",
  );
  const input = items.map((it) => JSON.stringify(it)).join("\n") + "\n";
  const result = spawnSync(binPath, [], {
    input,
    encoding: "utf8",
    maxBuffer: 50 * 1024 * 1024,
  });
  if (result.status !== 0) {
    console.error("count_stdin failed:", result.stderr);
    process.exit(1);
  }
  return result.stdout
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line) as TokenResult);
}

interface ToolBreakdown {
  tool: string;
  description: number;
  paramsCount: number;
  paramsTokens: number;
  total: number;
  largestParam?: { name: string; tokens: number };
}

function main() {
  const surfaces = [
    {
      label: "OpenCode",
      root: join(REPO_ROOT, "packages/opencode-plugin/src/tools"),
      extraFiles: [join(REPO_ROOT, "packages/opencode-plugin/src/workflow-hints.ts")],
    },
    {
      label: "Pi",
      root: join(REPO_ROOT, "packages/pi-plugin/src/tools"),
      extraFiles: [join(REPO_ROOT, "packages/pi-plugin/src/workflow-hints.ts")],
    },
  ];

  for (const surface of surfaces) {
    console.log(`\n=== ${surface.label} ===\n`);
    const files = walkTsFiles(surface.root).concat(surface.extraFiles);

    // Collect descriptions across all files
    const allDescs: (Description & { sourceFile: string })[] = [];
    for (const f of files) {
      for (const d of describeFile(f)) {
        allDescs.push({ ...d, sourceFile: relative(REPO_ROOT, f) });
      }
    }

    // Tokenize in one batch
    const tokenItems = allDescs.map((d, i) => ({
      label: `${i}`,
      text: d.text,
    }));
    const tokens = countTokensBatch(tokenItems);
    const tokenMap = new Map(tokens.map((t) => [t.label, t.tokens]));

    // Group by tool
    const byTool = new Map<string, ToolBreakdown>();
    for (let i = 0; i < allDescs.length; i++) {
      const d = allDescs[i];
      if (!d) continue;
      const n = tokenMap.get(String(i)) ?? 0;
      let entry = byTool.get(d.tool);
      if (!entry) {
        entry = {
          tool: d.tool,
          description: 0,
          paramsCount: 0,
          paramsTokens: 0,
          total: 0,
        };
        byTool.set(d.tool, entry);
      }
      if (d.param === null) {
        entry.description += n;
      } else {
        entry.paramsCount += 1;
        entry.paramsTokens += n;
        if (!entry.largestParam || n > entry.largestParam.tokens) {
          entry.largestParam = { name: d.param, tokens: n };
        }
      }
      entry.total += n;
    }

    // Sort by total descending
    const rows = [...byTool.values()].sort((a, b) => b.total - a.total);

    // Print table
    const pad = (s: string, n: number) => s.padEnd(n);
    const padL = (s: string, n: number) => s.padStart(n);
    console.log(
      `${pad("tool", 22)} ${padL("desc", 6)} ${padL("params", 6)} ${padL("p-tok", 6)} ${padL("total", 6)}   largest param`,
    );
    console.log("-".repeat(80));
    let descTotal = 0;
    let paramTotal = 0;
    for (const r of rows) {
      const largest =
        r.largestParam !== undefined
          ? `${r.largestParam.name} (${r.largestParam.tokens})`
          : "-";
      console.log(
        `${pad(r.tool, 22)} ${padL(String(r.description), 6)} ${padL(String(r.paramsCount), 6)} ${padL(String(r.paramsTokens), 6)} ${padL(String(r.total), 6)}   ${largest}`,
      );
      descTotal += r.description;
      paramTotal += r.paramsTokens;
    }

    // If --dump-unknown is set, dump all "?" entries with first 80 chars
    if (process.argv.includes("--dump-unknown")) {
      console.log("\n  unattributed entries:");
      for (let i = 0; i < allDescs.length; i++) {
        const d = allDescs[i];
        if (!d || d.tool !== "?") continue;
        const preview = d.text.replace(/\s+/g, " ").slice(0, 80);
        console.log(
          `    [${d.sourceFile}] param=${d.param ?? "(desc)"}  text="${preview}..."`,
        );
      }
    }
    console.log("-".repeat(80));
    console.log(
      `${pad("TOTAL", 22)} ${padL(String(descTotal), 6)} ${padL("", 6)} ${padL(String(paramTotal), 6)} ${padL(String(descTotal + paramTotal), 6)}`,
    );
  }
}

main();

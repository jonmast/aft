import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { delimiter, dirname, join } from "node:path";

export interface AftFixtureBehavior {
  stdout?: string;
  stderr?: string;
  exitCode?: number;
  sleepMs?: number;
}

const compilerSearchPath = process.env.PATH ?? "";
const nativeCompiler = process.platform === "win32" ? null : resolveNativeCompiler();

export function writeAftFixture(path: string, behavior: AftFixtureBehavior): string {
  mkdirSync(dirname(path), { recursive: true });

  if (process.platform === "win32") {
    writeShellFixture(path, behavior);
  } else {
    writeNativeFixture(path, behavior);
  }

  return path;
}

export function writeAftVersionFixture(path: string, version: string): string {
  return writeAftFixture(path, { stdout: `aft ${version}\n` });
}

function writeNativeFixture(path: string, behavior: AftFixtureBehavior): void {
  const sourcePath = `${path}.c`;
  writeFileSync(sourcePath, nativeFixtureSource(behavior), "utf8");

  if (!nativeCompiler)
    throw new Error("Native aft test fixtures are not available on this platform");

  const result = spawnSync(
    nativeCompiler.command,
    [...nativeCompiler.args, sourcePath, "-o", path],
    {
      encoding: "utf8",
      env: { ...process.env, PATH: compilerSearchPath },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );

  if (result.error || result.status !== 0) {
    const detail = [
      result.error instanceof Error ? result.error.message : undefined,
      String(result.stdout ?? "").trim(),
      String(result.stderr ?? "").trim(),
    ]
      .filter(Boolean)
      .join("\n");
    throw new Error(
      `Failed to compile native aft test fixture with ${nativeCompiler.label}: ${detail}`,
    );
  }

  chmodSync(path, 0o755);
}

function nativeFixtureSource(behavior: AftFixtureBehavior): string {
  const stdout = bytesForC(behavior.stdout ?? "");
  const stderr = bytesForC(behavior.stderr ?? "");
  const sleepMs = Math.max(0, Math.trunc(behavior.sleepMs ?? 0));
  const exitCode = Math.trunc(behavior.exitCode ?? 0);

  return `#if !defined(_WIN32)
#define _POSIX_C_SOURCE 199309L
#endif
#include <stdio.h>
#if defined(_WIN32)
#include <windows.h>
#else
#include <errno.h>
#include <time.h>
#endif

static void aft_fixture_sleep(unsigned int milliseconds) {
  if (milliseconds == 0u) return;
#if defined(_WIN32)
  Sleep(milliseconds);
#else
  struct timespec requested;
  requested.tv_sec = milliseconds / 1000u;
  requested.tv_nsec = (long)(milliseconds % 1000u) * 1000000L;
  while (nanosleep(&requested, &requested) == -1 && errno == EINTR) {}
#endif
}

int main(int argc, char **argv) {
  (void)argc;
  (void)argv;
  static const unsigned char stdout_bytes[] = { ${stdout.literal} };
  static const unsigned char stderr_bytes[] = { ${stderr.literal} };

  aft_fixture_sleep(${sleepMs}u);
  if (${stdout.length}u > 0u) {
    fwrite(stdout_bytes, 1u, ${stdout.length}u, stdout);
    fflush(stdout);
  }
  if (${stderr.length}u > 0u) {
    fwrite(stderr_bytes, 1u, ${stderr.length}u, stderr);
    fflush(stderr);
  }

  return ${exitCode};
}
`;
}

function bytesForC(value: string): { literal: string; length: number } {
  const bytes = Buffer.from(value, "utf8");
  return {
    literal: bytes.length > 0 ? [...bytes].map((byte) => `0x${byte.toString(16)}`).join(", ") : "0",
    length: bytes.length,
  };
}

interface NativeCompiler {
  command: string;
  args: string[];
  label: string;
}

function resolveNativeCompiler(): NativeCompiler {
  const candidates = [process.env.CC, "cc", "clang", "gcc"].filter(
    (candidate): candidate is string => Boolean(candidate?.trim()),
  );

  for (const candidate of candidates) {
    const [command, ...args] = candidate.trim().split(/\s+/);
    const resolved = resolveCommand(command);
    if (resolved) return { command: resolved, args, label: candidate };
  }

  return { command: "cc", args: [], label: "cc" };
}

function resolveCommand(command: string): string | null {
  if (command.includes("/") || command.includes("\\")) return existsSync(command) ? command : null;

  for (const dir of compilerSearchPath.split(delimiter)) {
    if (!dir) continue;
    const path = join(dir, command);
    if (existsSync(path)) return path;
  }

  return null;
}

function writeShellFixture(path: string, behavior: AftFixtureBehavior): void {
  const sleepSeconds = Math.ceil(Math.max(0, behavior.sleepMs ?? 0) / 1000);
  const lines = ["#!/bin/sh"];

  if (sleepSeconds > 0) lines.push(`sleep ${sleepSeconds}`);
  if (behavior.stdout) lines.push(`printf '%s' ${shellQuote(behavior.stdout)}`);
  if (behavior.stderr) lines.push(`printf '%s' ${shellQuote(behavior.stderr)} >&2`);
  lines.push(`exit ${Math.trunc(behavior.exitCode ?? 0)}`, "");

  writeFileSync(path, lines.join("\n"), "utf8");
  chmodSync(path, 0o755);
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

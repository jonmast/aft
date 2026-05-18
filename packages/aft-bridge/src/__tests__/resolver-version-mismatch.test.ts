/// <reference path="../bun-test.d.ts" />

/**
 * Resolver version-mismatch test — verifies that `findBinarySync` rejects
 * an npm platform binary whose `--version` does not match the requested
 * `expectedVersion`, and falls through to PATH lookup instead.
 *
 * Regression case (caught during v0.23 Pi RPC e2e dogfooding): a workspace
 * upgraded to plugin v0.22.x can still have a bun-hoisted older
 * `@cortexkit/aft-<platform>` symlink in node_modules (e.g. v0.19.5). The
 * resolver would happily run that older binary, producing stale behavior
 * (in the original repro: `bgb-` task slugs instead of `bash-`).
 *
 * No module mocking — uses a real fake binary directory and writes a shell
 * script that emits a controlled `--version` output. The npm-package
 * resolution leg cannot be exercised without `node_modules/@cortexkit/aft-*`
 * present, so this test focuses on the version-check helper directly.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { findBinarySync, readBinaryVersion } from "../resolver.js";

describe("readBinaryVersion", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-version-test-"));
  });

  afterEach(() => {
    rmSync(tmpDir, { recursive: true, force: true });
  });

  test("parses 'aft 0.22.1' style output", () => {
    const fakeBin = join(tmpDir, "fake-aft.sh");
    writeFileSync(fakeBin, '#!/bin/sh\necho "aft 0.22.1"\n');
    chmodSync(fakeBin, 0o755);
    expect(readBinaryVersion(fakeBin)).toBe("0.22.1");
  });

  test("parses 'aft 0.19.5' (older pre-rename version)", () => {
    const fakeBin = join(tmpDir, "fake-aft.sh");
    writeFileSync(fakeBin, '#!/bin/sh\necho "aft 0.19.5"\n');
    chmodSync(fakeBin, 0o755);
    expect(readBinaryVersion(fakeBin)).toBe("0.19.5");
  });

  test("returns null for empty output", () => {
    const fakeBin = join(tmpDir, "fake-aft.sh");
    writeFileSync(fakeBin, "#!/bin/sh\nexit 0\n");
    chmodSync(fakeBin, 0o755);
    expect(readBinaryVersion(fakeBin)).toBeNull();
  });

  test("parses stderr-only version output when stdout is empty", () => {
    const fakeBin = join(tmpDir, "fake-aft.sh");
    writeFileSync(fakeBin, '#!/bin/sh\necho "aft 0.74.0" >&2\n');
    chmodSync(fakeBin, 0o755);
    expect(readBinaryVersion(fakeBin)).toBe("0.74.0");
  });

  test("returns null for binaries that fail", () => {
    const fakeBin = join(tmpDir, "fake-aft.sh");
    writeFileSync(fakeBin, "#!/bin/sh\nexit 1\n");
    chmodSync(fakeBin, 0o755);
    // Non-zero exit with no stdout is null
    expect(readBinaryVersion(fakeBin)).toBeNull();
  });

  test("returns null when path does not exist", () => {
    expect(readBinaryVersion(join(tmpDir, "does-not-exist"))).toBeNull();
  });

  test("strips 'v' prefix not applied — readBinaryVersion returns bare version", () => {
    // The cache layout uses `v<version>` paths but readBinaryVersion returns
    // the bare version without the `v` prefix. Callers (e.g.
    // findBinarySync's version-mismatch check) compare bare versions, so this
    // is the load-bearing contract: pluginVersion="0.22.1" must equal
    // readBinaryVersion(npm-binary) when no leading "v" is involved.
    const fakeBin = join(tmpDir, "fake-aft.sh");
    writeFileSync(fakeBin, '#!/bin/sh\necho "aft 0.22.1"\n');
    chmodSync(fakeBin, 0o755);
    expect(readBinaryVersion(fakeBin)).toBe("0.22.1"); // not "v0.22.1"
  });
});

describe("findBinarySync versioned cache validation", () => {
  let tmpDir: string;

  beforeEach(() => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-cache-version-test-"));
  });

  afterEach(() => {
    rmSync(tmpDir, { recursive: true, force: true });
  });

  function withResolverEnvironment<T>(callback: () => T): T {
    const prevXdgCacheHome = process.env.XDG_CACHE_HOME;
    const prevPath = process.env.PATH;
    const prevHome = process.env.HOME;

    // Bun runs test files concurrently in one process. Keep process.env
    // overrides scoped to the synchronous resolver call so other async test
    // files cannot clobber XDG_CACHE_HOME between setup and lookup.
    process.env.XDG_CACHE_HOME = tmpDir;
    process.env.PATH = "";
    process.env.HOME = tmpDir;

    try {
      return callback();
    } finally {
      if (prevXdgCacheHome === undefined) delete process.env.XDG_CACHE_HOME;
      else process.env.XDG_CACHE_HOME = prevXdgCacheHome;
      if (prevPath === undefined) delete process.env.PATH;
      else process.env.PATH = prevPath;
      if (prevHome === undefined) delete process.env.HOME;
      else process.env.HOME = prevHome;
    }
  }

  function writeCachedVersion(dirVersion: string, reportedVersion: string): string {
    const binaryPath = join(
      tmpDir,
      "aft",
      "bin",
      dirVersion,
      process.platform === "win32" ? "aft.exe" : "aft",
    );
    mkdirSync(join(tmpDir, "aft", "bin", dirVersion), { recursive: true });
    writeFileSync(binaryPath, `#!/bin/sh\necho "aft ${reportedVersion}"\n`);
    chmodSync(binaryPath, 0o755);
    return binaryPath;
  }

  test("returns exact-version cached binary after probing --version", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "1.2.3");

    // Precondition: the cached binary actually exists at the expected path.
    expect(existsSync(binaryPath)).toBe(true);

    // Precondition: the fake binary actually reports the expected version.
    expect(readBinaryVersion(binaryPath)).toBe("1.2.3");

    expect(withResolverEnvironment(() => findBinarySync("1.2.3"))).toBe(binaryPath);
  });

  test("skips mislabeled newer cached binary instead of accepting directory name", () => {
    const binaryPath = writeCachedVersion("v1.2.3", "9.9.9");

    expect(existsSync(binaryPath)).toBe(true);

    expect(withResolverEnvironment(() => findBinarySync("1.2.3"))).toBeNull();
  });
});

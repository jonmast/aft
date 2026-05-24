/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fetchUrlToTempFile } from "../url-fetch.js";

let storageDir: string;

beforeEach(() => {
  storageDir = mkdtempSync(join(tmpdir(), "aft-url-fetch-"));
});

afterEach(() => {
  rmSync(storageDir, { recursive: true, force: true });
});

describe("fetchUrlToTempFile", () => {
  test("cache hits still enforce the current private-host policy", async () => {
    const privateUrl = "http://127.0.0.1/x";
    const fetchImpl = async () =>
      new Response("# cached private content\n", {
        headers: { "content-type": "text/markdown" },
      });

    await fetchUrlToTempFile(privateUrl, storageDir, {
      allowPrivate: true,
      fetchImpl,
    });

    await expect(
      fetchUrlToTempFile(privateUrl, storageDir, {
        allowPrivate: false,
        fetchImpl,
      }),
    ).rejects.toThrow(/Blocked private URL host/);
  });

  test("aborts body read when stream stalls after headers (no infinite hang)", async () => {
    // Simulate the docs.rs failure mode: server returns 200 + Content-Type, but
    // the body stream never delivers chunks. Before the per-chunk timeout fix,
    // fetchUrlToTempFile would block forever on reader.read(). With the fix,
    // it must throw within BODY_CHUNK_TIMEOUT_MS (real value: 15s; we just
    // assert the call completes — Bun's default test timeout is 5s so a
    // genuine hang would surface as a test-runner timeout failure, but here
    // we explicitly cap the test budget below the production timeout to
    // confirm the abort path is wired correctly without waiting 15s).
    //
    // To keep the test fast, we override the body-read timeout indirectly:
    // create a Response whose body stalls but whose underlying reader is
    // exposed so we can verify the loop terminates. Since BODY_CHUNK_TIMEOUT_MS
    // is internal, we settle for asserting via Promise.race that the fetch
    // call rejects before our own (longer) test budget.
    const stallingBody = new ReadableStream<Uint8Array>({
      // Never enqueue or close — pulls hang forever.
      pull() {},
    });
    const fetchImpl = async () =>
      new Response(stallingBody, {
        headers: { "content-type": "text/markdown" },
      });

    const fetchPromise = fetchUrlToTempFile("http://example.com/stalled", storageDir, {
      allowPrivate: true,
      fetchImpl,
    });

    // The production timeout is 15s. Tests run with the default Bun budget
    // (~5s) — that means a regression would surface as Bun's own test timeout
    // (which we want to distinguish from this assertion). Use a 20s race
    // budget so the test can complete cleanly when the fix is working.
    const testBudgetMs = 20_000;
    const raceTimer = new Promise<"test-budget-exceeded">((resolve) =>
      setTimeout(() => resolve("test-budget-exceeded"), testBudgetMs),
    );
    const outcome = await Promise.race([
      fetchPromise.then(() => "completed" as const).catch((err) => err as Error),
      raceTimer,
    ]);

    expect(outcome).not.toBe("test-budget-exceeded");
    expect(outcome).not.toBe("completed");
    expect((outcome as Error).message).toMatch(/stalled|aborted|Failed to fetch/i);
  }, 25_000);

  test("concurrent same-URL cache misses use independent temp files", async () => {
    const body = "# concurrent\n\nbody\n";
    let fetches = 0;
    const fetchImpl = async () => {
      fetches += 1;
      return new Response(body, {
        headers: { "content-type": "text/markdown" },
      });
    };

    const [first, second] = await Promise.all([
      fetchUrlToTempFile("http://example.com/concurrent", storageDir, {
        allowPrivate: true,
        fetchImpl,
      }),
      fetchUrlToTempFile("http://example.com/concurrent", storageDir, {
        allowPrivate: true,
        fetchImpl,
      }),
    ]);

    expect(first).toBe(second);
    expect(fetches).toBe(2);
    const { readFileSync } = await import("node:fs");
    expect(readFileSync(first, "utf8")).toBe(body);
  });

  test("legitimate body completes normally even with multiple chunks", async () => {
    // Make sure the per-chunk timeout doesn't break the happy path.
    const body = "# heading\n\nbody paragraph with several words to span chunks\n";
    const fetchImpl = async () =>
      new Response(body, {
        headers: { "content-type": "text/markdown" },
      });

    const path = await fetchUrlToTempFile("http://example.com/ok", storageDir, {
      allowPrivate: true,
      fetchImpl,
    });

    expect(path).toMatch(/\.md$/);
    const { readFileSync } = await import("node:fs");
    expect(readFileSync(path, "utf8")).toBe(body);
  });
});

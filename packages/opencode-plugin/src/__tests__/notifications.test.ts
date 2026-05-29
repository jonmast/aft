/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, mock, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import {
  __resetNotificationStateForTests,
  type ConfigureWarning,
  deliverConfigureWarnings,
  getSessionMessages,
  SESSION_MESSAGES_LIMIT,
  sendFeatureAnnouncement,
  sendWarning,
} from "../notifications.js";

const tempRoots = new Set<string>();

function createStorageDir(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-opencode-notifications-"));
  tempRoots.add(root);
  return root;
}

function createClient() {
  const messages: string[] = [];
  const client = {
    session: {
      prompt(input: { body?: { parts?: Array<{ text?: string }> } }): void {
        const text = input.body?.parts?.[0]?.text;
        if (text) messages.push(text);
      },
    },
  };
  return { client, messages };
}

function createStateBridge(initialValue: string | null = null) {
  let value = initialValue;
  const send = mock(async (command: string, params: Record<string, unknown>) => {
    if (command === "db_get_state") {
      return { success: true, data: { value } };
    }
    if (command === "db_set_state") {
      value = params.value as string;
      return { success: true };
    }
    return { success: false };
  });

  return {
    bridge: { send } as unknown as Pick<BinaryBridge, "send">,
    send,
    get value() {
      return value;
    },
  };
}

function createFailingBridge() {
  const send = mock(async () => {
    throw new Error("bridge unavailable");
  });
  return { bridge: { send } as unknown as Pick<BinaryBridge, "send">, send };
}

function dbSetCalls(send: ReturnType<typeof createStateBridge>["send"]) {
  return send.mock.calls
    .filter((call) => call[0] === "db_set_state")
    .map((call) => call[1] as { key: string; value: string });
}

function baseWarning(overrides: Partial<ConfigureWarning> = {}): ConfigureWarning {
  return {
    kind: "formatter_not_installed",
    language: "typescript",
    tool: "biome",
    hint: "Install biome with bun add -d @biomejs/biome.",
    ...overrides,
  };
}

afterEach(() => {
  __resetNotificationStateForTests();
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("Desktop notification session routing", () => {
  test("session-less warnings wait for an explicit session instead of using Desktop last-session state", async () => {
    const { client, messages } = createClient();

    await sendWarning({ client, directory: "/repo" }, "queued warning");
    expect(messages).toHaveLength(0);

    await sendWarning({ client, directory: "/repo", sessionId: "session-1" }, "current warning");

    expect(messages).toHaveLength(2);
    expect(messages[0]).toContain("queued warning");
    expect(messages[1]).toContain("current warning");
  });

  test("session-less feature announcements persist only after queued delivery", async () => {
    const storageDir = createStorageDir();
    // L7 storage commit moved this file under <storageDir>/opencode/.
    const versionFile = join(storageDir, "opencode", "last_announced_version");
    // Pre-seed an older version so this is treated as an UPGRADE, not a
    // fresh install. (Per magic-context#99 / shouldShowAnnouncement, fresh
    // installs are silently suppressed and seeded — the queue/deliver
    // round-trip is only exercised on real upgrades.)
    mkdirSync(join(storageDir, "opencode"), { recursive: true });
    writeFileSync(versionFile, "0.0.1", "utf8");
    const { client, messages } = createClient();

    await sendFeatureAnnouncement(
      { client, directory: "/repo" },
      "9.9.9",
      ["Audit fix"],
      "",
      storageDir,
    );

    expect(messages).toHaveLength(0);
    // Queued, not delivered — marker still records the pre-existing version.
    expect(readFileSync(versionFile, "utf-8")).toBe("0.0.1");

    await sendFeatureAnnouncement(
      { client, directory: "/repo", sessionId: "session-1" },
      "9.9.9",
      ["Audit fix"],
      "",
      storageDir,
    );

    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("New in v9.9.9");
    expect(readFileSync(versionFile, "utf-8")).toBe("9.9.9");
  });

  test("fresh-install session-less call suppresses the announcement and seeds the marker", async () => {
    // Per magic-context#99: a fresh install or ephemeral sandbox (Docker,
    // CI, disposable dev container) has no last_announced_version file
    // yet. The pre-fix behavior queued the changelog dialog and showed it
    // as soon as a session bound — that spammed every sandbox restart and
    // confused first-time users with changelog bullets they had no context
    // to interpret. Post-fix: silently seed and suppress.
    const storageDir = createStorageDir();
    const versionFile = join(storageDir, "opencode", "last_announced_version");
    const { client, messages } = createClient();

    await sendFeatureAnnouncement(
      { client, directory: "/repo" },
      "9.9.9",
      ["Audit fix"],
      "",
      storageDir,
    );

    // Marker is silently seeded so the NEXT launch also stays quiet.
    expect(readFileSync(versionFile, "utf-8")).toBe("9.9.9");

    // Even when a session binds later, no announcement is delivered for
    // this fresh-install version.
    await sendFeatureAnnouncement(
      { client, directory: "/repo", sessionId: "session-1" },
      "9.9.9",
      ["Audit fix"],
      "",
      storageDir,
    );
    expect(messages).toHaveLength(0);
  });
});

describe("deliverConfigureWarnings", () => {
  test("first-time warning delivers via sendIgnoredMessage", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge, send } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("🔧 AFT: ⚠️");
    expect(messages[0]).toContain("Formatter is not installed");
    expect(messages[0]).toContain("Install biome");
    expect(dbSetCalls(send)).toHaveLength(1);
  });

  test("second call with same warning skips delivery", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();
    const opts = {
      client,
      sessionId: "session-1",
      bridge,
      storageDir,
      pluginVersion: "1.0.0",
      projectRoot: "/repo",
    };

    await deliverConfigureWarnings(opts, [baseWarning()]);
    await deliverConfigureWarnings(opts, [baseWarning()]);

    expect(messages).toHaveLength(1);
  });

  test("different warnings deliver independently", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [
        baseWarning(),
        baseWarning({ kind: "checker_not_installed", tool: "tsc", hint: "Install typescript." }),
      ],
    );

    expect(messages).toHaveLength(2);
    expect(messages[0]).toContain("Formatter is not installed");
    expect(messages[1]).toContain("Checker is not installed");
  });

  test("plugin version bump does not re-fire stale warnings", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge, send } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "2.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
    expect(dbSetCalls(send)).toHaveLength(1);
  });

  test("corrupt bridge state and missing storage_dir are non-fatal", async () => {
    const storageDir = createStorageDir();
    const missingStorageDir = join(storageDir, "missing", "nested");
    const { client, messages } = createClient();
    const { bridge } = createStateBridge("not json");

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir: missingStorageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning({ tool: "prettier", hint: "Install prettier." })],
    );

    expect(messages).toHaveLength(2);
  });

  test("lsp_binary_missing warnings dedup across project roots", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();
    const warning = baseWarning({
      kind: "lsp_binary_missing",
      language: undefined,
      tool: undefined,
      server: "typescript-language-server",
      binary: "typescript-language-server",
      hint: "Install `typescript-language-server`.",
    });

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-a",
      },
      [warning],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-b",
      },
      [warning],
    );

    expect(messages).toHaveLength(1);
  });

  test("formatter warnings remain project-scoped", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-a",
      },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-b",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(2);
  });

  test("recordWarning_sends_db_set_state_with_merged_map", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge, send } = createStateBridge();
    const opts = {
      client,
      sessionId: "session-1",
      bridge,
      storageDir,
      pluginVersion: "1.0.0",
      projectRoot: "/repo",
    };

    await deliverConfigureWarnings(opts, [baseWarning()]);
    await deliverConfigureWarnings(opts, [
      baseWarning({ kind: "checker_not_installed", tool: "tsc", hint: "Install typescript." }),
    ]);

    expect(messages).toHaveLength(2);
    const sets = dbSetCalls(send);
    expect(sets).toHaveLength(2);
    const first = JSON.parse(sets[0].value) as Record<string, boolean>;
    expect(sets[0].key).toBe("warned_tools");
    expect(Object.values(first)).toEqual([true]);
    const merged = JSON.parse(sets[1].value) as Record<string, boolean>;
    expect(Object.keys(merged)).toHaveLength(2);
    expect(Object.values(merged)).toEqual([true, true]);
  });

  test("delivers_once_when_state_read_succeeds_with_empty_value", async () => {
    // db_get_state succeeds with a null value (configured bridge, nothing
    // recorded yet) → "fresh" → deliver once.
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createStateBridge(null);

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
  });

  test("does_not_deliver_when_state_read_fails_success_false", async () => {
    // Regression for the "LSP/formatter warning shows every session" bug:
    // when db_get_state returns success:false (bridge not configured yet),
    // the dedup state is UNKNOWN. Delivering anyway is what re-fired the
    // warning every session. The gate must treat unknown as "skip" and let a
    // later configured call deliver once.
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const send = mock(async (command: string) => {
      if (command === "db_get_state") return { success: false };
      return { success: true };
    });
    const bridge = { send } as unknown as Pick<BinaryBridge, "send">;

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(0);
    // And it must NOT record a warning (no blind write that would clobber state).
    expect(send.mock.calls.some((call) => call[0] === "db_set_state")).toBe(false);
  });

  test("skips_redelivery_when_key_already_recorded", async () => {
    const storageDir = createStorageDir();
    const firstClient = createClient();
    const first = createStateBridge();

    await deliverConfigureWarnings(
      {
        client: firstClient.client,
        sessionId: "session-1",
        bridge: first.bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    const { client, messages } = createClient();
    const { bridge } = createStateBridge(first.value);
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        bridge,
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(0);
  });

  test("bridge_error_suppresses_delivery_and_is_non_fatal", async () => {
    // A throwing bridge means the dedup state is UNKNOWN. Previously the
    // gate treated an unreadable state as "never warned" and delivered
    // anyway, which re-fired the same warning every session. Now an
    // unreadable state suppresses delivery (a later configured call delivers
    // once), while remaining non-fatal — deliverConfigureWarnings must still
    // resolve cleanly.
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const { bridge } = createFailingBridge();

    await expect(
      deliverConfigureWarnings(
        {
          client,
          sessionId: "session-1",
          bridge,
          storageDir,
          pluginVersion: "1.0.0",
          projectRoot: "/repo",
        },
        [baseWarning()],
      ),
    ).resolves.toBeUndefined();
    expect(messages).toHaveLength(0);
  });
});

describe("sendFeatureAnnouncement storage", () => {
  test("repairs root-scoped announcement version into opencode harness path", async () => {
    const storageDir = createStorageDir();
    writeFileSync(join(storageDir, "last_announced_version"), "0.30.0", "utf8");
    const showToast = mock(async () => undefined);

    await sendFeatureAnnouncement(
      { client: { tui: { showToast } }, directory: "/repo" },
      "0.30.0",
      ["Feature"],
      "",
      storageDir,
    );

    expect(showToast).not.toHaveBeenCalled();
    expect(existsSync(join(storageDir, "last_announced_version"))).toBe(false);
    expect(readFileSync(join(storageDir, "opencode", "last_announced_version"), "utf8")).toBe(
      "0.30.0",
    );
  });

  test("persists new announcement version under opencode harness path", async () => {
    const storageDir = createStorageDir();
    // Pre-seed an older version so this is an UPGRADE, not a fresh install.
    // Fresh installs are silently suppressed by shouldShowAnnouncement
    // (magic-context#99); only real upgrades fire the toast.
    mkdirSync(join(storageDir, "opencode"), { recursive: true });
    writeFileSync(join(storageDir, "opencode", "last_announced_version"), "0.30.0", "utf8");
    const showToast = mock(async () => undefined);

    await sendFeatureAnnouncement(
      { client: { tui: { showToast } }, directory: "/repo" },
      "0.30.1",
      ["Feature"],
      "",
      storageDir,
    );

    expect(showToast).toHaveBeenCalledTimes(1);
    expect(readFileSync(join(storageDir, "opencode", "last_announced_version"), "utf8")).toBe(
      "0.30.1",
    );
    expect(existsSync(join(storageDir, "last_announced_version"))).toBe(false);
  });
});

// Regression coverage for the unbounded-messages-call bug surfaced by
// OpenCode's plugin agent: legacy `client.session.messages()` without a
// `query.limit` hydrates the entire session. These tests pin the bounded
// contract for the cleanup paths (`sendStatus` auto-delete + `cleanupWarnings`)
// so future edits cannot accidentally drop the limit.
describe("getSessionMessages: bounded SDK call", () => {
  test("sends query.limit on every request", async () => {
    const calls: Array<{ path: { id: string }; query?: { limit?: number } }> = [];
    const client = {
      session: {
        messages: async (input: { path: { id: string }; query?: { limit?: number } }) => {
          calls.push(input);
          return { data: [] };
        },
      },
    };

    await getSessionMessages(client, "session-1");

    expect(calls).toHaveLength(1);
    expect(calls[0].path).toEqual({ id: "session-1" });
    expect(calls[0].query).toBeDefined();
    expect(calls[0].query?.limit).toBe(SESSION_MESSAGES_LIMIT);
  });

  test("limit constant is a small positive integer", () => {
    expect(SESSION_MESSAGES_LIMIT).toBeGreaterThan(0);
    // Defensive ceiling — actual is 50; if it ever grows past 200 we want
    // a deliberate review, not a silent regression toward unboundedness.
    expect(SESSION_MESSAGES_LIMIT).toBeLessThanOrEqual(200);
  });

  test("returns the data array when call succeeds", async () => {
    const fakeMsgs = [{ info: { id: "m1", role: "user" }, parts: [{ type: "text", text: "hi" }] }];
    const client = {
      session: {
        messages: async () => ({ data: fakeMsgs }),
      },
    };
    const result = await getSessionMessages(client, "session-1");
    expect(result).toEqual(fakeMsgs);
  });

  test("returns empty array when client.session.messages is unavailable", async () => {
    expect(await getSessionMessages({}, "session-1")).toEqual([]);
  });

  test("returns empty array when the messages API throws", async () => {
    const client = {
      session: {
        messages: async () => {
          throw new Error("boom");
        },
      },
    };
    expect(await getSessionMessages(client, "session-1")).toEqual([]);
  });
});

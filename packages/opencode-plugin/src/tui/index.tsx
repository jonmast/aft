/** @jsxImportSource @opentui/solid */
// @ts-nocheck

import type { TuiPlugin, TuiPluginApi } from "@opencode-ai/plugin/tui";
import packageJson from "../../package.json";
import { AftRpcClient } from "../shared/rpc-client";
import { coerceAftStatus, formatStatusDialogMessage } from "../shared/status";
import { createAftSidebarSlot } from "./sidebar";

// The TUI talks to the server plugin via AftRpcClient. The client reads the
// JSON port file written by AftRpcServer ({ port, token }) and includes that
// per-server token on every RPC request; legacy integer port files are still
// tolerated for already-running older server plugins.

const STATUS_COMMAND = "aft-status";

// RPC clients keyed by directory — one per project
const rpcClients = new Map<string, AftRpcClient>();

function getRpcClient(directory: string): AftRpcClient {
  let client = rpcClients.get(directory);
  if (client) return client;

  const home = process.env.HOME || process.env.USERPROFILE || "";
  const dataHome = process.env.XDG_DATA_HOME || `${home}/.local/share`;
  const storageDir = `${dataHome}/opencode/storage/plugin/aft`;

  client = new AftRpcClient(storageDir, directory);
  rpcClients.set(directory, client);
  return client;
}

function getSessionId(api: TuiPluginApi): string | null {
  try {
    const route = api.route.current;
    if (route?.name === "session" && route.params?.sessionID) {
      return route.params.sessionID;
    }
  } catch {
    // ignore
  }
  return null;
}

async function showStatusDialog(api: TuiPluginApi): Promise<void> {
  const sessionID = getSessionId(api);
  if (!sessionID) {
    api.ui.toast({ message: "No active session", variant: "warning", duration: 5000 });
    return;
  }

  const directory = api.state.path.directory ?? "";
  if (!directory) {
    api.ui.toast({ message: "No project directory", variant: "warning", duration: 5000 });
    return;
  }

  const client = getRpcClient(directory);

  // Fetch status immediately and show the dialog
  let currentMessage = "Connecting to AFT...";
  try {
    const response = await client.call("status", { sessionID });
    if ((response as Record<string, unknown>).success !== false) {
      const status = coerceAftStatus(response as Record<string, unknown>);
      currentMessage = formatStatusDialogMessage(status);
    }
  } catch {
    currentMessage = "AFT is starting up. Status will refresh automatically...";
  }

  // Track whether dialog is still open for polling cleanup
  let dialogOpen = true;
  let pollTimer: ReturnType<typeof setInterval> | null = null;

  // Show dialog with initial data
  api.ui.dialog.setSize("large");
  api.ui.dialog.replace(
    () => {
      // Start polling after dialog renders
      if (!pollTimer) {
        pollTimer = setInterval(async () => {
          if (!dialogOpen) {
            if (pollTimer) clearInterval(pollTimer);
            return;
          }
          try {
            const response = await client.call("status", { sessionID });
            if ((response as Record<string, unknown>).success !== false) {
              const status = coerceAftStatus(response as Record<string, unknown>);
              const newMessage = formatStatusDialogMessage(status);
              if (newMessage !== currentMessage) {
                currentMessage = newMessage;
                // Re-render dialog with updated status
                api.ui.dialog.replace(
                  () => (
                    <api.ui.DialogAlert
                      title="AFT Status"
                      message={currentMessage}
                      onConfirm={() => {
                        dialogOpen = false;
                        if (pollTimer) clearInterval(pollTimer);
                        api.ui.dialog.setSize("medium");
                      }}
                    />
                  ),
                  () => {
                    dialogOpen = false;
                    if (pollTimer) clearInterval(pollTimer);
                    api.ui.dialog.setSize("medium");
                  },
                );
              }
            }
          } catch {
            // Polling failure is non-fatal — just skip this tick
          }
        }, 1500);
      }

      return (
        <api.ui.DialogAlert
          title="AFT Status"
          message={currentMessage}
          onConfirm={() => {
            dialogOpen = false;
            if (pollTimer) clearInterval(pollTimer);
            api.ui.dialog.setSize("medium");
          }}
        />
      );
    },
    () => {
      dialogOpen = false;
      if (pollTimer) clearInterval(pollTimer);
      api.ui.dialog.setSize("medium");
    },
  );
}

/**
 * Register the `/aft-status` slash command, preferring the v1.14.42+ keymap
 * API and falling back to the legacy `api.command.register` for older hosts.
 *
 * The `keymap.registerLayer` shape uses `name`/`title`/`run`/`namespace`/
 * `slashName` (see `@opencode-ai/plugin/tui` types) and is what the host's
 * own legacy command-shim translates into. Calling it directly skips the
 * deprecation warning and works without depending on the (now-deprecated)
 * `api.command` namespace existing at all.
 */
function registerStatusCommand(api: TuiPluginApi): void {
  type ApiAny = {
    keymap?: {
      registerLayer?: (layer: {
        commands: Array<Record<string, unknown>>;
        bindings: Array<Record<string, unknown>>;
      }) => unknown;
    };
    command?: {
      register?: (cb: () => Array<Record<string, unknown>>) => unknown;
    };
  };
  const apiAny = api as unknown as ApiAny;

  if (typeof apiAny.keymap?.registerLayer === "function") {
    apiAny.keymap.registerLayer({
      commands: [
        {
          namespace: "palette",
          name: "aft.status",
          title: "AFT: Status",
          category: "AFT",
          slashName: STATUS_COMMAND,
          run() {
            void showStatusDialog(api);
          },
        },
      ],
      bindings: [],
    });
    return;
  }

  if (typeof apiAny.command?.register === "function") {
    apiAny.command.register(() => [
      {
        title: "AFT: Status",
        value: "aft.status",
        category: "AFT",
        slash: { name: STATUS_COMMAND },
        onSelect() {
          void showStatusDialog(api);
        },
      },
    ]);
    return;
  }

  // Neither API surface is present. The TUI host can still load — we only
  // lose the slash-command entry point. The sidebar (registered above)
  // remains available so users can still see AFT status visually.
}

async function showStartupNotifications(api: TuiPluginApi): Promise<void> {
  const directory = api.state.path.directory ?? "";
  if (!directory) return;

  const client = getRpcClient(directory);

  // Check for feature announcements
  try {
    const announcement = (await client.call("get-announcement", {})) as {
      show?: boolean;
      version?: string;
      features?: string[];
    };

    if (announcement.show && announcement.version && announcement.features?.length) {
      const featureText = announcement.features.map((f: string) => `  • ${f}`).join("\n");

      api.ui.dialog.replace(
        () => (
          <api.ui.DialogAlert
            title={`AFT v${announcement.version}`}
            message={`What's new:\n\n${featureText}`}
            onConfirm={() => {
              // Mark as announced so it doesn't show again
              void client.call("mark-announced", {});
            }}
          />
        ),
        () => {
          void client.call("mark-announced", {});
        },
      );
      return; // Show one dialog at a time
    }
  } catch {
    // RPC server not ready yet — skip announcements
  }

  // Check for warnings
  try {
    const result = (await client.call("get-warnings", {})) as { warnings?: string[] };
    if (result.warnings?.length) {
      const warningText = result.warnings.join("\n\n");
      api.ui.dialog.replace(
        () => <api.ui.DialogAlert title="AFT Warning" message={warningText} onConfirm={() => {}} />,
        () => {},
      );
    }
  } catch {
    // RPC server not ready — skip warnings
  }
}

const tui: TuiPlugin = async (api) => {
  // Sidebar slot: live status of search index, semantic index, and disk
  // usage. See ./sidebar.tsx for the panel itself. Registered before the
  // command palette entry so the sidebar is available immediately when the
  // user opens their first session.
  try {
    api.slots.register(createAftSidebarSlot(api, packageJson.version));
  } catch {
    // Older OpenCode TUI hosts may not implement api.slots; fall through
    // and keep the slash command working.
  }

  // OpenCode 1.14.42 removed `api.command.register` entirely
  // (anomalyco/opencode PR #26053). Later patches reinstated it as a
  // deprecated shim that translates to `api.keymap.registerLayer`. To work
  // across the whole 1.14.x line — including the brief 1.14.42 / 1.14.43
  // window where neither the legacy API nor a shim was present — we prefer
  // `api.keymap.registerLayer` and fall back to `api.command.register` only
  // when the keymap surface is missing (older hosts that predate keymap).
  // See https://github.com/cortexkit/aft/issues/33.
  registerStatusCommand(api);

  // Show startup notifications — RPC server is already running by the time TUI loads
  void showStartupNotifications(api);
};

const id = "aft-opencode";

export default {
  id,
  tui,
};

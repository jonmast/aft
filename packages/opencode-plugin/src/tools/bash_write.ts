import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBashBridge } from "./_shared.js";

const z = tool.schema;

/**
 * Write bytes to a running PTY background task.
 *
 * Two input shapes:
 *
 *   - **String** — verbatim bytes written to the PTY. Use this for plain text
 *     such as REPL commands (`"print(1)\n"`) or when the agent specifically
 *     wants the literal characters `\u001b` (e.g. writing source code).
 *
 *   - **Sequence array** — mix plain strings (text) with `{ key: "<name>" }`
 *     objects (named control keys). Items concatenate into one atomic write
 *     so the PTY sees the whole sequence as one input chunk. The agent never
 *     has to JSON-encode escape characters.
 *
 *     Allowed key names: `enter`, `return`, `tab`, `space`, `backspace`,
 *     `esc`, `escape`, `up`, `down`, `left`, `right`, `home`, `end`,
 *     `page-up`, `page-down`, `delete`, `insert`, `f1`..`f12`,
 *     `ctrl-a`..`ctrl-z`. Names are case-insensitive.
 *
 * The vim "type text, exit insert, save and quit" idiom:
 *
 * ```ts
 * bash_write({ taskId, input: [
 *   "iHello",
 *   { key: "esc" },
 *   ":wq",
 *   { key: "enter" },
 * ]})
 * ```
 *
 * Rust enforces a 1 MiB maximum on the EXPANDED byte stream. Agents should
 * check `bash_status` first and only call `bash_write` when the task reports
 * `mode: "pty"`.
 */
export function createBashWriteTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      'Write input bytes to a running PTY bash task. PTY-only; check bash_status reports mode: "pty" first. ' +
      'Input is either a string (verbatim bytes) or an array mixing strings and { key: "esc" | "enter" | "up" | "ctrl-c" | ... } objects ' +
      'for atomic text+key sequences such as [ "iHello", { key: "esc" }, ":wq", { key: "enter" } ]. ' +
      "Named keys cover enter/return/tab/space/backspace/esc/escape, arrows, home/end/page-up/page-down/delete/insert, f1..f12, and ctrl-a..ctrl-z. " +
      "Maximum 1 MiB per call (post-expansion).",
    args: {
      taskId: z
        .string()
        .describe("Background PTY task ID returned by bash({ pty: true, background: true })."),
      input: z
        .union([
          z.string(),
          z.array(
            z.union([
              z.string(),
              z.object({
                key: z
                  .string()
                  .describe(
                    "Named control key, e.g. 'esc', 'enter', 'up', 'ctrl-c'. Case-insensitive.",
                  ),
              }),
            ]),
          ),
        ])
        .describe(
          "Either a string of verbatim bytes (e.g. 'print(1)\\n') OR an array mixing strings " +
            "and { key: '<name>' } objects for atomic text+key sequences. " +
            "Example: [ 'iHello', { key: 'esc' }, ':wq', { key: 'enter' } ].",
        ),
    },
    execute: async (args, context) => {
      const data = await callBashBridge(ctx, context, "bash_write", {
        task_id: args.taskId as string,
        input: args.input as unknown,
      });
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_write failed");
      }
      return JSON.stringify({ bytes_written: data.bytes_written }, null, 2);
    },
  };
}

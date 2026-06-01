/**
 * `npx @cortexkit/aft --version` — report the versions that actually matter
 * for debugging a setup: the CLI/npx package being run, the resolved `aft`
 * binary, and, per harness, the host version + the installed plugin version.
 */

import { probeBinaryVersion } from "../lib/binary-probe.js";
import { getAllRegistryAdapters } from "../lib/harness-select.js";
import { getSelfVersion } from "../lib/self-version.js";

export function runVersion(): number {
  const cliVersion = getSelfVersion();
  const binaryVersion = probeBinaryVersion();

  const lines: string[] = [];
  lines.push("");
  lines.push("  AFT versions");
  lines.push("");
  lines.push(`  @cortexkit/aft (this CLI)   v${cliVersion}`);
  lines.push(`  aft binary                  ${binaryVersion ?? "not installed"}`);
  lines.push("");

  const adapters = getAllRegistryAdapters();
  const labelWidth = Math.max(...adapters.map((adapter) => adapter.displayName.length));
  for (const adapter of adapters) {
    const label = adapter.displayName.padEnd(labelWidth);
    if (!adapter.isInstalled()) {
      lines.push(`  ${label}   host not installed`);
      continue;
    }
    const host = adapter.getHostVersion() ?? "unknown";
    const registered = adapter.hasPluginEntry();
    let pluginPart: string;
    if (!registered) {
      pluginPart = "plugin not registered";
    } else {
      const cached = adapter.getPluginCacheInfo().cached;
      pluginPart = cached ? `plugin ${cached}` : "plugin registered (version unknown)";
    }
    lines.push(`  ${label}   host ${host} · ${pluginPart}`);
  }
  lines.push("");

  console.log(lines.join("\n"));
  return 0;
}

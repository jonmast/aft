import { existsSync, readdirSync, readFileSync, unlinkSync } from "node:fs";
import { join } from "node:path";
import { isPidAlive, parseRpcPortRecord, rpcPortFileDir, rpcPortFilePath } from "./rpc-utils";

const MAX_RETRIES = 10;
const RETRY_DELAY_MS = 500;
const REQUEST_TIMEOUT_MS = 5000;

type PortInfoSource = "instance" | "legacy";
type ParsedPortInfo = { port: number; token: string | null; pid?: number; started_at?: number };
type PortInfo = ParsedPortInfo & { source: PortInfoSource; path?: string };

export interface AftRpcCallOptions {
  signal?: AbortSignal;
}

function abortError(signal: AbortSignal): Error {
  const reason = signal.reason;
  if (reason instanceof Error) return reason;
  return new Error("AFT RPC request aborted");
}

function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted) throw abortError(signal);
}

function delay(ms: number, signal?: AbortSignal): Promise<void> {
  throwIfAborted(signal);
  return new Promise((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout>;
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal ? abortError(signal) : new Error("AFT RPC request aborted"));
    };
    timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

export class AftRpcClient {
  private port: number | null = null;
  private token: string | null = null;
  private portsDir: string;
  private legacyPortFile: string;
  private stalePortFailures = new Map<string, number>();

  constructor(storageDir: string, directory: string) {
    this.portsDir = rpcPortFileDir(storageDir, directory);
    this.legacyPortFile = rpcPortFilePath(storageDir, directory);
  }

  /** Call an RPC method. Retries port resolution if the server isn't ready yet. */
  async call<T = Record<string, unknown>>(
    method: string,
    params: Record<string, unknown> = {},
    options: AftRpcCallOptions = {},
  ): Promise<T> {
    const { signal } = options;
    throwIfAborted(signal);

    // Try ALL discovered ports for this project (OpenCode TUI under --port 0
    // loads our plugin twice in the same process, so two RPC servers listen
    // and we have to try both — only one's bridge is actually warm).
    const infos = await this.resolvePortInfos(signal);
    if (infos.length === 0) {
      throw new Error("AFT RPC server not available");
    }

    // First pass: try every port. Prefer responses that look like "warm
    // bridge" output (i.e. not the synthetic `status: "not_initialized"`
    // placeholder served when this instance's bridge hasn't been spawned).
    let placeholder: T | null = null;
    let lastError: unknown = null;
    for (const info of infos) {
      throwIfAborted(signal);
      try {
        const result = await this.callOne<T>(method, params, info, signal);
        if (this.looksLikePlaceholder(result)) {
          placeholder = result; // remember but keep trying
          continue;
        }
        // Warm response — cache this port for subsequent calls.
        this.port = info.port;
        this.token = info.token;
        return result;
      } catch (err) {
        throwIfAborted(signal);
        lastError = err;
      }
    }

    // All ports returned placeholder OR failed. Use placeholder if we have
    // one (sidebar then shows the lazy-spawn UI); otherwise rethrow last error.
    if (placeholder !== null) return placeholder;
    throw lastError instanceof Error ? lastError : new Error(String(lastError));
  }

  private async callOne<T>(
    method: string,
    params: Record<string, unknown>,
    info: PortInfo,
    signal?: AbortSignal,
  ): Promise<T> {
    const response = await this.fetchWithTimeout(
      `http://127.0.0.1:${info.port}/rpc/${method}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...params, token: info.token }),
      },
      signal,
    );
    if (!response.ok) {
      const text = await response.text();
      throw new Error(`RPC ${method} failed (${response.status}): ${text}`);
    }
    return (await response.json()) as T;
  }

  /**
   * Heuristic for "this response is the lazy-spawn placeholder, not the real
   * data." We treat any `not_initialized` status as a placeholder so the
   * client knows to try the next port (the warm one).
   */
  private looksLikePlaceholder<T>(result: T): boolean {
    if (!result || typeof result !== "object") return false;
    const status = (result as Record<string, unknown>).status;
    return status === "not_initialized";
  }

  /** Check if any RPC server is reachable. */
  async isAvailable(): Promise<boolean> {
    try {
      const infos = await this.resolvePortInfos();
      return infos.length > 0;
    } catch {
      return false;
    }
  }

  /**
   * Discover all live RPC port files for this project. Tries the per-instance
   * directory first (v0.28.2+), then the single legacy `port` file (older
   * plugin versions in mixed deployments) as the final fallback candidate.
   */
  private async resolvePortInfos(signal?: AbortSignal): Promise<PortInfo[]> {
    for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
      throwIfAborted(signal);
      const infos = this.readAllPortFiles();
      if (infos.length > 0) {
        const alive: PortInfo[] = [];
        for (const info of infos) {
          throwIfAborted(signal);
          if (await this.healthCheck(info.port, signal)) {
            this.clearPortFailure(info);
            alive.push(info);
          } else {
            this.recordPortFailure(info);
          }
        }
        if (alive.length > 0) return alive;
      }
      if (attempt < MAX_RETRIES - 1) {
        await delay(RETRY_DELAY_MS, signal);
      }
    }
    return [];
  }

  private readAllPortFiles(): PortInfo[] {
    const collected: PortInfo[] = [];
    const seenPorts = new Set<number>();
    const add = (info: PortInfo) => {
      if (seenPorts.has(info.port)) return;
      seenPorts.add(info.port);
      collected.push(info);
    };

    // Per-instance directory (v0.28.2+): one file per plugin load. Each file now
    // records the owning process pid; we skip (and reclaim) any whose process is
    // dead so a poll doesn't wade through crash/restart leftovers, and we order
    // newest-first so the freshest live server wins the port de-dupe (a stale
    // file naming a reused port can no longer mask a fresh one with its old token).
    if (existsSync(this.portsDir)) {
      try {
        const live: PortInfo[] = [];
        for (const entry of readdirSync(this.portsDir)) {
          if (!entry.endsWith(".json")) continue;
          const filePath = join(this.portsDir, entry);
          const info = this.parsePortFile(filePath);
          if (!info) continue;
          // Only reclaim when we can PROVE the owner is dead (pid present and not
          // alive). Files without a pid (older format) are kept and fall through
          // to the health check, since we can't prove they're dead.
          if (info.pid !== undefined && !isPidAlive(info.pid)) {
            this.reclaimDeadPortFile(filePath);
            continue;
          }
          live.push({ ...info, source: "instance", path: filePath });
        }
        // Newest first: files with a started_at sort before those without.
        live.sort((a, b) => (b.started_at ?? 0) - (a.started_at ?? 0));
        for (const info of live) add(info);
      } catch {
        // ignore read errors
      }
    }

    // Legacy single file (pre-v0.28.2 plugin versions in mixed deployments).
    // Always append it after per-instance entries so a stale JSON file cannot
    // mask an older live server, then de-dupe by port with per-instance winning.
    const legacyInfo = this.parsePortFile(this.legacyPortFile);
    if (legacyInfo) add({ ...legacyInfo, source: "legacy", path: this.legacyPortFile });

    return collected;
  }

  /** Delete a port file whose owning process is provably dead (best-effort). */
  private reclaimDeadPortFile(filePath: string): void {
    try {
      unlinkSync(filePath);
    } catch {
      // best-effort; a concurrent writer may have already replaced it
    }
  }

  private parsePortFile(filePath: string): ParsedPortInfo | null {
    try {
      const content = readFileSync(filePath, "utf-8");
      return parseRpcPortRecord(content);
    } catch {
      return null;
    }
  }

  private portFailureKey(info: PortInfo): string | null {
    if (info.source !== "instance" || !info.path) return null;
    return `${info.path}\0${info.port}\0${info.token ?? ""}`;
  }

  private clearPortFailure(info: PortInfo): void {
    const key = this.portFailureKey(info);
    if (key) this.stalePortFailures.delete(key);
  }

  private recordPortFailure(info: PortInfo): void {
    const key = this.portFailureKey(info);
    if (!key || !info.path) return;

    const failures = (this.stalePortFailures.get(key) ?? 0) + 1;
    if (failures < 2) {
      this.stalePortFailures.set(key, failures);
      return;
    }

    this.stalePortFailures.delete(key);
    try {
      // Do not unlink a replacement written after the failed health checks.
      const current = this.parsePortFile(info.path);
      if (current?.port === info.port && current.token === info.token) {
        unlinkSync(info.path);
      }
    } catch {
      // best-effort stale cleanup only
    }
  }

  private async healthCheck(port: number, signal?: AbortSignal): Promise<boolean> {
    try {
      const response = await this.fetchWithTimeout(
        `http://127.0.0.1:${port}/health`,
        { method: "GET" },
        signal,
      );
      return response.ok;
    } catch {
      throwIfAborted(signal);
      return false;
    }
  }

  private async fetchWithTimeout(
    url: string,
    options: RequestInit,
    signal?: AbortSignal,
  ): Promise<Response> {
    throwIfAborted(signal);

    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
    const onAbort = () => controller.abort();
    signal?.addEventListener("abort", onAbort, { once: true });
    try {
      return await fetch(url, { ...options, signal: controller.signal });
    } finally {
      clearTimeout(timeout);
      signal?.removeEventListener("abort", onAbort);
    }
  }

  reset(): void {
    this.port = null;
    this.token = null;
    this.stalePortFailures.clear();
  }
}

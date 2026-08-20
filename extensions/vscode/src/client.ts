import * as http from 'node:http';
import type { Log } from './log';
import type { ServerAddress } from './server';
import type { FileStructure, Health, InsightsPayload, ReferencesResult, RefreshResult } from './types';

export class CccHttpError extends Error {
  constructor(
    readonly status: number,
    readonly path: string,
    readonly body: string,
  ) {
    super(`ccc ${path} returned ${status}: ${body.slice(0, 200)}`);
    this.name = 'CccHttpError';
  }
}

export class AbortedError extends Error {
  constructor() {
    super('request aborted');
    this.name = 'AbortedError';
  }
}

export function isAborted(err: unknown): boolean {
  return err instanceof AbortedError;
}

const TIMEOUT_FAST_MS = 5000;
// a cold rescan of a large repo is slow and waiting beats failing
const TIMEOUT_SLOW_MS = 60000;

// the analyser's HTTP surface - `node:http` not `fetch` to bypass the host's proxy patching
export class CccClient {
  private readonly agent: http.Agent;

  constructor(
    private readonly addr: ServerAddress,
    private readonly log: Log,
    private readonly userAgent: string,
  ) {
    // the analyser runs exactly four worker threads; more sockets only queue
    this.agent = new http.Agent({ keepAlive: true, maxSockets: 4 });
  }

  health(signal?: AbortSignal): Promise<Health> {
    return this.getJson<Health>('/health', TIMEOUT_FAST_MS, signal);
  }

  insights(base: string | undefined, signal?: AbortSignal): Promise<InsightsPayload> {
    const query = base ? `?base=${encodeURIComponent(base)}` : '';
    return this.getJson<InsightsPayload>(`/insights.json${query}`, TIMEOUT_SLOW_MS, signal);
  }

  // pass the full repo-relative path - the server suffix-matches so a bare `money.rs` can mis-resolve
  async file(rel: string, signal?: AbortSignal): Promise<FileStructure | undefined> {
    try {
      return await this.getJson<FileStructure>(
        `/file?path=${encodeURIComponent(rel)}`,
        TIMEOUT_FAST_MS,
        signal,
      );
    } catch (err) {
      if (err instanceof CccHttpError && err.status === 404) return undefined;
      throw err;
    }
  }

  references(symbol: string, signal?: AbortSignal): Promise<ReferencesResult> {
    return this.getJson<ReferencesResult>(
      `/references?symbol=${encodeURIComponent(symbol)}`,
      TIMEOUT_FAST_MS,
      signal,
    );
  }

  refresh(signal?: AbortSignal): Promise<RefreshResult> {
    return this.request<RefreshResult>('POST', '/refresh', TIMEOUT_SLOW_MS, signal);
  }

  private getJson<T>(path: string, timeoutMs: number, signal?: AbortSignal): Promise<T> {
    return this.request<T>('GET', path, timeoutMs, signal);
  }

  private request<T>(method: string, path: string, timeoutMs: number, signal?: AbortSignal): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      if (signal?.aborted) {
        reject(new AbortedError());
        return;
      }
      const started = Date.now();
      const req = http.request(
        {
          host: this.addr.host,
          port: this.addr.port,
          path,
          method,
          agent: this.agent,
          // no Origin: the analyser rejects cross-origin and a Node request sending none is same-origin
          headers: { Accept: 'application/json', 'User-Agent': this.userAgent },
        },
        (res) => {
          const chunks: Buffer[] = [];
          res.on('data', (c: Buffer) => chunks.push(c));
          res.on('end', () => {
            cleanup();
            const body = Buffer.concat(chunks).toString('utf8');
            const status = res.statusCode ?? 0;
            this.log.trace(`${method} ${path} -> ${status} ${body.length}b in ${Date.now() - started}ms`);
            if (status < 200 || status >= 300) {
              reject(new CccHttpError(status, path, body));
              return;
            }
            try {
              resolve(JSON.parse(body) as T);
            } catch (err) {
              reject(new Error(`ccc ${path} returned malformed JSON: ${String(err)}`));
            }
          });
        },
      );

      const onAbort = () => {
        req.destroy();
        cleanup();
        reject(new AbortedError());
      };
      const cleanup = () => signal?.removeEventListener('abort', onAbort);
      signal?.addEventListener('abort', onAbort, { once: true });

      req.setTimeout(timeoutMs, () => {
        req.destroy(new Error(`timed out after ${timeoutMs}ms`));
      });
      req.on('error', (err) => {
        cleanup();
        if (signal?.aborted) reject(new AbortedError());
        else reject(err);
      });
      req.end();
    });
  }

  dispose(): void {
    this.agent.destroy();
  }
}

import { type ChildProcessByStdio, spawn } from 'node:child_process';
import type { Readable } from 'node:stream';
import * as vscode from 'vscode';
import { CccBinaryError, resolveCccBinary } from './binary';
import type { Cfg } from './config';
import { describe, type Log } from './log';
import { publishMcpConfig } from './mcpconfig';

export interface ServerAddress {
  host: string;
  port: number;
  // e.g. "http://127.0.0.1:41337"
  base: string;
}

export type ServerState =
  | { kind: 'stopped' }
  | { kind: 'starting' }
  | { kind: 'running'; address: ServerAddress; pid: number | undefined; startedAt: number }
  | { kind: 'failed'; error: string; retryAt?: number };

// the startup line carrying the bound port - greedy host group so an IPv6 bind still parses
const LISTENING = /^listening on https?:\/\/(?<addr>\S+?)\s/;

// stdin is `ignore`, so the child has readable pipes and no writable stdin
type CccChild = ChildProcessByStdio<null, Readable, Readable>;

const BACKOFF_MS = [1000, 2000, 4000, 8000, 16000, 30000];
const MAX_FAILURES = 5;
const FAILURE_WINDOW_MS = 5 * 60 * 1000;
const STABLE_UPTIME_MS = 60 * 1000;
const STDERR_TAIL = 20;

// owns one `ccc serve` child per folder - private to this window so a user's own server is undisturbed
export class ServerProcess implements vscode.Disposable {
  private readonly emitter = new vscode.EventEmitter<ServerState>();
  readonly onDidChangeState = this.emitter.event;

  private current: ServerState = { kind: 'stopped' };
  private child: CccChild | undefined;
  private starting: Promise<ServerAddress> | undefined;
  private disposing = false;
  private failures: number[] = [];
  private retryTimer: NodeJS.Timeout | undefined;
  private stderrTail: string[] = [];
  private readonly exitGuard: () => void;

  constructor(
    private readonly folder: vscode.Uri,
    private cfg: Cfg,
    private readonly log: Log,
    private readonly label: string,
  ) {
    // last-ditch cleanup if the extension host dies without calling deactivate
    this.exitGuard = () => this.child?.kill();
    process.once('exit', this.exitGuard);
  }

  get state(): ServerState {
    return this.current;
  }

  get address(): ServerAddress | undefined {
    return this.current.kind === 'running' ? this.current.address : undefined;
  }

  updateConfig(cfg: Cfg): void {
    this.cfg = cfg;
  }

  // idempotent - returns the existing address when already running
  async start(): Promise<ServerAddress> {
    if (this.current.kind === 'running') return this.current.address;
    if (this.starting) return this.starting;
    this.starting = this.spawnAndWait().finally(() => {
      this.starting = undefined;
    });
    return this.starting;
  }

  async restart(): Promise<ServerAddress> {
    this.clearRetry();
    this.failures = [];
    this.kill();
    this.setState({ kind: 'stopped' });
    return this.start();
  }

  stop(): void {
    this.clearRetry();
    this.kill();
    this.setState({ kind: 'stopped' });
  }

  private async spawnAndWait(): Promise<ServerAddress> {
    this.setState({ kind: 'starting' });
    let bin: string;
    try {
      const resolved = await resolveCccBinary(this.folder, this.cfg, this.log);
      bin = resolved.path;
    } catch (err) {
      const message =
        err instanceof CccBinaryError
          ? `${err.message} Searched: ${err.searched.join(', ')}.`
          : describe(err);
      this.setState({ kind: 'failed', error: message });
      throw err;
    }

    const args = [
      this.folder.fsPath,
      '--html',
      '--addr',
      this.cfg.server.address,
      '--port',
      String(this.cfg.server.port),
      ...(this.cfg.server.watchIntervalSec === 0
        ? ['--no-watch']
        : ['--watch-interval', String(this.cfg.server.watchIntervalSec)]),
      ...this.cfg.server.extraArgs,
    ];
    this.log.info(`[${this.label}] spawning: ${bin} serve ${args.join(' ')}`);

    const child = spawn(bin, ['serve', ...args], {
      cwd: this.folder.fsPath,
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
      env: { ...process.env, NO_COLOR: '1' },
    });
    this.child = child;
    this.stderrTail = [];

    const address = await this.awaitListening(child);
    this.wireExit(child);
    this.setState({ kind: 'running', address, pid: child.pid, startedAt: Date.now() });
    this.log.info(`[${this.label}] analyser ready on ${address.base} (pid ${child.pid ?? '?'})`);
    // the port is ephemeral, so republish it for agents on every start
    // deliberately not awaited - a slow disk must not hold up a server that is already serving
    void publishMcpConfig(this.folder, address, this.log);
    return address;
  }

  private awaitListening(child: CccChild): Promise<ServerAddress> {
    return new Promise<ServerAddress>((resolve, reject) => {
      let settled = false;
      let buffer = '';

      const finish = (fn: () => void) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        fn();
      };

      const timer = setTimeout(() => {
        finish(() => {
          child.kill();
          reject(
            new Error(
              `the analyser did not report a listening port within ${this.cfg.server.startupTimeoutMs}ms. ` +
                'A cold scan of a very large repo can exceed this: raise ccc.server.startupTimeoutMs.',
            ),
          );
        });
      }, this.cfg.server.startupTimeoutMs);

      child.stdout.setEncoding('utf8');
      child.stdout.on('data', (chunk: string) => {
        this.log.server(this.label, 'out', chunk);
        if (settled) return;
        buffer += chunk;
        const lines = buffer.split('\n');
        buffer = lines.pop() ?? '';
        for (const line of lines) {
          const address = parseListening(line);
          if (!address) continue;
          finish(() => resolve(address));
          return;
        }
      });

      child.stderr.setEncoding('utf8');
      child.stderr.on('data', (chunk: string) => {
        this.log.server(this.label, 'err', chunk);
        for (const line of chunk.split(/\r?\n/)) {
          if (line.trim().length === 0) continue;
          this.stderrTail.push(line);
          if (this.stderrTail.length > STDERR_TAIL) this.stderrTail.shift();
        }
      });

      child.on('error', (err) => {
        finish(() => reject(new Error(`could not launch the analyser: ${describe(err)}`)));
      });

      child.on('exit', (code, signal) => {
        finish(() =>
          reject(
            new Error(
              `the analyser exited before it started listening (code ${code ?? 'null'}, signal ${
                signal ?? 'none'
              })${this.stderrTail.length > 0 ? `:\n${this.stderrTail.join('\n')}` : '.'}`,
            ),
          ),
        );
      });
    });
  }

  // restart with backoff when a healthy process dies unexpectedly
  private wireExit(child: CccChild): void {
    child.on('exit', (code, signal) => {
      if (this.disposing || this.child !== child) return;
      this.child = undefined;
      const startedAt = this.current.kind === 'running' ? this.current.startedAt : Date.now();
      const uptime = Date.now() - startedAt;
      this.log.warn(
        `[${this.label}] analyser exited (code ${code ?? 'null'}, signal ${signal ?? 'none'}) ` +
          `after ${Math.round(uptime / 1000)}s`,
      );
      if (this.stderrTail.length > 0) {
        this.log.warn(`[${this.label}] last analyser output:\n${this.stderrTail.join('\n')}`);
      }

      const now = Date.now();
      if (uptime >= STABLE_UPTIME_MS) this.failures = [];
      this.failures = this.failures.filter((t) => now - t < FAILURE_WINDOW_MS);
      this.failures.push(now);

      if (this.failures.length >= MAX_FAILURES) {
        this.setState({
          kind: 'failed',
          error: `the analyser exited ${this.failures.length} times in five minutes — giving up. See the ccc log.`,
        });
        return;
      }

      const delay = BACKOFF_MS[Math.min(this.failures.length - 1, BACKOFF_MS.length - 1)] ?? 30000;
      this.setState({ kind: 'failed', error: 'analyser exited', retryAt: now + delay });
      this.log.info(`[${this.label}] restarting the analyser in ${delay}ms`);
      this.clearRetry();
      this.retryTimer = setTimeout(() => {
        this.retryTimer = undefined;
        if (this.disposing) return;
        void this.start().catch((err) => this.log.error(`[${this.label}] restart failed`, err));
      }, delay);
    });
  }

  private setState(state: ServerState): void {
    this.current = state;
    this.emitter.fire(state);
  }

  private clearRetry(): void {
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = undefined;
    }
  }

  private kill(): void {
    const child = this.child;
    if (!child) return;
    this.child = undefined;
    try {
      child.kill('SIGTERM');
    } catch {
      // already gone
    }
    // SIGTERM is meaningless on Windows - kill() maps to TerminateProcess so the escalation below is a no-op
    const hard = setTimeout(() => {
      try {
        child.kill('SIGKILL');
      } catch {
        // already gone
      }
    }, 2000);
    child.once('exit', () => clearTimeout(hard));
  }

  dispose(): void {
    this.disposing = true;
    this.clearRetry();
    this.kill();
    process.removeListener('exit', this.exitGuard);
    this.emitter.dispose();
  }
}

// exported for the port-parsing edge cases (IPv6, non-default hosts)
export function parseListening(line: string): ServerAddress | undefined {
  const match = LISTENING.exec(line.trim());
  const addr = match?.groups?.['addr'];
  if (!addr) return undefined;
  const split = addr.lastIndexOf(':');
  if (split <= 0) return undefined;
  const host = addr.slice(0, split);
  const port = Number.parseInt(addr.slice(split + 1), 10);
  if (!Number.isInteger(port) || port <= 0) return undefined;
  return { host, port, base: `http://${host}:${port}` };
}

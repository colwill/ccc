import * as path from 'node:path';
import * as vscode from 'vscode';
import { CccClient, isAborted } from './client';
import { type Cfg, needsRebuild, needsServerRestart } from './config';
import { FileStructureCache, refineFileHints } from './enclosing';
import type { Log } from './log';
import { buildHintIndex, type FileHints, type HintIndex } from './model';
import { keyOf, relOf } from './paths';
import { ServerProcess, type ServerState } from './server';
import type { FileStructure, InsightsPayload, ReferencesResult } from './types';

export interface RefreshOptions {
  // POST /refresh before reading the analysis - the files on disk changed
  rescan: boolean;
  // ignore the generation short-circuit
  force: boolean;
  reason: string;
}

// everything belonging to one workspace folder - process, client, index and refresh scheduling
export class WorkspaceSession implements vscode.Disposable {
  private readonly server: ServerProcess;
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChange = this.changed.event;

  private client: CccClient | undefined;
  private structures: FileStructureCache | undefined;
  private currentIndex: HintIndex | undefined;
  private lastPayload: InsightsPayload | undefined;
  private lastGenerated: string | undefined;
  private lastBase: string | undefined;
  private inFlight: AbortController | undefined;
  private debounce: NodeJS.Timeout | undefined;
  private pending: RefreshOptions | undefined;
  private poll: NodeJS.Timeout | undefined;
  private disposed = false;

  constructor(
    readonly folder: vscode.WorkspaceFolder,
    private cfg: Cfg,
    private readonly log: Log,
    private readonly userAgent: string,
  ) {
    this.server = new ServerProcess(folder.uri, cfg, log, folder.name);
    this.server.onDidChangeState((state) => this.onServerState(state));
  }

  get index(): HintIndex | undefined {
    return this.currentIndex;
  }

  get serverState(): ServerState {
    return this.server.state;
  }

  get root(): vscode.Uri {
    return this.folder.uri;
  }

  async ensureStarted(): Promise<void> {
    if (this.disposed) return;
    const address = await this.server.start();
    if (this.disposed) return;
    if (!this.client) {
      this.client = new CccClient(address, this.log, this.userAgent);
      this.structures = new FileStructureCache(this.client, this.log);
      await this.waitForHealth();
    }
    this.schedule({ rescan: false, force: true, reason: 'session start' });
    this.startPoll();
  }

  // the listening line lands before the worker threads exist so a request there can be refused
  private async waitForHealth(): Promise<void> {
    const delays = [100, 200, 400];
    for (let attempt = 0; attempt <= delays.length; attempt += 1) {
      try {
        await this.client?.health();
        return;
      } catch (err) {
        const delay = delays[attempt];
        if (delay === undefined) {
          this.log.warn(`[${this.folder.name}] analyser health check never succeeded: ${String(err)}`);
          return;
        }
        await sleep(delay);
      }
    }
  }

  updateConfig(next: Cfg): void {
    const previous = this.cfg;
    this.cfg = next;
    this.server.updateConfig(next);
    if (needsServerRestart(previous, next)) {
      this.log.info(`[${this.folder.name}] server settings changed, restarting the analyser`);
      this.client?.dispose();
      this.client = undefined;
      this.structures = undefined;
      void this.server.restart().then(() => this.ensureStarted());
      return;
    }
    if (previous.baseRef !== next.baseRef) {
      this.schedule({ rescan: false, force: true, reason: 'base ref changed' });
      return;
    }
    if (needsRebuild(previous, next)) this.rebuild();
    this.startPoll();
  }

  // rebuild the index from the cached payload - no network, no rescan
  rebuild(): void {
    if (!this.lastPayload) return;
    this.currentIndex = buildHintIndex(this.lastPayload, {
      rootPath: this.folder.uri.fsPath,
      cfg: this.cfg,
    });
    this.changed.fire();
  }

  // coalesce triggers - the strongest request in the window wins
  schedule(options: RefreshOptions): void {
    if (this.disposed) return;
    this.pending = this.pending
      ? {
          rescan: this.pending.rescan || options.rescan,
          force: this.pending.force || options.force,
          reason: options.reason,
        }
      : options;
    if (this.debounce) clearTimeout(this.debounce);
    this.debounce = setTimeout(() => {
      this.debounce = undefined;
      const next = this.pending;
      this.pending = undefined;
      if (next) void this.refresh(next).catch(() => undefined);
    }, this.cfg.refresh.debounceMs);
  }

  async refresh(options: RefreshOptions): Promise<void> {
    if (this.disposed) return;
    await this.ensureStarted();
    const client = this.client;
    if (!client) return;

    // a newer trigger abandons the in-flight request - it also keeps the two locks from overlapping
    this.inFlight?.abort();
    const controller = new AbortController();
    this.inFlight = controller;
    const signal = controller.signal;

    try {
      if (options.rescan) {
        const result = await client.refresh(signal);
        this.log.trace(`[${this.folder.name}] rescan: ${result.files_before} -> ${result.files_after} files`);
      } else if (!options.force) {
        // `generated` is one-second resolution so only low-frequency paths may short-circuit here
        const health = await client.health(signal);
        if (health.generated === this.lastGenerated && this.cfg.baseRef === this.lastBase) {
          this.log.trace(`[${this.folder.name}] skipping refresh (${options.reason}): map unchanged`);
          return;
        }
      }

      const payload = await client.insights(this.cfg.baseRef, signal);
      if (this.disposed || signal.aborted) return;

      this.lastPayload = payload;
      this.lastGenerated = payload.generated;
      this.lastBase = this.cfg.baseRef;
      this.structures?.clear();
      this.currentIndex = buildHintIndex(payload, { rootPath: this.folder.uri.fsPath, cfg: this.cfg });
      this.log.info(
        `[${this.folder.name}] ${options.reason}: ${this.currentIndex.counts.changed} changed, ` +
          `${this.currentIndex.counts.untested} untested, ${this.currentIndex.files.size} files with hints`,
      );
      this.changed.fire();
    } catch (err) {
      if (isAborted(err)) return;
      this.log.error(`[${this.folder.name}] refresh failed (${options.reason})`, err);
    } finally {
      if (this.inFlight === controller) this.inFlight = undefined;
    }
  }

  // hints for one file with the second pass applied - one small request per map generation
  async hintsFor(uri: vscode.Uri): Promise<FileHints | undefined> {
    const index = this.currentIndex;
    if (!index) return undefined;
    const hints = index.files.get(keyOf(uri));
    if (!hints) return undefined;
    const rel = relOf(this.folder.uri, uri);
    if (!rel || !this.structures) return hints;
    const structure = await this.structures.get(rel, index.generated);
    if (structure && this.currentIndex === index) refineFileHints(hints, index, structure, this.cfg);
    return hints;
  }

  // one file's structure whatever the diff touched - measurements are not diff-driven
  async structureFor(uri: vscode.Uri): Promise<FileStructure | undefined> {
    const index = this.currentIndex;
    if (!index || !this.structures) return undefined;
    const rel = relOf(this.folder.uri, uri);
    if (!rel) return undefined;
    return this.structures.get(rel, index.generated);
  }

  // whether a file is in the analyser's map at all
  async isMapped(uri: vscode.Uri): Promise<boolean> {
    const rel = relOf(this.folder.uri, uri);
    if (!rel || !this.structures || !this.currentIndex) return false;
    return (await this.structures.get(rel, this.currentIndex.generated)) !== undefined;
  }

  // URI of a file in a peer repo - undefined when the peer is known only by its surface
  async locateExternal(service: string, relFile: string): Promise<vscode.Uri | undefined> {
    const source = this.currentIndex?.externals.get(service)?.source ?? '';
    const prefix = 'path ';
    if (!source.startsWith(prefix)) return undefined;
    const dir = source.slice(prefix.length).trim();
    if (dir.length === 0) return undefined;
    const base = path.isAbsolute(dir)
      ? vscode.Uri.file(dir)
      : vscode.Uri.joinPath(this.folder.uri, ...dir.split('/'));
    const candidate = vscode.Uri.joinPath(base, ...relFile.split('/'));
    try {
      await vscode.workspace.fs.stat(candidate);
      return candidate;
    } catch {
      return undefined;
    }
  }

  async references(symbol: string): Promise<ReferencesResult> {
    await this.ensureStarted();
    if (!this.client) throw new Error('the ccc analyser is not running');
    return this.client.references(symbol);
  }

  get insightsUrl(): string | undefined {
    const address = this.server.address;
    return address ? `${address.base}/insights` : undefined;
  }

  async restartServer(): Promise<void> {
    this.client?.dispose();
    this.client = undefined;
    this.structures = undefined;
    this.currentIndex = undefined;
    this.lastPayload = undefined;
    this.lastGenerated = undefined;
    await this.server.restart();
    await this.ensureStarted();
  }

  stopServer(): void {
    this.stopPoll();
    this.server.stop();
    this.client?.dispose();
    this.client = undefined;
    this.structures = undefined;
    this.currentIndex = undefined;
    this.changed.fire();
  }

  private onServerState(state: ServerState): void {
    this.changed.fire();
    if (state.kind === 'running' && this.client === undefined && !this.disposed) {
      // came back after a crash restart
      void this.ensureStarted().catch((err) => this.log.error('restart handling failed', err));
    }
  }

  private startPoll(): void {
    this.stopPoll();
    if (this.cfg.refresh.intervalSec <= 0) return;
    this.poll = setInterval(
      () => this.schedule({ rescan: true, force: false, reason: 'poll' }),
      this.cfg.refresh.intervalSec * 1000,
    );
  }

  private stopPoll(): void {
    if (this.poll) {
      clearInterval(this.poll);
      this.poll = undefined;
    }
  }

  dispose(): void {
    this.disposed = true;
    this.stopPoll();
    if (this.debounce) clearTimeout(this.debounce);
    this.inFlight?.abort();
    this.client?.dispose();
    this.server.dispose();
    this.changed.dispose();
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

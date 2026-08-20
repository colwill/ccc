import * as vscode from 'vscode';
import { CccBinaryError } from './binary';
import { CccCodeLensProvider } from './codelens';
import { type CommandHost, registerCommands } from './commands';
import { type Cfg, needsDecorationReload, readConfig } from './config';
import { DecorationSet } from './decorations';
import { CccHoverProvider } from './hover';
import { Log } from './log';
import { isSupportedDocument, keyOf } from './paths';
import { WorkspaceSession } from './session';
import { type ActiveFileState, StatusBar } from './status';
import { ComplexityPanel } from './complexitypanel';
import { TestTriggerPanel } from './testpanel';

// re-applying decorations while typing is cheap but not free
const DIRTY_DEBOUNCE_MS = 150;
// don't rescan on every alt-tab
const FOCUS_COOLDOWN_MS = 10_000;

let extension: Extension | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  extension = new Extension(context);
  await extension.start();
}

export async function deactivate(): Promise<void> {
  // returning the promise makes VSCode wait for the analyser processes to die
  await extension?.shutdown();
  extension = undefined;
}

class Extension implements CommandHost {
  readonly log = new Log();
  private readonly status = new StatusBar();
  private readonly sessionMap = new Map<string, WorkspaceSession>();
  private readonly dirty = new Set<string>();
  private readonly warned = new Set<string>();
  private decorations: DecorationSet;
  private readonly codeLens: CccCodeLensProvider;
  private readonly hover: CccHoverProvider;
  private readonly testPanel: TestTriggerPanel;
  private readonly complexityPanel: ComplexityPanel;
  private cfg: Cfg;
  private lastFocusRefresh = 0;
  private dirtyTimer: NodeJS.Timeout | undefined;
  private disposables: vscode.Disposable[] = [];

  constructor(private readonly context: vscode.ExtensionContext) {
    this.cfg = readConfig();
    this.log.setLevel(this.cfg.trace);
    this.decorations = new DecorationSet(context, this.cfg);
    // the lens provider reads the same index the decorations do so they cannot disagree
    this.codeLens = new CccCodeLensProvider(async (uri) => {
      const folder = vscode.workspace.getWorkspaceFolder(uri);
      const session = folder ? this.sessionMap.get(folder.uri.toString()) : undefined;
      if (!session) return undefined;
      return { hints: await session.hintsFor(uri), stale: this.dirty.has(keyOf(uri)) };
    }, this.cfg);
    // the hover reads the same sources the decorations do so a mark cannot contradict itself
    this.hover = new CccHoverProvider(async (uri) => {
      const folder = vscode.workspace.getWorkspaceFolder(uri);
      const session = folder ? this.sessionMap.get(folder.uri.toString()) : undefined;
      if (!session) return undefined;
      return {
        hints: await session.hintsFor(uri),
        structure: this.cfg.complexity.enabled ? await session.structureFor(uri) : undefined,
        index: session.index,
        stale: this.dirty.has(keyOf(uri)),
      };
    }, this.cfg);
    this.testPanel = new TestTriggerPanel(
      () => this.sessions(),
      () => void this.wake(),
    );
    this.complexityPanel = new ComplexityPanel(
      () => this.sessions(),
      () => void this.wake(),
    );
  }

  async start(): Promise<void> {
    const version = this.context.extension.packageJSON?.version ?? '0.0.0';
    this.userAgent = `vscode-ccc/${version}`;
    this.log.info(`ccc extension ${version} activating`);

    // commands register unconditionally so `ccc: Show Log` still works when all else failed
    this.disposables.push(
      ...registerCommands(this),
      this.log,
      this.status,
      this.decorations,
      this.codeLens,
      this.testPanel,
      this.complexityPanel,
      vscode.languages.registerCodeLensProvider({ scheme: 'file' }, this.codeLens),
      vscode.languages.registerHoverProvider({ scheme: 'file' }, this.hover),
      vscode.commands.registerCommand('ccc.refreshTestTriggers', () =>
        this.refreshAll('test triggers panel'),
      ),
      vscode.commands.registerCommand('ccc.filterComplexityByName', () =>
        this.complexityPanel.filterByName(),
      ),
      vscode.commands.registerCommand('ccc.filterComplexityByArity', () =>
        this.complexityPanel.filterByArity(),
      ),
      vscode.commands.registerCommand('ccc.filterComplexityByScore', () =>
        this.complexityPanel.filterByScore(),
      ),
      vscode.commands.registerCommand('ccc.clearComplexityFilters', () =>
        this.complexityPanel.clearFilters(),
      ),
      vscode.commands.registerCommand('ccc.toggleComplexityTests', () =>
        this.complexityPanel.toggleTests(),
      ),
      // a suggested command is only useful if running it is one click away
      vscode.commands.registerCommand('ccc.runTestCommand', (arg: unknown) => {
        const spec = arg as { command?: string; cwd?: string } | undefined;
        if (!spec?.command) return;
        const terminal =
          vscode.window.terminals.find((t) => t.name === 'ccc tests') ??
          vscode.window.createTerminal({ name: 'ccc tests', cwd: spec.cwd });
        terminal.show();
        terminal.sendText(spec.command);
      }),
    );
    await vscode.commands.executeCommand('setContext', 'ccc.active', false);

    this.disposables.push(
      vscode.workspace.onDidChangeConfiguration((e) => {
        if (!e.affectsConfiguration('ccc')) return;
        void this.onConfigChanged();
      }),
      vscode.window.onDidChangeActiveTextEditor(() => void this.onActiveEditor()),
      vscode.window.onDidChangeVisibleTextEditors(() => void this.render()),
      vscode.workspace.onDidSaveTextDocument((doc) => this.onSave(doc)),
      vscode.workspace.onDidChangeTextDocument((e) => this.onEdit(e)),
      vscode.window.onDidChangeWindowState((state) => this.onWindowState(state)),
      vscode.workspace.onDidChangeWorkspaceFolders((e) => this.onFoldersChanged(e)),
    );

    await this.onActiveEditor();
  }

  private userAgent = 'vscode-ccc';

  // sessions start lazily so a twelve-folder workspace does not spawn twelve analysers
  private async sessionFor(uri: vscode.Uri): Promise<WorkspaceSession | undefined> {
    if (!this.cfg.enable || !this.cfg.server.autoStart) return undefined;
    const folder = vscode.workspace.getWorkspaceFolder(uri);
    if (!folder) return undefined;
    const key = folder.uri.toString();
    const existing = this.sessionMap.get(key);
    if (existing) return existing;

    const cfg = readConfig(folder);
    const session = new WorkspaceSession(folder, cfg, this.log, this.userAgent);
    this.sessionMap.set(key, session);
    session.onDidChange(() => void this.render());
    try {
      await session.ensureStarted();
      await vscode.commands.executeCommand('setContext', 'ccc.active', true);
    } catch (err) {
      this.reportStartFailure(folder, err);
    }
    return session;
  }

  // the lazy path starts the analyser from the active editor, which leaves the panels dead
  // when a window restores with no editor open - opening a ccc view is intent enough to start
  private async wake(): Promise<void> {
    if (this.sessionMap.size > 0) return;
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) return;
    this.log.info(`starting the analyser for ${folder.name} because a ccc panel was opened`);
    await this.sessionFor(folder.uri);
  }

  activeSession(): WorkspaceSession | undefined {
    const uri = vscode.window.activeTextEditor?.document.uri;
    if (uri) {
      const folder = vscode.workspace.getWorkspaceFolder(uri);
      if (folder) {
        const session = this.sessionMap.get(folder.uri.toString());
        if (session) return session;
      }
    }
    const first = this.sessionMap.values().next();
    return first.done ? undefined : first.value;
  }

  sessions(): WorkspaceSession[] {
    return [...this.sessionMap.values()];
  }

  private reportStartFailure(folder: vscode.WorkspaceFolder, err: unknown): void {
    const key = folder.uri.toString();
    this.log.error(`could not start the analyser for ${folder.name}`, err);
    if (this.warned.has(key)) return;
    this.warned.add(key);
    const message =
      err instanceof CccBinaryError
        ? `ccc: ${err.message} Searched: ${err.searched.join(', ')}.`
        : `ccc: could not start the analyser for ${folder.name}. See the log for details.`;
    void vscode.window
      .showWarningMessage(message, 'Open Settings', 'Show Log')
      .then((choice) => {
        if (choice === 'Open Settings') {
          void vscode.commands.executeCommand('workbench.action.openSettings', 'ccc.binaryPath');
        } else if (choice === 'Show Log') this.log.show();
      });
  }

  private async onConfigChanged(): Promise<void> {
    const previous = this.cfg;
    this.cfg = readConfig();
    this.log.setLevel(this.cfg.trace);

    if (needsDecorationReload(previous, this.cfg)) this.decorations.reload(this.cfg);
    this.codeLens.updateConfig(this.cfg);
    this.hover.updateConfig(this.cfg);

    for (const [key, session] of this.sessionMap) {
      session.updateConfig(readConfig(vscode.workspace.getWorkspaceFolder(vscode.Uri.parse(key))));
    }
    if (!this.cfg.enable) {
      this.clearAllDecorations();
    }
    await this.render();
  }

  private async onActiveEditor(): Promise<void> {
    const editor = vscode.window.activeTextEditor;
    if (editor && isSupportedDocument(editor.document)) await this.sessionFor(editor.document.uri);
    await this.render();
  }

  private onSave(doc: vscode.TextDocument): void {
    if (!isSupportedDocument(doc)) return;
    this.dirty.delete(keyOf(doc.uri));
    if (!this.cfg.refresh.onSave) {
      void this.render();
      return;
    }
    const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
    const session = folder ? this.sessionMap.get(folder.uri.toString()) : undefined;
    // the disk changed so the analyser must re-read it before the analysis means anything
    session?.schedule({ rescan: true, force: true, reason: `save ${doc.fileName}` });
  }

  // the analyser reads disk not the buffer so an unsaved edit only dims the hints
  private onEdit(e: vscode.TextDocumentChangeEvent): void {
    if (e.contentChanges.length === 0) return;
    if (!isSupportedDocument(e.document)) return;
    const key = keyOf(e.document.uri);
    const wasClean = !this.dirty.has(key);
    this.dirty.add(key);
    if (this.dirtyTimer) clearTimeout(this.dirtyTimer);
    if (wasClean) void this.render();
    this.dirtyTimer = setTimeout(() => {
      this.dirtyTimer = undefined;
      void this.render();
    }, DIRTY_DEBOUNCE_MS);
  }

  // catches git checkouts, codegen and edits made outside the editor
  private onWindowState(state: vscode.WindowState): void {
    if (!state.focused || !this.cfg.refresh.onWindowFocus) return;
    const now = Date.now();
    if (now - this.lastFocusRefresh < FOCUS_COOLDOWN_MS) return;
    this.lastFocusRefresh = now;
    for (const session of this.sessionMap.values()) {
      session.schedule({ rescan: true, force: false, reason: 'window focus' });
    }
  }

  private onFoldersChanged(e: vscode.WorkspaceFoldersChangeEvent): void {
    for (const folder of e.removed) {
      const key = folder.uri.toString();
      this.sessionMap.get(key)?.dispose();
      this.sessionMap.delete(key);
      this.warned.delete(key);
    }
    void this.render();
  }

  async render(): Promise<void> {
    this.codeLens.refresh(this.lensSignature());
    this.testPanel.refresh();
    this.complexityPanel.refresh();
    const active = vscode.window.activeTextEditor;
    let activeFile: ActiveFileState | undefined;

    for (const editor of vscode.window.visibleTextEditors) {
      if (!this.cfg.enable || !isSupportedDocument(editor.document)) {
        this.decorations.clear(editor);
        continue;
      }
      const folder = vscode.workspace.getWorkspaceFolder(editor.document.uri);
      const session = folder ? this.sessionMap.get(folder.uri.toString()) : undefined;
      if (!session) {
        this.decorations.clear(editor);
        continue;
      }
      const hints = await session.hintsFor(editor.document.uri);
      const stale = this.dirty.has(keyOf(editor.document.uri));
      this.decorations.apply(editor, hints, session.index, stale);
      // complexity is a property of the code not the diff so it is drawn either way
      this.decorations.applyComplexity(
        editor,
        this.cfg.complexity.enabled ? await session.structureFor(editor.document.uri) : undefined,
        stale,
      );
      if (editor === active && session.index) {
        // no marks has two causes the user cannot tell apart - ask, for the active editor only
        activeFile = hints
          ? 'hints'
          : (await session.isMapped(editor.document.uri))
            ? 'unchanged'
            : 'unmapped';
      }
    }

    const session = this.activeSession();
    if (!session) {
      this.status.hide();
      return;
    }
    this.status.update({
      server: session.serverState,
      index: session.index,
      dirty: active ? this.dirty.has(keyOf(active.document.uri)) : false,
      disabled: !this.cfg.enable,
      activeFile,
    });
  }

  // everything a CodeLens is drawn from - equal signature means identical lenses
  private lensSignature(): string {
    const sessions = [...this.sessionMap].map(([key, session]) => [
      key,
      session.index?.generated ?? '',
      session.index?.base ?? '',
    ]);
    const editors = vscode.window.visibleTextEditors.map((editor) => {
      const key = keyOf(editor.document.uri);
      return [key, this.dirty.has(key)];
    });
    return JSON.stringify([this.cfg.enable, this.cfg.hints.codeLens, sessions, editors]);
  }

  private clearAllDecorations(): void {
    for (const editor of vscode.window.visibleTextEditors) this.decorations.clear(editor);
  }

  refreshAll(reason: string): void {
    for (const session of this.sessionMap.values()) {
      // a manual refresh bypasses every cache - the user asked because something changed
      void session.refresh({ rescan: true, force: true, reason });
    }
  }

  async toggleHints(): Promise<void> {
    const target = vscode.workspace.workspaceFolders?.length
      ? vscode.ConfigurationTarget.Workspace
      : vscode.ConfigurationTarget.Global;
    await vscode.workspace.getConfiguration('ccc').update('enable', !this.cfg.enable, target);
  }

  async shutdown(): Promise<void> {
    if (this.dirtyTimer) clearTimeout(this.dirtyTimer);
    for (const session of this.sessionMap.values()) session.dispose();
    this.sessionMap.clear();
    for (const disposable of this.disposables) disposable.dispose();
    this.disposables = [];
  }
}

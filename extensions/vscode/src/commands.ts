import { execFile } from 'node:child_process';
import * as vscode from 'vscode';
import type { Log } from './log';
import { type Coverage, type InboundRef, missingTestPhrase, type OutboundRef, type TestLink } from './model';
import { absOf } from './paths';
import type { WorkspaceSession } from './session';

export interface CommandHost {
  log: Log;
  // the session for the active editor, or the only session if there is one
  activeSession(): WorkspaceSession | undefined;
  sessions(): WorkspaceSession[];
  refreshAll(reason: string): void;
  toggleHints(): Promise<void>;
  render(): void;
}

export function registerCommands(host: CommandHost): vscode.Disposable[] {
  return [
    vscode.commands.registerCommand('ccc.showOutput', () => host.log.show()),

    vscode.commands.registerCommand('ccc.refresh', () => host.refreshAll('manual refresh')),

    vscode.commands.registerCommand('ccc.toggleHints', () => host.toggleHints()),

    vscode.commands.registerCommand('ccc.restartServer', async () => {
      const sessions = host.sessions();
      if (sessions.length === 0) {
        void vscode.window.showInformationMessage('ccc: no analyser is running.');
        return;
      }
      await Promise.all(sessions.map((s) => s.restartServer().catch((err) => host.log.error('restart failed', err))));
      host.render();
    }),

    vscode.commands.registerCommand('ccc.stopServer', () => {
      for (const session of host.sessions()) session.stopServer();
      host.render();
    }),

    vscode.commands.registerCommand('ccc.openInsights', async () => {
      const session = host.activeSession();
      const url = session?.insightsUrl;
      if (!url) {
        void vscode.window.showWarningMessage('ccc: the analyser is not running yet.');
        return;
      }
      await vscode.env.openExternal(vscode.Uri.parse(url));
    }),

    vscode.commands.registerCommand('ccc.openLocation', async (arg: unknown) => {
      const target = readLocation(arg);
      const session = host.activeSession();
      if (!target || !session) return;
      const uri = absOf(session.root, target.file);
      try {
        const doc = await vscode.workspace.openTextDocument(uri);
        const editor = await vscode.window.showTextDocument(doc, { preview: true, preserveFocus: false });
        const line = Math.max(0, Math.min(target.line - 1, doc.lineCount - 1));
        const range = doc.lineAt(line).range;
        editor.selection = new vscode.Selection(range.start, range.start);
        editor.revealRange(range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
      } catch (err) {
        host.log.error(`could not open ${target.file}:${target.line}`, err);
        void vscode.window.showWarningMessage(`ccc: could not open ${target.file}:${target.line}.`);
      }
    }),

    // reuse VSCode's own peek UI rather than a results view - this is purely a translation
    vscode.commands.registerCommand('ccc.showReferences', async (arg: unknown) => {
      const symbol = readSymbol(arg);
      const session = host.activeSession();
      const editor = vscode.window.activeTextEditor;
      if (!symbol || !session || !editor) return;
      try {
        const result = await session.references(symbol);
        const locations = [...result.definitions, ...result.references]
          .filter((hit) => typeof hit.file === 'string' && typeof hit.line === 'number')
          .map((hit) => {
            const uri = absOf(session.root, hit.file);
            const line = Math.max(0, hit.line - 1);
            return new vscode.Location(uri, new vscode.Position(line, 0));
          });
        if (locations.length === 0) {
          void vscode.window.showInformationMessage(`ccc: no references to ${symbol}.`);
          return;
        }
        await vscode.commands.executeCommand(
          'editor.action.showReferences',
          editor.document.uri,
          editor.selection.active,
          locations,
        );
      } catch (err) {
        host.log.error(`could not look up references to ${symbol}`, err);
      }
    }),

    // a lens is a promise that clicking it goes somewhere - these keep it
    vscode.commands.registerCommand('ccc.showTests', async (arg: unknown) => {
      const tests = readTests(arg);
      const session = host.activeSession();
      if (tests.length === 0 || !session) return;
      const located = tests.filter((t) => t.file && t.line !== undefined);
      if (located.length === 0) {
        void vscode.window.showInformationMessage(
          `ccc: ${tests.map((t) => t.name).join(', ')} — no location recorded for these tests.`,
        );
        return;
      }
      const picked =
        located.length === 1
          ? located[0]
          : await vscode.window.showQuickPick(
              located.map((t) => ({
                label: t.name,
                description: `${t.file}:${t.line}`,
                detail:
                  t.distance === 0
                    ? 'references the change directly'
                    : `${t.distance} call hop(s) away — ${t.reason ?? ''}`,
                value: t,
              })),
              { placeHolder: 'Test covering this change' },
            ).then((p) => p?.value);
      if (!picked?.file || picked.line === undefined) return;
      await vscode.commands.executeCommand('ccc.openLocation', {
        file: picked.file,
        line: picked.line,
      });
    }),

    vscode.commands.registerCommand('ccc.explainUntested', async (arg: unknown) => {
      const cov = readCoverage(arg);
      if (!cov) return;
      const kind = cov.target?.kind;
      const message = cov.target?.suggest
        ? `ccc: ${cov.fn} — ${cov.target.suggest}`
        : `ccc: ${cov.fn} changed and ${missingTestPhrase(kind)} covers it.`;
      const actions = ['Find references'];
      if (kind) actions.unshift(`Copy "${kind}"`);
      const choice = await vscode.window.showWarningMessage(message, ...actions);
      if (choice === 'Find references') {
        await vscode.commands.executeCommand('ccc.showReferences', { symbol: cov.fn });
      } else if (choice && kind) {
        await vscode.env.clipboard.writeText(kind);
      }
    }),

    // the far side may be in another checkout - open it when reachable, say where it lives when not
    vscode.commands.registerCommand('ccc.openCrossing', async (arg: unknown) => {
      const refs = readOutbound(arg);
      const target = refs.find((r) => r.targetFile && r.targetLine !== undefined);
      if (!target?.targetFile || target.targetLine === undefined) {
        void vscode.window.showInformationMessage('ccc: no handler location was recorded.');
        return;
      }
      if (target.remote) {
        await openRemote(host, target);
        return;
      }
      await vscode.commands.executeCommand('ccc.openLocation', {
        file: target.targetFile,
        line: target.targetLine,
      });
    }),

    vscode.commands.registerCommand('ccc.explainCrossing', async (arg: unknown) => {
      const refs = readOutbound(arg);
      const key = refs[0]?.remote?.key ?? refs[0]?.symbol ?? '';
      await vscode.window.showWarningMessage(
        `ccc: nothing serves "${key}". Check the key is spelled the same at both ends, ` +
          'and that the repository serving it is listed under `externals` in .ccc/map.json.',
      );
    }),

    vscode.commands.registerCommand('ccc.showCallers', async (arg: unknown) => {
      const refs = readInbound(arg);
      const located = refs.filter((r) => r.callerFile && r.callerLine !== undefined);
      if (located.length === 0) {
        void vscode.window.showInformationMessage('ccc: no calling location was recorded.');
        return;
      }
      const picked =
        located.length === 1
          ? located[0]
          : await vscode.window.showQuickPick(
              located.map((r) => ({
                label: r.callerFn ?? r.fromService,
                description: `${r.callerFile}:${r.callerLine}`,
                detail: r.remote ? `${r.fromService} — another repository` : r.fromService,
                value: r,
              })),
              { placeHolder: 'Caller' },
            ).then((p) => p?.value);
      if (!picked?.callerFile || picked.callerLine === undefined) return;
      if (picked.remote) {
        void vscode.window.showInformationMessage(
          `ccc: ${picked.callerFn ?? 'the caller'} lives in ${picked.remote.repo ?? picked.fromService}, ` +
            `at ${picked.callerFile}:${picked.callerLine} — another repository.`,
        );
        return;
      }
      await vscode.commands.executeCommand('ccc.openLocation', {
        file: picked.callerFile,
        line: picked.callerLine,
      });
    }),

    vscode.commands.registerCommand('ccc.copyTestCommand', async () => {
      const commands = host.activeSession()?.index?.commands ?? [];
      if (commands.length === 0) {
        void vscode.window.showInformationMessage('ccc: no test command is suggested for these changes.');
        return;
      }
      let chosen = commands[0];
      if (commands.length > 1) {
        const picked = await vscode.window.showQuickPick(
          commands.map((c) => ({
            label: c.command,
            description: c.language,
            detail: c.caveat,
            value: c,
          })),
          { placeHolder: 'Test command to copy' },
        );
        if (!picked) return;
        chosen = picked.value;
      }
      if (!chosen) return;
      await vscode.env.clipboard.writeText(chosen.command);
      void vscode.window.showInformationMessage(`ccc: copied \`${chosen.command}\``);
    }),

    vscode.commands.registerCommand('ccc.selectBaseRef', async () => {
      const session = host.activeSession();
      if (!session) {
        void vscode.window.showWarningMessage('ccc: open a file in a workspace folder first.');
        return;
      }
      const refs = await gitRefs(session.root.fsPath, host.log);
      const items: vscode.QuickPickItem[] = [
        { label: '$(discard) Automatic', description: 'let ccc pick origin/main, main, origin/master or master' },
        ...refs.map((ref) => ({ label: ref })),
        { label: '$(edit) Enter a ref…', alwaysShow: true },
      ];
      const picked = await vscode.window.showQuickPick(items, { placeHolder: 'Git ref to diff against' });
      if (!picked) return;

      let value: string | undefined;
      if (picked.label.startsWith('$(discard)')) value = '';
      else if (picked.label.startsWith('$(edit)')) {
        value = await vscode.window.showInputBox({ prompt: 'Git ref to diff against', value: 'origin/main' });
        if (value === undefined) return;
      } else value = picked.label;

      await vscode.workspace
        .getConfiguration('ccc', session.folder)
        .update('baseRef', value, vscode.ConfigurationTarget.WorkspaceFolder);
    }),
  ];
}

// a peer's handler has no URI here unless it is checked out - try the local path first
async function openRemote(host: CommandHost, ref: OutboundRef): Promise<void> {
  const session = host.activeSession();
  if (!session || !ref.targetFile || ref.targetLine === undefined) return;
  const local = await session.locateExternal(ref.toService, ref.targetFile);
  if (local) {
    const doc = await vscode.workspace.openTextDocument(local);
    const editor = await vscode.window.showTextDocument(doc, { preview: true });
    const line = Math.max(0, Math.min(ref.targetLine - 1, doc.lineCount - 1));
    editor.selection = new vscode.Selection(line, 0, line, 0);
    editor.revealRange(doc.lineAt(line).range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
    return;
  }
  const where = ref.remote?.repo ?? ref.toService;
  void vscode.window.showInformationMessage(
    `ccc: ${ref.targetFunction ?? ref.symbol} is in ${where} at ${ref.targetFile}:${ref.targetLine}. ` +
      'That repository is not checked out here, so there is nothing to open.',
  );
}

function readTests(arg: unknown): TestLink[] {
  if (typeof arg !== 'object' || arg === null) return [];
  const tests = (arg as Record<string, unknown>)['tests'];
  return Array.isArray(tests) ? (tests as TestLink[]) : [];
}

function readCoverage(arg: unknown): Coverage | undefined {
  if (typeof arg !== 'object' || arg === null) return undefined;
  const cov = (arg as Record<string, unknown>)['coverage'];
  return typeof cov === 'object' && cov !== null ? (cov as Coverage) : undefined;
}

function readOutbound(arg: unknown): OutboundRef[] {
  if (typeof arg !== 'object' || arg === null) return [];
  const refs = (arg as Record<string, unknown>)['refs'];
  return Array.isArray(refs) ? (refs as OutboundRef[]) : [];
}

function readInbound(arg: unknown): InboundRef[] {
  if (typeof arg !== 'object' || arg === null) return [];
  const refs = (arg as Record<string, unknown>)['refs'];
  return Array.isArray(refs) ? (refs as InboundRef[]) : [];
}

function readLocation(arg: unknown): { file: string; line: number } | undefined {
  if (typeof arg !== 'object' || arg === null) return undefined;
  const record = arg as Record<string, unknown>;
  const file = record['file'];
  const line = record['line'];
  if (typeof file !== 'string' || typeof line !== 'number') return undefined;
  return { file, line };
}

function readSymbol(arg: unknown): string | undefined {
  if (typeof arg === 'string') return arg;
  if (typeof arg !== 'object' || arg === null) return undefined;
  const symbol = (arg as Record<string, unknown>)['symbol'];
  return typeof symbol === 'string' ? symbol : undefined;
}

function gitRefs(cwd: string, log: Log): Promise<string[]> {
  return new Promise((resolve) => {
    execFile(
      'git',
      ['for-each-ref', '--format=%(refname:short)', 'refs/heads', 'refs/remotes'],
      { cwd, timeout: 5000, windowsHide: true },
      (err, stdout) => {
        if (err) {
          log.trace(`could not list git refs: ${String(err)}`);
          resolve([]);
          return;
        }
        resolve(
          stdout
            .split('\n')
            .map((l) => l.trim())
            .filter((l) => l.length > 0 && !l.endsWith('/HEAD')),
        );
      },
    );
  });
}

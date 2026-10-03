import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { describe, type Log } from './log';
import type { WorkspaceSession } from './session';

// what the page may ask the analyser for: its own levels, the edit timeline and search
const ALLOWED = ['/vis', '/find?'];

// The architecture visualiser in VS Code - the analyser's own `/vis` page,
// as a view in the ccc side bar and as a tab beside the editor. The page asks
// this side for everything it reads and for files to open, so it works
// wherever the analyser runs, local or remote, and follows each edit ccc's
// tools make as it lands.
export class VisualiserView implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'ccc.visualiser';
  private panel: vscode.WebviewPanel | undefined;

  constructor(
    private readonly session: () => Promise<WorkspaceSession | undefined>,
    private readonly log: Log,
  ) {}

  resolveWebviewView(view: vscode.WebviewView): Promise<void> {
    const listening = this.attach(view.webview, undefined);
    view.onDidDispose(() => listening.then((d) => d.dispose()));
    return listening.then(() => undefined);
  }

  // the same page as an editor tab, with room to read a long function's logic
  async openPanel(): Promise<void> {
    if (this.panel) {
      this.panel.reveal(vscode.ViewColumn.Beside);
      return;
    }
    const panel = vscode.window.createWebviewPanel(
      'ccc.visualiserPanel',
      'ccc Visualiser',
      vscode.ViewColumn.Beside,
      { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [] },
    );
    this.panel = panel;
    const listening = this.attach(panel.webview, vscode.ViewColumn.One);
    panel.onDidDispose(() => {
      this.panel = undefined;
      void listening.then((d) => d.dispose());
    });
  }

  private async attach(webview: vscode.Webview, openIn: vscode.ViewColumn | undefined): Promise<vscode.Disposable> {
    webview.options = { enableScripts: true, localResourceRoots: [] };
    webview.html = notice('Starting the ccc analyser…');
    const session = await this.session();
    if (!session) {
      webview.html = notice('Open a folder ccc can map to see its architecture here.');
      return new vscode.Disposable(() => undefined);
    }
    const listener = webview.onDidReceiveMessage((m: unknown) => void this.onMessage(webview, session, openIn, m));
    await this.load(webview, session);
    return listener;
  }

  // The page as the analyser serves it now. The webview keeps the copy it was
  // given, so a restarted analyser - perhaps a newer build - would otherwise
  // be shown through the page of the one before it; the page asks for this
  // again when it sees the analyser start over.
  private async load(webview: vscode.Webview, session: WorkspaceSession): Promise<void> {
    try {
      const page = await session.fetchRaw('/vis');
      webview.html = page.status === 200 ? withPolicy(page.body) : notice(`The analyser would not serve the visualiser: ${errorOf(page.body)}`);
    } catch (err) {
      this.log.error('could not load the visualiser', err);
      webview.html = notice(`Could not reach the ccc analyser: ${describe(err)}`);
    }
  }

  private async onMessage(
    webview: vscode.Webview,
    session: WorkspaceSession,
    openIn: vscode.ViewColumn | undefined,
    raw: unknown,
  ): Promise<void> {
    const m = raw as { type?: string; id?: number; url?: string; file?: string; line?: number };
    if (m.type === 'get' && typeof m.id === 'number' && typeof m.url === 'string') {
      const url = m.url;
      if (!ALLOWED.some((p) => url.startsWith(p))) {
        void webview.postMessage({ type: 'res', id: m.id, error: `the visualiser may not ask for ${url}` });
        return;
      }
      try {
        const res = await session.fetchRaw(url);
        void webview.postMessage({ type: 'res', id: m.id, status: res.status, body: res.body });
      } catch (err) {
        void webview.postMessage({ type: 'res', id: m.id, error: describe(err) });
      }
    } else if (m.type === 'open' && typeof m.file === 'string') {
      await this.open(session, m.file, typeof m.line === 'number' ? m.line : 1, openIn);
    } else if (m.type === 'reload') {
      await this.load(webview, session);
    }
  }

  // a file the page names, relative to the folder and never outside it
  private async open(
    session: WorkspaceSession,
    file: string,
    line: number,
    column: vscode.ViewColumn | undefined,
  ): Promise<void> {
    const parts = file.split('/').filter((p) => p !== '' && p !== '.');
    if (parts.some((p) => p === '..') || file.startsWith('/')) return;
    const at = Math.max(0, line - 1);
    try {
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.joinPath(session.root, ...parts));
      await vscode.window.showTextDocument(doc, {
        viewColumn: column,
        preview: true,
        selection: new vscode.Range(at, 0, at, 0),
      });
    } catch (err) {
      void vscode.window.showWarningMessage(`ccc: could not open ${file}: ${describe(err)}`);
    }
  }

  dispose(): void {
    this.panel?.dispose();
  }
}

// The page as served, locked down for a webview: nothing loads from anywhere,
// and only its own inline script runs.
function withPolicy(html: string): string {
  const nonce = randomBytes(16).toString('base64');
  const policy = `default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'nonce-${nonce}';`;
  return html
    .replace('<head>', `<head>\n<meta http-equiv="Content-Security-Policy" content="${policy}">`)
    .replace(/<script>/g, `<script nonce="${nonce}">`);
}

function notice(text: string): string {
  const safe = text.replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
  return `<!DOCTYPE html><html><head><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline';"></head>
<body style="font-family: var(--vscode-font-family); color: var(--vscode-foreground); padding: 12px;">${safe}</body></html>`;
}

function errorOf(body: string): string {
  try {
    return (JSON.parse(body) as { error?: string }).error ?? body.slice(0, 200);
  } catch {
    return body.slice(0, 200);
  }
}

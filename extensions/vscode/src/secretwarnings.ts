import * as vscode from 'vscode';
import type { InsightsPayload, SecretFinding } from './types';

// The lines a branch adds that look like a credential, as a warning on each
// line and a message when one first appears - said while it can still be
// taken out, before it is committed or pushed.
export class SecretWarnings implements vscode.Disposable {
  private readonly diagnostics = vscode.languages.createDiagnosticCollection('ccc-secrets');
  // per folder: the files marked now, and every finding already announced
  private readonly marked = new Map<string, vscode.Uri[]>();
  private readonly announced = new Map<string, Set<string>>();

  update(folder: vscode.Uri, payload: InsightsPayload | undefined): void {
    const changes = payload?.changes;
    // a change set that could not be read says nothing either way - what is shown stays
    if (!changes || !('secrets' in changes) || !changes.secrets) return;
    const findings = changes.secrets.findings;
    const key = folder.toString();
    for (const uri of this.marked.get(key) ?? []) this.diagnostics.delete(uri);
    const byFile = new Map<string, SecretFinding[]>();
    for (const f of findings) byFile.set(f.file, [...(byFile.get(f.file) ?? []), f]);
    const uris: vscode.Uri[] = [];
    for (const [file, found] of byFile) {
      const uri = vscode.Uri.joinPath(folder, ...file.split('/'));
      uris.push(uri);
      this.diagnostics.set(uri, found.map(diagnostic));
    }
    this.marked.set(key, uris);

    // a finding is announced once, wherever its line moves to
    const seen = this.announced.get(key) ?? new Set<string>();
    this.announced.set(key, seen);
    const fresh = findings.filter((f) => !seen.has(identity(f)));
    for (const f of findings) seen.add(identity(f));
    if (fresh.length > 0) void announce(folder, fresh);
  }

  clear(): void {
    this.diagnostics.clear();
    this.marked.clear();
  }

  dispose(): void {
    this.diagnostics.dispose();
  }
}

const identity = (f: SecretFinding): string => `${f.file}\u0000${f.what}\u0000${f.evidence}`;

function diagnostic(f: SecretFinding): vscode.Diagnostic {
  const line = Math.max(0, f.line - 1);
  const d = new vscode.Diagnostic(
    new vscode.Range(line, 0, line, Number.MAX_SAFE_INTEGER),
    `This looks like ${f.what} (${f.evidence})${f.uncommitted ? '' : ', already committed on this branch'}. ` +
      'Once pushed it stays in git history - take it out, read it from a secret store or the environment, ' +
      'and rotate it if it is real. Mark the line ccc:allow-secret if it is meant to be there.',
    vscode.DiagnosticSeverity.Warning,
  );
  d.source = 'ccc';
  d.code = 'secret-in-change';
  return d;
}

// one message for what just appeared - the first named, the rest counted
async function announce(folder: vscode.Uri, fresh: SecretFinding[]): Promise<void> {
  const first = fresh[0];
  if (!first) return;
  const where = `${first.file}:${first.line}`;
  const text =
    fresh.length === 1
      ? `ccc: ${where} adds what looks like ${first.what}. Once it is pushed it stays in git history.`
      : `ccc: this branch adds ${fresh.length} lines that look like secrets, starting at ${where} (${first.what}). Once pushed they stay in git history.`;
  const choice = await vscode.window.showWarningMessage(text, 'Show', ...(fresh.length > 1 ? ['Show All'] : []));
  if (choice === 'Show') {
    const line = Math.max(0, first.line - 1);
    try {
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.joinPath(folder, ...first.file.split('/')));
      await vscode.window.showTextDocument(doc, { selection: new vscode.Range(line, 0, line, 0) });
    } catch {
      await vscode.commands.executeCommand('workbench.actions.view.problems');
    }
  } else if (choice === 'Show All') {
    await vscode.commands.executeCommand('workbench.actions.view.problems');
  }
}

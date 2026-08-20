import * as path from 'node:path';
import * as vscode from 'vscode';
import { keyOfPath } from './pathkeys';

export { isTestPath, keyOfPath } from './pathkeys';

// a repo-relative, '/'-separated path as the analyser emits them - undefined outside the folder
export function relOf(root: vscode.Uri, uri: vscode.Uri): string | undefined {
  if (uri.scheme !== 'file') return undefined;
  const rel = path.relative(root.fsPath, uri.fsPath);
  if (rel.length === 0) return undefined;
  if (rel.startsWith('..') || path.isAbsolute(rel)) return undefined;
  return rel.split(path.sep).join('/');
}

// resolve a repo-relative analyser path back to an absolute URI
export function absOf(root: vscode.Uri, rel: string): vscode.Uri {
  return vscode.Uri.joinPath(root, ...rel.split('/').filter((s) => s.length > 0));
}

// the index key for a document
export function keyOf(uri: vscode.Uri): string {
  return uri.scheme === 'file' ? keyOfPath(uri.fsPath) : uri.toString();
}

// only real files on disk get hints - `untitled:` is never in the map and `git:` is another revision
export function isSupportedDocument(doc: vscode.TextDocument): boolean {
  return doc.uri.scheme === 'file';
}

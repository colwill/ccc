import * as vscode from 'vscode';
import type { Cfg } from './config';
import { addTestPhrase, type Coverage, type FileHints, type Hot, type InboundRef, type LineHint, type OutboundRef } from './model';

// what a lens needs to resolve hints for a document
export type HintLookup = (
  uri: vscode.Uri,
) => Promise<{ hints: FileHints | undefined; stale: boolean } | undefined>;

// hints as CodeLens lines - one lens per aspect, since VS Code lays them out side by side on a row
export class CccCodeLensProvider implements vscode.CodeLensProvider {
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.changed.event;
  private shown: string | undefined;

  constructor(
    private readonly lookup: HintLookup,
    private cfg: Cfg,
  ) {}

  updateConfig(cfg: Cfg): void {
    this.cfg = cfg;
    this.refresh();
  }

  // announce new lenses - the signature no-ops an unchanged analysis so the lenses do not twitch
  refresh(signature?: string): void {
    if (signature !== undefined && signature === this.shown) return;
    this.shown = signature;
    this.changed.fire();
  }

  async provideCodeLenses(
    document: vscode.TextDocument,
    token: vscode.CancellationToken,
  ): Promise<vscode.CodeLens[]> {
    if (!this.cfg.enable || !this.cfg.hints.codeLens) return [];
    if (document.uri.scheme !== 'file') return [];

    const found = await this.lookup(document.uri);
    if (token.isCancellationRequested || !found?.hints) return [];

    const lenses: vscode.CodeLens[] = [];
    for (const hint of found.hints.lines.values()) {
      const line = hint.anchor.line - 1;
      if (line < 0 || line >= document.lineCount) continue;
      // anchor on the whole line - a lens is positioned by its range's start line only
      const range = new vscode.Range(line, 0, line, 0);
      for (const lens of lensesFor(hint, found.stale)) {
        lenses.push(new vscode.CodeLens(range, lens));
      }
    }
    return lenses;
  }

  dispose(): void {
    this.changed.dispose();
  }
}

function lensesFor(hint: LineHint, stale: boolean): vscode.Command[] {
  const out: vscode.Command[] = [];
  const suffix = stale ? ' (unsaved)' : '';

  if (hint.coverage) {
    const lens = coverageLens(hint.coverage, suffix);
    if (lens) out.push(lens);
  }
  for (const command of outboundLenses(hint.outbound, suffix)) out.push(command);
  const inbound = inboundLens(hint.inbound, suffix);
  if (inbound) out.push(inbound);
  if (hint.hot) {
    const lens = hotLens(hint.hot, suffix);
    if (lens) out.push(lens);
  }
  return out;
}

function coverageLens(cov: Coverage, suffix: string): vscode.Command | undefined {
  switch (cov.status) {
    case 'tested': {
      const n = cov.tests.length;
      return {
        title: `$(beaker)  ${n} ${n === 1 ? 'test' : 'tests'}${suffix}`,
        tooltip: `${cov.fn} changed and ${n === 1 ? 'one test covers' : `${n} tests cover`} it`,
        command: 'ccc.showTests',
        arguments: [{ tests: cov.tests, fn: cov.fn }],
      };
    }
    case 'untested': {
      return {
        title: `$(warning)  ${addTestPhrase(cov.target?.kind)}${suffix}`,
        tooltip: cov.target?.suggest ?? `${cov.fn} changed and no test covers it`,
        command: 'ccc.explainUntested',
        arguments: [{ coverage: cov }],
      };
    }
    case 'test-code':
      return undefined;
  }
}

function outboundLenses(refs: OutboundRef[], suffix: string): vscode.Command[] {
  if (refs.length === 0) return [];
  // group by destination so three calls to one service read as one lens
  const byService = new Map<string, OutboundRef[]>();
  for (const ref of refs) {
    const list = byService.get(ref.toService);
    if (list) list.push(ref);
    else byService.set(ref.toService, [ref]);
  }

  const out: vscode.Command[] = [];
  for (const [service, group] of byService) {
    const first = group[0];
    if (!first) continue;
    const remote = group.find((r) => r.remote)?.remote;

    if (remote && !remote.answered) {
      out.push({
        title: `$(question)  ${remote.key} unanswered${suffix}`,
        tooltip:
          `Nothing serves \`${remote.key}\`. Either the key is spelled differently at the ` +
          'other end, or the repository that serves it is not listed under `externals` in .ccc/map.json.',
        command: 'ccc.explainCrossing',
        arguments: [{ refs: group }],
      });
      continue;
    }

    const where = remote?.repo ? ` in ${remote.repo}` : '';
    const label = remote
      ? `$(arrow-up)  calls ${service}${where}${suffix}`
      : `$(arrow-up)  calls ${service}${group.length > 1 ? ` (${group.length})` : ''}${suffix}`;
    out.push({
      title: label,
      tooltip: remote
        ? `${remote.transport} · ${remote.key}${remote.language ? ` · ${remote.language}` : ''}`
        : group.map((r) => r.symbol).join(', '),
      command: 'ccc.openCrossing',
      arguments: [{ refs: group, service }],
    });
  }
  return out;
}

function inboundLens(refs: InboundRef[], suffix: string): vscode.Command | undefined {
  if (refs.length === 0) return undefined;
  const services = [...new Set(refs.map((r) => r.fromService))];
  const remote = refs.find((r) => r.remote)?.remote;
  const head = services[0] ?? '';
  const more = services.length > 1 ? ` +${services.length - 1}` : '';
  return {
    title: `$(arrow-down)  called by ${head}${more}${suffix}`,
    tooltip: remote
      ? `${remote.transport} · ${remote.key}${remote.repo ? ` · ${remote.repo}` : ''}`
      : refs.map((r) => `${r.fromService}.${r.symbol}`).join(', '),
    command: 'ccc.showCallers',
    arguments: [{ refs }],
  };
}

function hotLens(hot: Hot, suffix: string): vscode.Command | undefined {
  // complexity never shows here - the band beside the name carries it, so the flame means one thing
  const reason = hot.reasons.find((r) => r.kind !== 'most_complex');
  let title: string;
  if (reason) {
    title =
      reason.kind === 'cycle'
        ? `$(sync)  cycle of ${reason.value}${suffix}`
        : reason.kind === 'most_called'
          ? `$(flame)  ${reason.value} caller${reason.value === 1 ? '' : 's'}${suffix}`
          : reason.kind === 'widest'
            ? `$(flame)  calls ${reason.value}${suffix}`
            : `$(flame)  ${reason.value} deep${suffix}`;
  } else {
    // ranked only by complexity, but its row still knows how called it is
    const callers = hot.row?.callers;
    if (typeof callers !== 'number') return undefined;
    title = `$(flame)  ${callers} caller${callers === 1 ? '' : 's'}${suffix}`;
  }
  return {
    title,
    tooltip: 'Structural, not measured: ranked by call-graph shape, not execution frequency.',
    command: 'ccc.showReferences',
    arguments: [{ symbol: hot.fn }],
  };
}

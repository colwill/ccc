import * as vscode from 'vscode';
import type { HintIndex } from './model';
import type { ServerState } from './server';

// `unchanged` and `unmapped` look identical in the editor, so only the status bar can tell them apart
export type ActiveFileState = 'hints' | 'unchanged' | 'unmapped';

export interface StatusView {
  server: ServerState;
  index?: HintIndex;
  // the active document has unsaved edits
  dirty: boolean;
  // hints are switched off in settings
  disabled: boolean;
  // what the analyser has to say about the active file
  activeFile?: ActiveFileState;
}

export class StatusBar implements vscode.Disposable {
  private readonly item: vscode.StatusBarItem;
  // what the item currently shows; `undefined` while it is hidden
  private shown: string | undefined;

  constructor() {
    this.item = vscode.window.createStatusBarItem('ccc', vscode.StatusBarAlignment.Right, 100);
    this.item.name = 'ccc';
    this.item.command = 'ccc.showOutput';
  }

  hide(): void {
    if (this.shown === undefined) return;
    this.shown = undefined;
    this.item.hide();
  }

  // writes only on a real change - assigning to a StatusBarItem re-renders it and strobes any open hover
  update(view: StatusView): void {
    const { text, tooltip, background } = render(view);
    // compare the rendered markdown, not the MarkdownString - a fresh instance is built on every call
    const signature = `${text}\u001f${background ?? ''}\u001f${tooltip.value}`;
    if (signature === this.shown) return;
    this.shown = signature;
    this.item.text = text;
    this.item.tooltip = tooltip;
    this.item.backgroundColor = background ? new vscode.ThemeColor(background) : undefined;
    this.item.show();
  }

  dispose(): void {
    this.item.dispose();
  }
}

function render(view: StatusView): {
  text: string;
  tooltip: vscode.MarkdownString;
  background?: string;
} {
  const md = new vscode.MarkdownString();
  md.isTrusted = { enabledCommands: ['ccc.showOutput', 'ccc.refresh', 'ccc.selectBaseRef'] };

  if (view.disabled) {
    md.appendMarkdown('ccc hints are off — run **ccc: Toggle Inline Hints** to switch them back on.');
    return { text: '$(circle-slash) ccc', tooltip: md };
  }

  switch (view.server.kind) {
    case 'stopped':
      md.appendMarkdown('The ccc analyser is not running.');
      return { text: '$(circle-slash) ccc', tooltip: md };
    case 'starting':
      md.appendMarkdown('Starting the ccc analyser…');
      return { text: '$(sync~spin) ccc', tooltip: md };
    case 'failed':
      md.appendMarkdown(`**ccc analyser failed**\n\n${view.server.error}\n\n[Show log](command:ccc.showOutput)`);
      return { text: '$(error) ccc', tooltip: md, background: 'statusBarItem.errorBackground' };
    case 'running':
      break;
  }

  const index = view.index;
  if (!index) {
    md.appendMarkdown('The ccc analyser is running; no analysis has been read yet.');
    return { text: '$(sync~spin) ccc', tooltip: md };
  }

  if (index.changes.available === false) {
    md.appendMarkdown(`**No change set** — ${index.changes.reason}`);
    if (index.changes.hint) md.appendMarkdown(`\n\n${index.changes.hint}`);
    md.appendMarkdown('\n\n[Select a base ref](command:ccc.selectBaseRef) · [Show log](command:ccc.showOutput)');
    return {
      text: '$(beaker) ccc $(warning)',
      tooltip: md,
      background: 'statusBarItem.warningBackground',
    };
  }

  const { tested, untested, outbound, inbound, changed } = index.counts;
  const parts: string[] = [];
  if (tested > 0) parts.push(`$(check)${tested}`);
  if (untested > 0) parts.push(`$(warning)${untested}`);
  const suffix = view.dirty ? ' $(circle-outline)' : '';
  const text = `$(beaker) ccc${parts.length > 0 ? ` ${parts.join(' ')}` : ''}${suffix}`;

  md.appendMarkdown(`**ccc** - ${changed} changed ${changed === 1 ? 'function' : 'functions'}`);
  md.appendMarkdown(`\n\n- ${tested} covered by tests\n- ${untested} with no test`);
  if (index.crossServiceEnabled) {
    md.appendMarkdown(`\n- ${outbound} outbound, ${inbound} inbound cross-boundary calls`);
  } else {
    md.appendMarkdown(
      `\n- cross-service hints off: boundaries are ${index.serviceSource || 'not configured'}`,
    );
  }
  if (index.base) md.appendMarkdown(`\n\nComparing base: \`${index.base}\``);
  if (view.activeFile === 'unmapped') {
    md.appendMarkdown(
      '\n\n $(info) This file is not in the ccc map — it is ignored by git, or written in a ' +
        'language ccc does not parse.',
    );
  } else if (view.activeFile === 'unchanged') {
    md.appendMarkdown(
      `\n\n $(info) Nothing in this file changed against \`${index.base ?? 'the base ref'}\`, and ` +
        'no hot path runs through it, so there is nothing to mark.',
    );
  }
  if (view.dirty) md.appendMarkdown('\n\nHints reflect the last saved state.');
  md.appendMarkdown('\n\n[Refresh](command:ccc.refresh) · [Show log](command:ccc.showOutput)');

  return { text, tooltip: md };
}

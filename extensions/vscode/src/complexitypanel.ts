import * as vscode from 'vscode';
import { ARITIES, type HintIndex, SCORE_DESCRIPTION } from './model';
import { absOf } from './paths';
import type { WorkspaceSession } from './session';
import type { Arity, ComplexityRow } from './types';

// complexity as a filterable list - no decoration can answer a question about the whole map
export class ComplexityPanel implements vscode.TreeDataProvider<Node>, vscode.Disposable {
  private readonly changed = new vscode.EventEmitter<Node | undefined>();
  readonly onDidChangeTreeData = this.changed.event;
  private readonly view: vscode.TreeView<Node>;
  private shown: string | undefined;
  private readonly filter: Filter = { name: '', arities: new Set(), minScore: 1, maxScore: 10, tests: false };

  private readonly visibility: vscode.Disposable;

  constructor(
    private readonly sessions: () => WorkspaceSession[],
    wake?: () => void,
  ) {
    this.view = vscode.window.createTreeView('ccc.complexity', {
      treeDataProvider: this,
      showCollapseAll: true,
    });
    this.publishFilterState();
    // opening the panel is itself a request for ccc, so let it start the analyser
    this.visibility = this.view.onDidChangeVisibility((e) => {
      if (e.visible) wake?.();
    });
  }

  // rebuild only when the analysis moved so expansion state survives typing
  refresh(): void {
    const signature = JSON.stringify([
      this.sessions().map((s) => [s.folder.name, s.index?.generated ?? '', s.index?.complexity.length ?? 0]),
      this.filterSignature(),
    ]);
    if (signature === this.shown) return;
    this.shown = signature;
    this.changed.fire(undefined);
    this.view.description = this.describe();
  }

  async filterByName(): Promise<void> {
    const value = await vscode.window.showInputBox({
      title: 'Filter functions by name',
      prompt: 'Substring match, case-insensitive. Empty clears it.',
      value: this.filter.name,
    });
    if (value === undefined) return; // dismissed, which is not the same as cleared
    this.filter.name = value.trim();
    this.apply();
  }

  async filterByArity(): Promise<void> {
    const picked = await vscode.window.showQuickPick(
      ARITIES.map((a) => ({
        label: a,
        description: ARITY_DESCRIPTION[a],
        picked: this.filter.arities.has(a),
      })),
      {
        title: 'Show which parameter counts?',
        placeHolder: 'Pick none to show every function',
        canPickMany: true,
      },
    );
    if (picked === undefined) return;
    this.filter.arities = new Set(picked.map((p) => p.label as Arity));
    this.apply();
  }

  async filterByScore(): Promise<void> {
    const pick = async (title: string, fallback: number): Promise<number | undefined> => {
      const chosen = await vscode.window.showQuickPick(
        Array.from({ length: 10 }, (_, i) => ({
          label: `${i + 1}`,
          description: SCORE_DESCRIPTION[i + 1] ?? '',
        })),
        { title, placeHolder: `currently ${fallback}` },
      );
      return chosen ? Number(chosen.label) : undefined;
    };
    const min = await pick('Lowest complexity band to show', this.filter.minScore);
    if (min === undefined) return;
    const max = await pick('Highest complexity band to show', Math.max(this.filter.maxScore, min));
    if (max === undefined) return;
    // a reader who picks 8 then 3 means 3-8, not an empty list
    this.filter.minScore = Math.min(min, max);
    this.filter.maxScore = Math.max(min, max);
    this.apply();
  }

  async toggleTests(): Promise<void> {
    this.filter.tests = !this.filter.tests;
    this.apply();
  }

  clearFilters(): void {
    this.filter.name = '';
    this.filter.arities = new Set();
    this.filter.minScore = 1;
    this.filter.maxScore = 10;
    this.filter.tests = false;
    this.apply();
  }

  private apply(): void {
    this.publishFilterState();
    this.shown = undefined;
    this.refresh();
  }

  private publishFilterState(): void {
    void vscode.commands.executeCommand('setContext', 'ccc.complexityFiltered', this.isFiltered());
  }

  private isFiltered(): boolean {
    const f = this.filter;
    return f.name.length > 0 || f.arities.size > 0 || f.minScore > 1 || f.maxScore < 10 || f.tests;
  }

  private filterSignature(): string {
    const f = this.filter;
    return [f.name, [...f.arities].sort().join('+'), f.minScore, f.maxScore, f.tests].join('|');
  }

  getTreeItem(node: Node): vscode.TreeItem {
    return node.item;
  }

  getChildren(node?: Node): Node[] {
    const sessions = this.sessions();
    if (node) return node.children ?? [];
    if (sessions.length === 0) return [message('The ccc analyser is not running.')];

    const nodes: Node[] = [];
    for (const session of sessions) {
      const index = session.index;
      if (!index) {
        nodes.push(message(`${session.folder.name}: analysing…`));
        continue;
      }
      const rows = this.matching(index);
      const groups = rows.length === 0 ? [this.emptyMessage(index)] : bands(session, rows);
      if (sessions.length > 1) nodes.push(folder(session.folder.name, groups));
      else nodes.push(...groups);
    }
    return nodes;
  }

  private matching(index: HintIndex): ComplexityRow[] {
    const f = this.filter;
    const needle = f.name.toLowerCase();
    return index.complexity.filter((row) => {
      if (!f.tests && row.test) return false;
      if (row.score < f.minScore || row.score > f.maxScore) return false;
      if (f.arities.size > 0 && !f.arities.has(row.arity)) return false;
      if (needle.length > 0 && !row.function.toLowerCase().includes(needle)) return false;
      return true;
    });
  }

  // an empty list has two causes a reader cannot tell apart - nothing measured, or all filtered out
  private emptyMessage(index: HintIndex): Node {
    if (index.complexity.length === 0) return message('Nothing measured yet.');
    return this.isFiltered()
      ? message(
          `No function matches the filter (${index.complexity.length} measured).`,
          'filter',
          'ccc.clearComplexityFilters',
          'Clear filters',
        )
      : message('Nothing measured yet.');
  }

  private describe(): string {
    const index = this.sessions()[0]?.index;
    if (!index) return '';
    const shown = this.matching(index).length;
    const parts: string[] = [];
    const f = this.filter;
    if (f.name.length > 0) parts.push(`"${f.name}"`);
    if (f.arities.size > 0) parts.push([...f.arities].join('/'));
    if (f.minScore > 1 || f.maxScore < 10) parts.push(`${f.minScore}-${f.maxScore}`);
    if (f.tests) parts.push('+tests');
    const of = index.complexityTruncated ? `${index.complexity.length} of ${index.complexityTotal}` : `${index.complexity.length}`;
    const head = this.isFiltered() ? `${shown} of ${of}` : of;
    return parts.length > 0 ? `${head} · ${parts.join(' · ')}` : head;
  }

  dispose(): void {
    this.visibility.dispose();
    this.view.dispose();
    this.changed.dispose();
  }
}

interface Filter {
  name: string;
  arities: Set<Arity>;
  minScore: number;
  maxScore: number;
  tests: boolean;
}

interface Node {
  item: vscode.TreeItem;
  children?: Node[];
}

const ARITY_DESCRIPTION: Record<Arity, string> = {
  niladic: 'no parameters',
  monadic: 'one parameter',
  dyadic: 'two parameters',
  variadic: 'three or more',
};

// the same glyphs the editor draws so the panel and the code read alike
const GLYPH = ['❶', '❷', '❸', '❹', '❺', '❻', '❼', '❽', '❾', '❿'];

// grouped by band rather than listed flat
function bands(session: WorkspaceSession, rows: ComplexityRow[]): Node[] {
  const byScore = new Map<number, ComplexityRow[]>();
  for (const row of rows) {
    const list = byScore.get(row.score);
    if (list) list.push(row);
    else byScore.set(row.score, [row]);
  }
  return [...byScore.entries()]
    .sort((a, b) => b[0] - a[0])
    .map(([score, group]) => {
      const item = new vscode.TreeItem(
        `${GLYPH[score - 1] ?? score}  ${score}/10 - ${SCORE_DESCRIPTION[score] ?? ''}`,
        // the worst band opens expanded - the rest stay out of the way
        score >= 7 ? vscode.TreeItemCollapsibleState.Expanded : vscode.TreeItemCollapsibleState.Collapsed,
      );
      item.description = `${group.length}`;
      return { item, children: group.map((row) => functionNode(session, row)) };
    });
}

function functionNode(session: WorkspaceSession, row: ComplexityRow): Node {
  const item = new vscode.TreeItem(row.function);
  item.description = `${row.arity} · ${row.file}:${row.line}`;
  item.iconPath = new vscode.ThemeIcon(row.recursive ? 'sync' : 'symbol-function');
  const detail: string[] = [`${row.complexity} independent path(s)`, `${row.params} parameter(s)`];
  if (row.loop_depth > 0) detail.push(`${row.loop_depth} nested loop level(s)`);
  if (row.lines > 0) detail.push(`${row.lines} lines`);
  if (row.recursive) detail.push('recursive');
  item.tooltip = new vscode.MarkdownString(
    `**${row.function}** - complexity ${row.score}/10, _${SCORE_DESCRIPTION[row.score] ?? ''}_\n\n` +
      `${detail.join(' · ')}\n\n\`${row.file}:${row.line}\``,
  );
  const line = Math.max(0, row.line - 1);
  item.command = {
    command: 'vscode.open',
    title: 'Open',
    arguments: [absOf(session.root, row.file), { selection: new vscode.Range(line, 0, line, 0) }],
  };
  return { item };
}

function folder(name: string, children: Node[]): Node {
  const item = new vscode.TreeItem(name, vscode.TreeItemCollapsibleState.Expanded);
  item.iconPath = new vscode.ThemeIcon('folder');
  return { item, children };
}

function message(text: string, icon = 'info', command?: string, title?: string): Node {
  const item = new vscode.TreeItem(text);
  item.iconPath = new vscode.ThemeIcon(icon);
  if (command) item.command = { command, title: title ?? text };
  return { item };
}

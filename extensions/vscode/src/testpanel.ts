import * as vscode from 'vscode';
import { type HintIndex, missingTestPhrase } from './model';
import { absOf } from './paths';
import type { WorkspaceSession } from './session';
import type { TestRun, TestTarget, TriggerAdd } from './types';

// the tests your working tree makes necessary - a triggered test lives in another file
export class TestTriggerPanel implements vscode.TreeDataProvider<Node>, vscode.Disposable {
  private readonly changed = new vscode.EventEmitter<Node | undefined>();
  readonly onDidChangeTreeData = this.changed.event;
  private readonly view: vscode.TreeView<Node>;
  private shown: string | undefined;

  private readonly visibility: vscode.Disposable;

  constructor(
    private readonly sessions: () => WorkspaceSession[],
    wake?: () => void,
  ) {
    this.view = vscode.window.createTreeView('ccc.testTriggers', {
      treeDataProvider: this,
      showCollapseAll: true,
    });
    // opening the panel is itself a request for ccc, so let it start the analyser
    this.visibility = this.view.onDidChangeVisibility((e) => {
      if (e.visible) wake?.();
    });
  }

  // rebuild only when the analysis moved - firing the event would reset expansion state
  refresh(): void {
    const index = this.sessions()[0]?.index;
    const run = index?.commands ?? [];
    const counts = index?.triggerCounts;
    const signature = JSON.stringify([
      this.sessions().map((s) => [s.folder.name, s.index?.generated ?? '', s.index?.base ?? '']),
      counts ?? null,
      run.length,
    ]);
    if (signature === this.shown) return;
    this.shown = signature;

    this.changed.fire(undefined);
    // the badge is the number worth acting on, not the total
    this.view.badge = counts?.run
      ? { value: counts.run, tooltip: `${counts.run} test(s) triggered by your changes` }
      : undefined;
    this.view.description = describe(index, run.length);
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
      if (index.triggers.available === false) {
        nodes.push(
          message(
            `No change set — ${index.triggers.reason}`,
            'warning',
            'ccc.selectBaseRef',
            'Select a base ref',
          ),
        );
        continue;
      }
      const multi = sessions.length > 1;
      const groups = groupsFor(session, index);
      if (multi) {
        nodes.push(folder(session.folder.name, groups));
      } else {
        nodes.push(...groups);
      }
    }
    return nodes;
  }

  dispose(): void {
    this.visibility.dispose();
    this.view.dispose();
    this.changed.dispose();
  }
}

interface Node {
  item: vscode.TreeItem;
  children?: Node[];
}

function groupsFor(session: WorkspaceSession, index: HintIndex): Node[] {
  const out: Node[] = [];

  // 1. what to run
  const run = index.triggerRun;
  if (run.length > 0) {
    out.push(
      group(
        `Run these (${run.length})`,
        'beaker',
        run.map((test) => testNode(session, test)),
        vscode.TreeItemCollapsibleState.Expanded,
      ),
    );
  }

  // 2. what nothing covers
  const gaps = index.triggerGaps;
  if (gaps.length > 0) {
    out.push(
      group(
        `No test covers (${gaps.length})`,
        'warning',
        gaps.map((gap) => gapNode(session, gap, index)),
        vscode.TreeItemCollapsibleState.Expanded,
      ),
    );
  }

  // 3. the commands that select exactly the above
  if (index.commands.length > 0) {
    out.push(
      group(
        'Commands',
        'terminal',
        index.commands.map((command) => ({
          item: Object.assign(new vscode.TreeItem(command.command), {
            description: `${command.language} · selects ${command.selects}`,
            tooltip: command.caveat ?? command.command,
            iconPath: new vscode.ThemeIcon('terminal'),
            command: {
              command: 'ccc.runTestCommand',
              title: 'Run',
              arguments: [{ command: command.command, cwd: session.root.fsPath }],
            },
          }),
        })),
        vscode.TreeItemCollapsibleState.Collapsed,
      ),
    );
  }

  if (out.length === 0) {
    out.push(
      message(
        index.changes.available === false
          ? 'No change set.'
          : `Nothing changed against ${index.base ?? 'the base ref'}.`,
      ),
    );
  }
  return out;
}

function testNode(session: WorkspaceSession, test: TestRun): Node {
  const item = new vscode.TreeItem(test.test);
  item.description = `${test.file}:${test.line}`;
  item.iconPath = new vscode.ThemeIcon(test.distance === 0 ? 'testing-passed-icon' : 'testing-queued-icon');
  item.tooltip = new vscode.MarkdownString(
    `**${test.test}**\n\n${test.reason}\n\n` +
      (test.distance === 0
        ? '_References the change directly._'
        : `_${test.distance} call hop(s) from the change._`) +
      (test.covers.length > 0 ? `\n\nCovers: \`${test.covers.join('`, `')}\`` : ''),
  );
  item.command = {
    command: 'vscode.open',
    title: 'Open',
    arguments: [
      absOf(session.root, test.file),
      { selection: new vscode.Range(Math.max(0, test.line - 1), 0, Math.max(0, test.line - 1), 0) },
    ],
  };
  return { item };
}

function gapNode(session: WorkspaceSession, gap: TriggerAdd, index: HintIndex): Node {
  const [file = '', fn = gap.target] = splitTarget(gap.target);
  const target: TestTarget | undefined = index.targetsById.get(gap.target);
  const item = new vscode.TreeItem(fn);
  item.description = `${file} · ${missingTestPhrase(target?.kind)}`;
  item.iconPath = new vscode.ThemeIcon('warning');
  item.tooltip = new vscode.MarkdownString(
    `**${fn}** changed and ${missingTestPhrase(target?.kind)} covers it.\n\n${target?.suggest ?? ''}`,
  );
  const line = Math.max(0, (gap.lines?.[0] ?? target?.line ?? 1) - 1);
  item.command = {
    command: 'vscode.open',
    title: 'Open',
    arguments: [absOf(session.root, file), { selection: new vscode.Range(line, 0, line, 0) }],
  };
  return { item };
}

function splitTarget(id: string): [string, string] {
  const at = id.lastIndexOf('::');
  return at < 0 ? ['', id] : [id.slice(0, at), id.slice(at + 2)];
}

function group(
  label: string,
  icon: string,
  children: Node[],
  state = vscode.TreeItemCollapsibleState.Collapsed,
): Node {
  const item = new vscode.TreeItem(label, state);
  item.iconPath = new vscode.ThemeIcon(icon);
  return { item, children };
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

function describe(index: HintIndex | undefined, commands: number): string {
  if (!index) return '';
  if (index.triggers.available === false) return 'no change set';
  const { run, gaps } = index.triggerCounts;
  if (run === 0 && gaps === 0) return 'nothing changed';
  const parts = [`${run} to run`];
  if (gaps > 0) parts.push(`${gaps} uncovered`);
  if (commands > 0) parts.push(`${commands} command(s)`);
  return parts.join(' · ');
}

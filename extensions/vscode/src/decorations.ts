import * as vscode from 'vscode';
import type { Cfg } from './config';
import type { Anchor, FileHints, HintIndex, HintKind, LineHint } from './model';
import type { FileStructure } from './types';

const KINDS: HintKind[] = ['untested', 'tested', 'test-code', 'outbound', 'inbound', 'hot', 'cycle'];


// gutter icon file stem showing direction of deps
const GUTTER_ICON: Partial<Record<HintKind, string>> = {
  outbound: 'outbound',
  inbound: 'inbound',
};


// badge colours are ThemeColor only
const BADGE_COLOR: Record<HintKind, string> = {
  untested: 'editorWarning.foreground',
  tested: 'charts.green',
  'test-code': 'descriptionForeground',
  outbound: 'charts.purple',
  inbound: 'charts.blue',
  hot: 'charts.orange',
  cycle: 'charts.purple',
};


// the complexity band - one glyph consistent width
const COMPLEXITY_GLYPH = ['❶', '❷', '❸', '❹', '❺', '❻', '❼', '❽', '❾', '❿'];


// one colour per band
const COMPLEXITY_COLOR = [
  'ccc.complexity1',    // 1  grey - straight line
  'ccc.complexity2',    // 2  plain
  'charts.green',       // 3  green
  'charts.blue',        // 4  blue
  'charts.purple',      // 5  purple
  'charts.yellow',      // 6  yellow
  'ccc.complexity7',    // 7  brown
  'ccc.complexity8',    // 8  amber
  'charts.orange',      // 9  orange
  'charts.red',         // 10  red
];

type DecoKey = `${HintKind}:${'fresh' | 'stale'}`;

// the decoration types, created once and reused
export class DecorationSet implements vscode.Disposable {
  private types = new Map<DecoKey, vscode.TextEditorDecorationType>();
  private complexityTypes = new Map<'fresh' | 'stale', vscode.TextEditorDecorationType>();

  constructor(
    private readonly ctx: vscode.ExtensionContext,
    private cfg: Cfg,
  ) {
    this.create();
  }

  reload(cfg: Cfg): void {
    this.cfg = cfg;
    this.disposeTypes();
    this.create();
  }

  private create(): void {
    // created before the hint types so it draws closest to the name
    for (const fresh of [true, false]) {
      this.complexityTypes.set(
        fresh ? 'fresh' : 'stale',
        vscode.window.createTextEditorDecorationType({
          rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
          isWholeLine: false,
          // sits between the name and the signature - a thin space each side so it reads as part of neither
          after: { margin: '0 0.25em 0 0.25em' },
          ...(fresh ? {} : { opacity: '0.45' }),
        }),
      );
    }

    const withGutter = this.cfg.decorations.style !== 'badge';
    for (const kind of KINDS) {
      for (const fresh of [true, false]) {
        const options: vscode.DecorationRenderOptions = {
          // stop a decoration growing when the user types at its edge
          rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
          isWholeLine: false,
          after: { margin: '0 0 0 1.5em', fontStyle: 'italic' },
        };
        const stem = GUTTER_ICON[kind];
        if (withGutter && stem) {
          options.gutterIconSize = 'contain';
          options.light = { gutterIconPath: this.icon('light', stem) };
          options.dark = { gutterIconPath: this.icon('dark', stem) };
        }
        if (kind === 'untested' && this.cfg.decorations.overviewRuler) {
          options.overviewRulerColor = new vscode.ThemeColor('editorOverviewRuler.warningForeground');
          options.overviewRulerLane = vscode.OverviewRulerLane.Right;
        }
        if (!fresh) options.opacity = '0.45';
        this.types.set(`${kind}:${fresh ? 'fresh' : 'stale'}`, vscode.window.createTextEditorDecorationType(options));
      }
    }
  }

  private icon(theme: 'light' | 'dark', stem: string): vscode.Uri {
    return vscode.Uri.file(this.ctx.asAbsolutePath(`media/${theme}/${stem}.svg`));
  }


  // every type is set on every apply including the empty ones
  apply(editor: vscode.TextEditor, hints: FileHints | undefined, index: HintIndex | undefined, stale: boolean): void {
    const buckets = new Map<DecoKey, vscode.DecorationOptions[]>();
    const suffix: 'fresh' | 'stale' = stale && this.cfg.decorations.dimWhenDirty ? 'stale' : 'fresh';

    if (hints && index) {
      for (const hint of hints.lines.values()) {
        const range = anchorToRange(hint.anchor, editor.document);
        if (!range) continue;
        const key: DecoKey = `${hint.primary}:${suffix}`;
        // no hoverMessage here
        const option: vscode.DecorationOptions = { range };
        if (this.cfg.decorations.style !== 'gutter' && hint.badge.length > 0) {
          option.renderOptions = {
            after: {
              contentText: hint.badge,
              color: new vscode.ThemeColor(
                suffix === 'stale' ? 'descriptionForeground' : (BADGE_COLOR[hint.primary] ?? 'descriptionForeground'),
              ),
            },
          };
        }
        const list = buckets.get(key);
        if (list) list.push(option);
        else buckets.set(key, [option]);
      }
    }

    for (const [key, type] of this.types) {
      editor.setDecorations(type, buckets.get(key) ?? []);
    }
  }

  applyComplexity(editor: vscode.TextEditor, structure: FileStructure | undefined, stale: boolean): void {
    const suffix: 'fresh' | 'stale' = stale && this.cfg.decorations.dimWhenDirty ? 'stale' : 'fresh';
    const options: vscode.DecorationOptions[] = [];

    if (structure && this.cfg.complexity.enabled) {
      for (const fn of structure.funcs ?? []) {
        const score = fn.complexity_score;
        if (typeof score !== 'number' || score < 1 || score > 10) continue;
        if (score < this.cfg.complexity.minScore) continue;
        const name = typeof fn.name === 'string' ? fn.name : '';
        if (name.length === 0) continue;
        const range = anchorToRange({ line: fn.line, startCol: fn.col, endCol: fn.col + name.length }, editor.document);
        if (!range) continue;
        options.push({
          range,
          renderOptions: {
            after: {
              contentText: COMPLEXITY_GLYPH[score - 1] ?? '',
              color: new vscode.ThemeColor(
                suffix === 'stale' ? 'descriptionForeground' : (COMPLEXITY_COLOR[score - 1] ?? 'descriptionForeground'),
              ),
            },
          },
        });
      }
    }

    for (const [key, type] of this.complexityTypes) {
      editor.setDecorations(type, key === suffix ? options : []);
    }
  }

  clear(editor: vscode.TextEditor): void {
    for (const type of this.types.values()) editor.setDecorations(type, []);
    for (const type of this.complexityTypes.values()) editor.setDecorations(type, []);
  }

  private disposeTypes(): void {
    for (const type of this.types.values()) type.dispose();
    this.types.clear();
    for (const type of this.complexityTypes.values()) type.dispose();
    this.complexityTypes.clear();
  }

  dispose(): void {
    this.disposeTypes();
  }
}

export function anchorToRange(anchor: Anchor, document: vscode.TextDocument): vscode.Range | undefined {
  const line = anchor.line - 1;
  if (line < 0 || line >= document.lineCount) return undefined;
  const textRange = document.lineAt(line).range;
  const start = Math.max(0, Math.min((anchor.startCol ?? 1) - 1, textRange.end.character));
  const end =
    anchor.endCol === undefined
      ? textRange.end.character
      : Math.max(start, Math.min(anchor.endCol - 1, textRange.end.character));
  return new vscode.Range(line, start, line, end);
}

export type { LineHint };

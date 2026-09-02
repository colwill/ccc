import * as path from 'node:path';
import * as vscode from 'vscode';
import type { VulnFinding, VulnPayload } from './types';

// severities the analyser reports, worst first
const ORDER = ['critical', 'high', 'moderate', 'medium', 'low', 'rated', 'unknown'];

function rank(severity: string): number {
  const i = ORDER.indexOf(severity.toLowerCase());
  return i === -1 ? ORDER.length : i;
}

// advisories on one manifest line, and what to draw for them
interface Mark {
  line: number;
  via: string;
  hits: VulnFinding[];
}

const MAX_LISTED = 8;

// Draws dependency advisories on the manifest lines that declare them.
//
// Decorations rather than diagnostics: a diagnostic is always drawn as a wavy
// underline, and the severity picks the colour. A solid line in one colour with
// a trailing badge is only reachable through a decoration type.
//
// Everything here is idempotent. `update` returns early when the advisories have
// not moved, and drawing re-sets the same ranges rather than clearing first -
// a clear followed by a set is what makes a mark blink.
export class VulnerabilityMarks implements vscode.Disposable {
  // the solid line under the declaration itself
  private readonly underline = vscode.window.createTextEditorDecorationType({
    borderStyle: 'none none solid none',
    borderWidth: '0 0 1px 0',
    borderColor: new vscode.ThemeColor('ccc.vulnerable'),
    // a mark must not grow while the line is being edited
    rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
    overviewRulerColor: new vscode.ThemeColor('ccc.vulnerable'),
    overviewRulerLane: vscode.OverviewRulerLane.Right,
  });

  // the badge sitting past the end of the line
  private readonly badge = vscode.window.createTextEditorDecorationType({
    rangeBehavior: vscode.DecorationRangeBehavior.ClosedClosed,
  });

  // absolute manifest path -> the marks on it
  private marks = new Map<string, Mark[]>();
  // what the current marks were built from, so an unchanged payload redraws nothing
  private signature = '';
  private readonly hover: vscode.Disposable;

  constructor() {
    this.hover = vscode.languages.registerHoverProvider(
      { scheme: 'file' },
      {
        provideHover: (doc, pos) => this.hoverFor(doc, pos),
      },
    );
  }

  // returns true when the marks actually changed
  update(root: vscode.Uri, payload: VulnPayload | undefined): boolean {
    // no answer yet is not the same as no findings - keep what is on screen
    if (!payload) return false;

    const next = new Map<string, Mark[]>();
    for (const finding of payload.findings) {
      for (const loc of finding.locations) {
        const file = path.join(root.fsPath, loc.manifest);
        const list = next.get(file) ?? [];
        const slot = list.find((m) => m.line === loc.line && m.via === loc.via);
        // a transitive advisory arrives once per ancestor, so lines merge
        if (slot) slot.hits.push(finding);
        else list.push({ line: loc.line, via: loc.via, hits: [finding] });
        next.set(file, list);
      }
    }

    const signature = signatureOf(next);
    if (signature === this.signature) return false;
    this.signature = signature;
    this.marks = next;
    return true;
  }

  // draw every visible editor that has marks
  applyAll(): void {
    for (const editor of vscode.window.visibleTextEditors) this.apply(editor);
  }

  apply(editor: vscode.TextEditor): void {
    const marks = this.marks.get(editor.document.uri.fsPath);
    if (!marks || marks.length === 0) {
      // only clear an editor that could plausibly have been marked
      if (this.marks.size > 0) {
        editor.setDecorations(this.underline, []);
        editor.setDecorations(this.badge, []);
      }
      return;
    }

    const lines: vscode.Range[] = [];
    const badges: vscode.DecorationOptions[] = [];
    for (const mark of marks) {
      const row = mark.line - 1;
      if (row < 0 || row >= editor.document.lineCount) continue;
      const text = editor.document.lineAt(row);
      // underline the declaration itself, not the indent or the trailing space
      const from = text.firstNonWhitespaceCharacterIndex;
      const to = text.text.trimEnd().length;
      if (to > from) lines.push(new vscode.Range(row, from, row, to));

      const end = new vscode.Range(row, text.text.length, row, text.text.length);
      badges.push({
        range: end,
        renderOptions: {
          after: {
            contentText: `  ⚠ ${badgeText(mark)}`,
            color: new vscode.ThemeColor('ccc.vulnerable'),
            fontStyle: 'italic',
          },
        },
      });
    }
    editor.setDecorations(this.underline, lines);
    editor.setDecorations(this.badge, badges);
  }

  private hoverFor(doc: vscode.TextDocument, pos: vscode.Position): vscode.Hover | undefined {
    const marks = this.marks.get(doc.uri.fsPath);
    if (!marks) return undefined;
    const mark = marks.find((m) => m.line - 1 === pos.line);
    if (!mark) return undefined;

    const md = new vscode.MarkdownString();
    md.supportThemeIcons = true;
    md.appendMarkdown(`$(warning) **${headline(mark)}**\n\n`);

    const hits = sorted(mark.hits);
    for (const h of hits.slice(0, MAX_LISTED)) {
      const a = h.advisory;
      const ids = a.aliases.length > 0 ? `${a.id} · ${a.aliases.join(' · ')}` : a.id;
      const scope = h.package.dev ? ' _(dev only)_' : '';
      const fix = a.fixed ? `fixed in \`${a.fixed}\`` : 'no fixed version published';
      md.appendMarkdown(
        `- **${a.severity.toUpperCase()}** [${ids}](${a.url})${scope}  \n  ${a.summary}  \n  ${fix}\n`,
      );
    }
    if (hits.length > MAX_LISTED) {
      md.appendMarkdown(`\n_...and ${hits.length - MAX_LISTED} more — run \`ccc audit\` for all._\n`);
    }
    return new vscode.Hover(md, new vscode.Range(pos.line, 0, pos.line, doc.lineAt(pos.line).text.length));
  }

  dispose(): void {
    this.hover.dispose();
    this.underline.dispose();
    this.badge.dispose();
  }
}

function sorted(hits: VulnFinding[]): VulnFinding[] {
  return [...hits].sort((a, b) => rank(a.advisory.severity) - rank(b.advisory.severity));
}

// the packages a line is answerable for, named so the badge says what is wrong
function packagesOf(mark: Mark): string[] {
  return [...new Set(mark.hits.map((h) => `${h.package.name} ${h.package.version}`))];
}

function badgeText(mark: Mark): string {
  const packages = packagesOf(mark);
  const n = mark.hits.length;
  const advisories = n === 1 ? '1 advisory' : `${n} advisories`;
  return packages.length === 1
    ? `${packages[0]} — ${advisories}`
    : `${packages.length} vulnerable packages — ${advisories}`;
}

function headline(mark: Mark): string {
  const direct = mark.hits.filter((h) => h.package.name === mark.via);
  const indirect = mark.hits.filter((h) => h.package.name !== mark.via);
  if (indirect.length === 0) return `${packagesOf(mark).join(', ')} is vulnerable`;
  const names = [...new Set(indirect.map((h) => `${h.package.name} ${h.package.version}`))];
  const lead = `${mark.via} pulls in ${names.join(', ')}`;
  return direct.length > 0 ? `${lead}, and is itself vulnerable` : lead;
}

// identity of the whole mark set, so an unchanged payload redraws nothing
function signatureOf(marks: Map<string, Mark[]>): string {
  const parts: string[] = [];
  for (const file of [...marks.keys()].sort()) {
    for (const mark of marks.get(file)!) {
      const ids = mark.hits
        .map((h) => h.advisory.id)
        .sort()
        .join(',');
      parts.push(`${file}:${mark.line}:${mark.via}:${ids}`);
    }
  }
  return parts.sort().join('|');
}

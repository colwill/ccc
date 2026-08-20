import * as vscode from 'vscode';
import type { Cfg } from './config';
import { anchorToRange } from './decorations';
import {
  type Coverage,
  type FileHints,
  type HintIndex,
  type Hot,
  type InboundRef,
  type LineHint,
  missingTestPhrase,
  type OutboundRef,
  SCORE_DESCRIPTION,
  type ServiceMode,
  type TestLink,
} from './model';
import type { FileStructure, Via } from './types';

// commands a hover is allowed to invoke - see the note on isTrusted below
const TRUSTED_COMMANDS = [
  'ccc.openLocation',
  'ccc.openCrossing',
  'ccc.showTests',
  'ccc.showCallers',
  'ccc.showReferences',
  'ccc.refresh',
  'ccc.selectBaseRef',
  'ccc.copyTestCommand',
];

const VIA_PROSE: Record<Via, string> = {
  'receiver-type': 'resolved by receiver type',
  qualifier: 'resolved by qualified path',
  project: 'resolved by project name',
  import: 'resolved by import',
  'type-reference': 'used in a signature',
  'name-only': '⚠ matched by name only — this may be a different function with the same name',
};

export function buildHover(hint: LineHint, index: HintIndex, stale: boolean): vscode.MarkdownString {
  const md = new vscode.MarkdownString(undefined, true);
  // an allowlist rather than true - an injected command: link would be inert, not merely unlikely
  md.isTrusted = { enabledCommands: TRUSTED_COMMANDS };
  md.supportHtml = false;

  const sections: string[] = [];
  const coverageFirst = hint.primary !== 'outbound' && hint.primary !== 'inbound';

  if (coverageFirst && hint.coverage) sections.push(coverageSection(hint.coverage, index));
  if (hint.outbound.length > 0) sections.push(outboundSection(hint.outbound, index));
  if (hint.inbound.length > 0) sections.push(inboundSection(hint.inbound, index));
  // when a cross-service hint leads, the coverage note follows - reqs 3 and 4 carry the earlier notes
  if (!coverageFirst && hint.coverage) sections.push(coverageSection(hint.coverage, index));
  if (hint.hot) sections.push(hotSection(hint.hot, index));

  md.appendMarkdown(sections.join('\n\n---\n\n'));
  md.appendMarkdown(`\n\n${footer(index, stale)}`);
  return md;
}

function coverageSection(cov: Coverage, index: HintIndex): string {
  const fn = code(cov.fn);
  const lines: string[] = [];

  switch (cov.status) {
    case 'tested': {
      const n = cov.tests.length;
      lines.push(`$(beaker) **${fn} changed** — covered by ${n} ${n === 1 ? 'test' : 'tests'}`);
      lines.push('');
      lines.push(...testList(cov.tests));
      if (cov.testsCapped) lines.push('', '_The covering-test list is capped at 25 by the analyser._');
      const command = index.commands[0];
      if (command) {
        lines.push('', `[Copy test command](command:ccc.copyTestCommand) · [Refresh](command:ccc.refresh)`);
      }
      break;
    }
    case 'test-code': {
      lines.push(`$(circle-outline) **${fn} changed** — this is test code`);
      lines.push('');
      lines.push('_Coverage is not reported for tests themselves._');
      break;
    }
    case 'untested': {
      // the recommended kind is named once in the headline - it is one fact, not two
      const phrase = missingTestPhrase(cov.target?.kind);
      lines.push(
        cov.fromTargetsOnly
          ? `$(warning) **${fn} has ${phrase} covering it**`
          : `$(warning) **${fn} changed — ${phrase} covers it**`,
      );
      lines.push('');
      const target = cov.target;
      if (target) {
        if (target.suggest) lines.push(`> ${prose(target.suggest)}`);
        const why = (target.why ?? [])
          .slice(0, 4)
          .map((w) => `${prose(String(w.factor))} (${prose(String(w.detail ?? w.value))})`);
        if (why.length > 0) lines.push('', `Why here: ${why.join(' · ')}`);
        const also = (target.also ?? []).filter((k) => k !== target.kind);
        if (also.length > 0) lines.push('', `Also consider: ${also.map((k) => code(k)).join(', ')}`);
      } else if (cov.addResolved === false) {
        lines.push(
          'The analyser has no ranked recommendation for this function — usually a test helper, ' +
            'or a function it does not consider worth a dedicated test.',
        );
      } else {
        lines.push('The analyser did not rank this function, so there is no suggested test kind.');
      }
      lines.push(
        '',
        [
          locationLink(`Open ${cov.file}:${cov.span[0]}`, cov.file, cov.span[0]),
          referencesLink('Find all references', cov.fn),
        ].join(' · '),
      );
      if (target) {
        lines.push('', `_Ranked by the analyser at priority ${escapeMd(String(target.priority))}._`);
      }
      break;
    }
  }

  if (cov.calledFromServices.length > 0) {
    lines.push(
      '',
      `_Also called from ${cov.calledFromServices.map((s) => code(s)).join(', ')} ` +
        '(matched by name; no call site resolved)._',
    );
  }
  if (cov.status !== 'test-code' && index.notes.triggers) {
    lines.push('', `_${prose(index.notes.triggers)}_`);
  }
  return lines.join('\n');
}

function testList(tests: TestLink[]): string[] {
  if (tests.length === 0) return ['_No linkable test was reported._'];
  const ambiguous = new Set(tests.filter((t) => t.confidence === 'ambiguous').map((t) => t.name));
  const out: string[] = [];
  for (const name of ambiguous) {
    out.push(`⚠ more than one test is named ${code(name)} — every candidate is listed:`);
  }
  for (const test of tests) {
    if (test.file && test.line !== undefined) {
      const where = `${test.file}:${test.line}`;
      const hops =
        test.distance === undefined
          ? ''
          : test.distance === 0
            ? ' — direct'
            : ` — ${test.distance} call ${test.distance === 1 ? 'hop' : 'hops'} away`;
      out.push(
        `- ${locationLink(test.name, test.file, test.line, true)} \`${where}\`${hops}${evidenceNote(test.evidence)}`,
      );
    } else {
      out.push(`- ${code(test.name)} — _not in the trigger set, so no location was recorded_`);
    }
  }
  return out;
}

// only the weak tie is worth a word - the strong ones are what "covered by" already implies
function evidenceNote(evidence: string | undefined): string {
  return evidence === 'name-only' ? ' _(matched on the name alone)_' : '';
}

function outboundSection(refs: OutboundRef[], index: HintIndex): string {
  const noun = index.serviceMode === 'configured' ? 'service' : 'module';
  const names = [...new Set(refs.map((r) => r.toService))];
  const remote = refs.find((r) => r.remote)?.remote;

  // a call leaving the repository is a different fact from one crossing a directory
  if (remote && !remote.answered) {
    return [
      `$(question) **nothing serves ${code(remote.key)}**`,
      '',
      'An author wrote this `ccc:calls`, but no `ccc:serves` anywhere answers the key.',
      '',
      '- check the key is spelled identically at both ends',
      '- check the repository that serves it is listed under `externals` in `.ccc/map.json`',
    ].join('\n');
  }

  const lines: string[] = remote
    ? [
        `$(arrow-up) **calls ${code(names[0] ?? '')} in another repository**`,
        '',
        `${escapeMd(remote.transport)} · ${code(remote.key)}` +
          `${remote.repo ? ` · ${code(remote.repo)}` : ''}` +
          `${remote.language ? ` · ${escapeMd(remote.language)}` : ''}`,
        '',
      ]
    : [`$(arrow-right) **calls into ${noun} ${names.map((n) => code(n)).join(', ')}**`, ''];

  for (const ref of refs.slice(0, 12)) {
    const label = ref.kind === 'type' ? `${ref.symbol} (type)` : ref.symbol;
    const target =
      ref.targetFile && ref.targetLine !== undefined
        ? // a peer checkout has no URI here unless checked out locally, so the command resolves it
          ref.remote
          ? `[${escapeMd(`${ref.targetFunction ?? ref.symbol} — ${ref.targetFile}:${ref.targetLine}`)}](command:ccc.openCrossing?${encodeURIComponent(
              JSON.stringify([{ refs: [ref] }]),
            )})`
          : locationLink(`${ref.targetFile}:${ref.targetLine}`, ref.targetFile, ref.targetLine)
        : '_target location not resolved_';
    const evidence = ref.via ? ` _${VIA_PROSE[ref.via]}_` : '';
    lines.push(`- ${code(label)} → ${target}${evidence}`);
  }
  if (refs.length > 12) lines.push(`- _…and ${refs.length - 12} more_`);

  const declaredOnly = refs.filter((r) => r.declared && !r.detected);
  if (declaredOnly.length > 0) {
    lines.push(
      '',
      '_Declared in `.ccc/map.json` with no call site found — expected for HTTP, RPC and queue links._',
    );
  }
  if (refs.some((r) => r.source === 'services')) {
    lines.push(
      '',
      '_Marked on the calling function, not the exact call line: without a `services` block in ' +
        '`.ccc/map.json` the analyser reports the caller, not the call._',
    );
  }
  const first = refs[0];
  if (first) lines.push('', referencesLink(`Find all call sites of ${first.symbol}`, first.symbol));
  if (index.serviceMode !== 'configured') {
    lines.push(
      '',
      `_Boundaries inferred from ${escapeMd(index.serviceSource)} — add a \`services\` block to ` +
        '`.ccc/map.json` to name them._',
    );
  }
  return lines.join('\n');
}

function inboundSection(refs: InboundRef[], index: HintIndex): string {
  const noun = index.serviceMode === 'configured' ? 'service' : 'module';
  const names = [...new Set(refs.map((r) => r.fromService))];
  const lines: string[] = [
    `$(arrow-left) **called by ${noun} ${names.map((n) => code(n)).join(', ')}**`,
    '',
  ];

  for (const ref of refs.slice(0, 12)) {
    if (ref.callerFn && ref.callerFile && ref.callerLine !== undefined) {
      lines.push(
        `- from ${locationLink(ref.callerFn, ref.callerFile, ref.callerLine, true)} ` +
          `\`${ref.callerFile}:${ref.callerLine}\` — calls ${code(ref.symbol)}`,
      );
    } else {
      lines.push(`- from ${code(ref.fromService)} — calls ${code(ref.symbol)}`);
    }
  }
  if (refs.length > 12) lines.push(`- _…and ${refs.length - 12} more_`);

  lines.push(
    '',
    '_One representative call site per symbol, and each link opens the calling function’s ' +
      'definition rather than the call itself. Use “find all references” for the rest._',
  );
  return lines.join('\n');
}

function hotSection(hot: Hot, index: HintIndex): string {
  const lead = hot.reasons[0];
  const isCycle = lead?.kind === 'cycle';
  const lines: string[] = [
    isCycle
      ? `$(sync) **${code(hot.fn)} is in a call cycle** of ${lead.value}`
      : `$(flame) **${code(hot.fn)} is a hot path**${lead?.rank ? ` — ${ordinal(lead.rank)} ${VIEW_NAME[lead.kind]}` : ''}`,
    '',
  ];

  for (const reason of hot.reasons) {
    switch (reason.kind) {
      case 'cycle': {
        const others = (reason.members ?? [])
          .slice(0, 8)
          .map((m) => locationLink(m.name, m.file, m.line, true));
        lines.push(
          `- calls round to itself through ${others.length > 0 ? others.join(', ') : 'the graph'}` +
            ` — a change here can come back to you`,
        );
        break;
      }
      case 'most_called':
        lines.push(
          `- **${reason.value} caller${reason.value === 1 ? '' : 's'}**` +
            `${hot.row ? ` across ${hot.row.call_sites} call sites` : ''}` +
            `${reason.rank ? ` (${ordinal(reason.rank)} most called)` : ''} — breaking it breaks a lot`,
        );
        break;
      case 'most_complex':
        lines.push(
          `- **complexity ${reason.value}**` +
            `${hot.row ? `, ${hot.row.lines} lines, loop depth ${hot.row.loop_depth}` : ''}` +
            `${reason.rank ? ` (${ordinal(reason.rank)} most complex)` : ''}`,
        );
        break;
      case 'widest':
        lines.push(
          `- **calls ${reason.value} other functions**${reason.rank ? ` (${ordinal(reason.rank)} widest)` : ''}` +
            ' — it coordinates rather than computes',
        );
        break;
      case 'deep_chain':
        lines.push(`- heads the **deepest call chain here**, ${reason.value} frames down`);
        break;
    }
  }
  if (hot.row?.recursive) lines.push('- recursive');

  lines.push('', referencesLink(`Find all references to ${hot.fn}`, hot.fn));
  if (index.notes.hot) lines.push('', `_${prose(index.notes.hot)}_`);
  return lines.join('\n');
}

const VIEW_NAME: Record<string, string> = {
  most_called: 'most called',
  most_complex: 'most complex',
  widest: 'widest fan-out',
  deep_chain: 'deepest chain',
  cycle: 'cycle',
};

function ordinal(n: number): string {
  const rest = n % 100;
  if (rest >= 11 && rest <= 13) return `${n}th`;
  return `${n}${['th', 'st', 'nd', 'rd'][n % 10] ?? 'th'}`;
}

// command links carry args as a JSON array - VSCode spreads it into the parameters
export function locationLink(label: string, file: string, line: number, asCode = false): string {
  const args = encodeURIComponent(JSON.stringify([{ file, line }]));
  // a code-span label needs no escaping and reads better than backslash-escaped underscores
  const text = asCode ? `\`${label.replace(/`/g, 'ˋ')}\`` : escapeMd(label);
  return `[${text}](command:ccc.openLocation?${args} "${escapeTitle(`${file}:${line}`)}")`;
}

export function referencesLink(label: string, symbol: string): string {
  const args = encodeURIComponent(JSON.stringify([{ symbol }]));
  return `[${escapeMd(label)}](command:ccc.showReferences?${args} "${escapeTitle(`references to ${symbol}`)}")`;
}

function footer(index: HintIndex, stale: boolean): string {
  const bits: string[] = [];
  if (index.base) {
    const sha = index.baseSha ? ` (${index.baseSha.slice(0, 7)})` : '';
    bits.push(`base ${escapeMd(index.base)}${sha}`);
  } else if (index.changes.available === false) {
    bits.push(`no change set: ${escapeMd(index.changes.reason)}`);
  }
  if (index.generated) bits.push(`analysed ${escapeMd(index.generated)}`);
  const warning = stale ? '⚠ this file has unsaved changes — hints reflect the last save. ' : '';
  return `_${warning}${bits.join(' · ')}_`;
}

function code(text: string): string {
  return `\`${text.replace(/`/g, 'ˋ')}\``;
}

// full escape for identifiers and paths we embed - punctuation that cannot break out is left alone
export function escapeMd(text: string): string {
  return text.replace(/[\\`*_[\]()<>|]/g, '\\$&');
}

// the analyser's prose is already markdown - pass it through, neutralising square and angle brackets
export function prose(text: string): string {
  return text.replace(/[[\]<>]/g, '\\$&');
}

function escapeTitle(text: string): string {
  return text.replace(/"/g, "'");
}

type FileFunc = FileStructure['funcs'][number];

// everything the provider resolves per document, in one round trip
export interface HoverSources {
  hints?: FileHints;
  structure?: FileStructure;
  index?: HintIndex;
  stale: boolean;
}

export type HoverLookup = (uri: vscode.Uri) => Promise<HoverSources | undefined>;

// a real provider fired only at the anchor - decoration hoverMessage never shows over injected content
export class CccHoverProvider implements vscode.HoverProvider {
  constructor(
    private readonly lookup: HoverLookup,
    private cfg: Cfg,
  ) {}

  updateConfig(cfg: Cfg): void {
    this.cfg = cfg;
  }

  async provideHover(
    document: vscode.TextDocument,
    position: vscode.Position,
    token: vscode.CancellationToken,
  ): Promise<vscode.Hover | undefined> {
    if (!this.cfg.enable) return undefined;
    if (document.uri.scheme !== 'file') return undefined;
    const found = await this.lookup(document.uri);
    if (!found || token.isCancellationRequested) return undefined;

    const parts: vscode.MarkdownString[] = [];
    let at: vscode.Position | undefined;

    // the circled numeral - same filters as its decoration, so the hover exists where a glyph does
    if (found.structure && this.cfg.complexity.enabled) {
      for (const fn of found.structure.funcs ?? []) {
        if (fn.line - 1 !== position.line) continue;
        const score = fn.complexity_score;
        if (typeof score !== 'number' || score < 1 || score > 10) continue;
        if (score < this.cfg.complexity.minScore) continue;
        const name = typeof fn.name === 'string' ? fn.name : '';
        if (name.length === 0) continue;
        const range = anchorToRange({ line: fn.line, startCol: fn.col, endCol: fn.col + name.length }, document);
        if (!range || !range.end.isEqual(position)) continue;
        parts.push(complexityHover(fn));
        at = range.end;
        break;
      }
    }

    // the hint badge, anchored on the same boundary
    if (found.hints && found.index) {
      const hint = found.hints.lines.get(position.line + 1);
      if (hint) {
        const range = anchorToRange(hint.anchor, document);
        if (range?.end.isEqual(position)) {
          parts.push(buildHover(hint, found.index, found.stale));
          at ??= range.end;
        }
      }
    }

    if (parts.length === 0 || !at) return undefined;
    return new vscode.Hover(parts, new vscode.Range(at, at));
  }
}

// the pop-out behind the circled numeral - the verdict, then the raw counts so the score is checkable
function complexityHover(fn: FileFunc): vscode.MarkdownString {
  const score = fn.complexity_score ?? 0;
  const md = new vscode.MarkdownString();
  md.supportThemeIcons = true;
  md.appendMarkdown(`**Complexity ${score}/10** - _${SCORE_DESCRIPTION[score] ?? ''}_`);
  const parts: string[] = [];
  if (typeof fn.complexity === 'number') parts.push(`${fn.complexity} independent path(s)`);
  if (typeof fn.branches === 'number' && fn.branches > 0) {
    parts.push(`${fn.branches} branch${fn.branches === 1 ? '' : 'es'}`);
  }
  if (typeof fn.loop_depth === 'number' && fn.loop_depth > 0) {
    parts.push(`${fn.loop_depth} nested loop level(s)`);
  }
  if (typeof fn.body_lines === 'number' && fn.body_lines > 0) parts.push(`${fn.body_lines} lines`);
  if (parts.length > 0) md.appendMarkdown(`\n\nWhy: ${parts.join(' · ')}`);
  md.appendMarkdown('\n\n_Cyclomatic-style: one path, plus one per decision point and loop._');
  return md;
}

export type { ServiceMode };

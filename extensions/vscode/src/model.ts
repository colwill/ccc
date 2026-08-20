// turns one `/insights.json` payload into per-file line hints - no vscode imports, so no extension host

import * as path from 'node:path';
import type { Cfg } from './config';
import { isTestPath, keyOfPath } from './pathkeys';
import {
  arr,
  type Arity,
  bool,
  type ChangedFunction,
  type ChangesEdge,
  type ComplexityRow,
  type ChangesSection,
  type Crossing,
  type ExternalRepo,
  type HotChain,
  type HotCycle,
  type HotRow,
  type HotSection,
  isUnavailable,
  type InsightsPayload,
  lineSpan,
  num,
  type ServicesEdge,
  type ServicesSection,
  str,
  type TestCommand,
  type TestKind,
  type TestedBySite,
  type TestRun,
  type TestTarget,
  type TriggerAdd,
  type TriggersSection,
  type Unavailable,
  VIA_RANK,
  type Via,
} from './types';

export type HintKind = 'untested' | 'outbound' | 'inbound' | 'tested' | 'cycle' | 'hot' | 'test-code';

// one decoration per line - two would fight over the same badge slot and gutter
const PRECEDENCE: HintKind[] = [
  'untested',
  'outbound',
  'inbound',
  'tested',
  'cycle',
  'hot',
  'test-code',
];

// how service boundaries were derived - decides whether reqs 3/4 mean anything
export type ServiceMode = 'configured' | 'derived' | 'per-file';

// all 1-based, as the analyser reports them
export interface Anchor {
  line: number;
  startCol?: number;
  endCol?: number;
}

export type TestConfidence = 'exact' | 'by-name' | 'ambiguous' | 'unlocated';

export interface TestLink {
  name: string;
  file?: string;
  line?: number;
  language?: string;
  distance?: number;
  reason?: string;
  confidence: TestConfidence;
  // how the analyser tied this test to the function - absent on legacy payloads
  evidence?: string;
}

export type CoverageStatus = 'tested' | 'untested' | 'test-code';

export interface Coverage {
  fn: string;
  file: string;
  span: [number, number];
  status: CoverageStatus;
  tests: TestLink[];
  // true when tested_by hit its 25-entry cap
  testsCapped: boolean;
  // the ranked recommendation, when untested and ranked
  target?: TestTarget;
  // whether test_triggers.add listed it as a gap that test_targets ranked
  addResolved?: boolean;
  // services that call this function, by name match only
  calledFromServices: string[];
  services: string[];
  // set when this came from test_targets rather than the diff
  fromTargetsOnly?: boolean;
}

// how a missing test is named everywhere - the kind sits inside the phrase, not beside it
export function missingTestPhrase(kind?: TestKind): string {
  return kind ? `no ${kind.replace(/-test$/, '')} test` : 'no test';
}

// the same gap phrased as the fix, for surfaces that already carry a `⚠`
export function addTestPhrase(kind?: TestKind): string {
  return kind ? `Add ${kind.replace(/-test$/, '')} test` : 'Add test';
}

// each complexity band as a risk verdict - shared so one score reads the same everywhere
export const SCORE_DESCRIPTION: Record<number, string> = {
  1: 'good',
  2: 'low risk',
  3: 'low risk',
  4: 'medium risk',
  5: 'medium risk',
  6: 'moderate risk',
  7: 'moderate risk',
  8: 'high risk',
  9: 'high risk',
  10: 'highest risk',
};

export interface OutboundRef {
  toService: string;
  symbol: string;
  kind: 'call' | 'type';
  via?: Via;
  declared: boolean;
  detected: boolean;
  // filled by joining against services.edges[].sites[]
  targetFile?: string;
  targetLine?: number;
  targetFunction?: string;
  source: 'changes' | 'services' | 'crossing';
  // set when the call leaves this repo - targetFile/targetLine point into the peer's checkout
  remote?: {
    key: string;
    transport: string;
    repo?: string;
    language?: string;
    // false once nothing anywhere serves the key
    answered: boolean;
  };
}

export interface InboundRef {
  fromService: string;
  symbol: string;
  callerFn?: string;
  callerFile?: string;
  // the caller's DEFINITION line, not the line of the call
  callerLine?: number;
  declared: boolean;
  source: 'services' | 'crossing';
  // the caller is in another repository
  remote?: { key: string; transport: string; repo?: string; language?: string };
}

// why a function counts as structurally hot, strongest signal first
export type HotReasonKind = 'cycle' | 'most_called' | 'most_complex' | 'widest' | 'deep_chain';

export interface HotReason {
  kind: HotReasonKind;
  // 1-based position in the analyser's ranking, where it ranks
  rank?: number;
  // the headline number: callers, complexity, fan-out, chain depth or cycle size
  value: number;
  // other members of the cycle, when kind is 'cycle'
  members?: { name: string; file: string; line: number }[];
}

export interface Hot {
  fn: string;
  file: string;
  reasons: HotReason[];
  row?: HotRow;
}

export interface LineHint {
  // 1-based
  line: number;
  anchor: Anchor;
  kinds: Set<HintKind>;
  primary: HintKind;
  badge: string;
  coverage?: Coverage;
  outbound: OutboundRef[];
  inbound: InboundRef[];
  hot?: Hot;
  // true once the anchor has been narrowed to the function's name token
  refined: boolean;
}

export interface FileHints {
  rel: string;
  // absolute path, original casing, for display and opening
  abs: string;
  lines: Map<number, LineHint>;
}

export interface HintIndex {
  generated: string;
  base?: string;
  baseSha?: string;
  serviceMode: ServiceMode;
  serviceSource: string;
  crossServiceEnabled: boolean;
  changes: { available: true } | Unavailable;
  triggers: { available: true } | Unavailable;
  files: Map<string, FileHints>;
  // rel path -> function name -> coverage, for the enclosing-function join
  coverageByFile: Map<string, Map<string, Coverage>>;
  counts: { changed: number; tested: number; untested: number; outbound: number; inbound: number; hot: number };
  commands: TestCommand[];
  // peer repositories from `map.json` `externals`, by name
  externals: Map<string, ExternalRepo>;
  // the tests these changes make necessary, straight from the analyser
  triggerRun: TestRun[];
  // changed functions no test covers
  triggerGaps: TriggerAdd[];
  targetsById: Map<string, TestTarget>;
  triggerCounts: { run: number; gaps: number };
  // every measured function, ranked by complexity - the Complexity view's whole input
  complexity: ComplexityRow[];
  // measured before the analyser's cap, so a filtered list can admit what it cannot see
  complexityTotal: number;
  complexityTruncated: boolean;
  // the analyser's own caveats, quoted rather than paraphrased in hovers
  notes: { triggers?: string; targets?: string; hot?: string };
}

export interface BuildContext {
  // absolute fsPath of the workspace folder
  rootPath: string;
  cfg: Cfg;
}

export function buildHintIndex(payload: InsightsPayload, ctx: BuildContext): HintIndex {
  const { cfg } = ctx;
  const changes = readChanges(payload);
  const triggers = readTriggers(payload);
  const services = readServices(payload);
  const targets = arr<TestTarget>(payload.test_targets?.targets);

  const serviceSource = str(services?.source);
  const serviceMode = modeOf(serviceSource);
  const crossServiceEnabled =
    cfg.hints.crossServiceMode === 'always'
      ? true
      : cfg.hints.crossServiceMode === 'off'
        ? false
        : serviceMode !== 'per-file';

  const index: HintIndex = {
    generated: str(payload.generated),
    serviceMode,
    serviceSource,
    crossServiceEnabled,
    changes: changes ? { available: true } : unavailableOf(payload.changes),
    triggers: triggers ? { available: true } : unavailableOf(payload.test_triggers),
    files: new Map(),
    coverageByFile: new Map(),
    counts: { changed: 0, tested: 0, untested: 0, outbound: 0, inbound: 0, hot: 0 },
    commands: arr<TestCommand>(triggers?.commands),
    externals: new Map(
      arr<ExternalRepo>(services?.externals).map((e) => [str(e.name), e]),
    ),
    triggerRun: arr<TestRun>(triggers?.run),
    triggerGaps: arr<TriggerAdd>(triggers?.add),
    targetsById: new Map(targets.map((t) => [str(t.id), t])),
    triggerCounts: {
      run: arr<TestRun>(triggers?.run).length,
      gaps: arr<TriggerAdd>(triggers?.add).length,
    },
    complexity: arr<ComplexityRow>(payload.complexity?.functions).map(normaliseComplexityRow),
    complexityTotal: num(payload.complexity?.total),
    complexityTruncated: payload.complexity?.truncated === true,
    notes: { triggers: triggers?.note, targets: payload.test_targets?.note, hot: payload.hot?.note },
  };
  if (changes) {
    index.base = str(changes.base) || undefined;
    index.baseSha = str(changes.base_sha) || undefined;
  }

  const runByName = groupBy(arr<TestRun>(triggers?.run), (r) => str(r.test));
  // a test is one (file, name), which is what `tested_by_sites` addresses
  const runByIdentity = new Map(
    arr<TestRun>(triggers?.run).map((r) => [`${str(r.file)}::${str(r.test)}`, r]),
  );
  const addByTarget = new Map(arr<TriggerAdd>(triggers?.add).map((a) => [str(a.target), a]));
  const targetsById = new Map(targets.map((t) => [str(t.id), t]));
  const selfTests = new Set(arr<TestRun>(triggers?.run).map((r) => `${str(r.file)}::${str(r.test)}`));

  if (changes && (cfg.hints.testTriggers || cfg.hints.untested)) {
    for (const raw of arr<ChangedFunction>(changes.changed_functions)) {
      const cf = normaliseChangedFunction(raw);
      if (!cf) continue;
      index.counts.changed += 1;

      const status = classify(cf, selfTests);
      if (status === 'test-code' && !cfg.hints.includeTestFiles) continue;
      if (status === 'untested' && !cfg.hints.untested) continue;
      if (status !== 'untested' && !cfg.hints.testTriggers) continue;

      const id = targetId(cf.file, cf.function);
      const coverage: Coverage = {
        fn: cf.function,
        file: cf.file,
        span: cf.lines,
        status,
        tests: linkTests(cf, runByName, runByIdentity),
        testsCapped: cf.tested_by.length >= 25,
        calledFromServices: cf.called_from,
        services: cf.services,
      };
      if (status === 'untested') {
        const add = addByTarget.get(id);
        coverage.addResolved = add ? bool(add.resolved) : undefined;
        coverage.target = targetsById.get(id);
        index.counts.untested += 1;
      } else if (status === 'tested') {
        index.counts.tested += 1;
      }

      rememberCoverage(index, cf.file, cf.function, coverage);
      const hint = hintAt(index, ctx, cf.file, cf.lines[0]);
      if (!hint) continue;
      hint.coverage = coverage;
      hint.kinds.add(status);
    }
  }

  // unchanged-but-uncovered functions, only when asked - the fallback when no git base resolves
  if (cfg.hints.untested && cfg.untested.showUncoveredTargets) {
    for (const t of targets) {
      if (bool(t.covered)) continue;
      if (num(t.priority) < cfg.untested.minPriority) continue;
      const file = str(t.file);
      const line = num(t.line);
      if (file.length === 0 || line < 1) continue;
      if (index.coverageByFile.get(file)?.has(str(t.function))) continue;
      if (!cfg.hints.includeTestFiles && isTestPath(file)) continue;

      const coverage: Coverage = {
        fn: str(t.function),
        file,
        span: [line, line],
        status: 'untested',
        tests: [],
        testsCapped: false,
        target: t,
        calledFromServices: [],
        services: str(t.service) ? [str(t.service)] : [],
        fromTargetsOnly: true,
      };
      rememberCoverage(index, file, coverage.fn, coverage);
      const hint = hintAt(index, ctx, file, line);
      if (!hint) continue;
      hint.coverage = coverage;
      hint.kinds.add('untested');
      index.counts.untested += 1;
    }
  }

  // cross-service calls
  if (crossServiceEnabled && services) {
    const siteIndex = buildSiteIndex(services);

    if (cfg.hints.outbound) {
      // `changes.edges` is authoritative for the call SITE, but needs .ccc/map.json services
      for (const edge of arr<ChangesEdge>(changes?.edges)) {
        const from = str(edge.from);
        const to = str(edge.to);
        if (to.length === 0 || from === to) continue;
        for (const sym of arr<ChangesEdge['symbols'][number]>(edge.symbols)) {
          const via = str(sym.via) as Via;
          if (cfg.hints.minEvidence === 'evidence' && via === 'name-only') continue;
          const file = str(sym.file);
          const line = num(sym.line);
          if (file.length === 0 || line < 1) continue;
          const site = siteIndex.get(siteKey(from, to, str(sym.symbol)));
          addOutbound(index, ctx, file, line, {
            toService: to,
            symbol: str(sym.symbol),
            kind: str(sym.kind) === 'type' ? 'type' : 'call',
            via: via in VIA_RANK ? via : undefined,
            declared: bool(edge.declared),
            detected: bool(edge.detected),
            targetFile: site ? str(site.target_file) : undefined,
            targetLine: site ? num(site.target_line) : undefined,
            source: 'changes',
          });
        }
      }

      // fallback with no map.json - only the caller's line, so hints are per function
      if (arr(changes?.edges).length === 0) {
        for (const edge of arr<ServicesEdge>(services.edges)) {
          const from = str(edge.from);
          const to = str(edge.to);
          if (to.length === 0 || from === to) continue;
          for (const site of arr<ServicesEdge['sites'][number]>(edge.sites)) {
            if (bool(site.external) || str(site.via) === 'annotation') continue;
            const file = str(site.caller_file);
            const line = num(site.caller_line);
            if (file.length === 0 || line < 1) continue;
            addOutbound(index, ctx, file, line, {
              toService: to,
              symbol: str(site.symbol),
              kind: 'call',
              declared: bool(edge.declared),
              detected: bool(edge.detected),
              targetFile: str(site.target_file) || undefined,
              targetLine: num(site.target_line) || undefined,
              source: 'services',
            });
          }
        }
      }
    }

    // boundary crossings first - an author stated these and they can end in another repository
    const externalNames = new Set(arr<string>(services.external_names));
    const repoOf = new Map(
      arr<ExternalRepo>(services.externals).map((e) => [str(e.name), e]),
    );
    for (const raw of arr<Crossing>(services.crossings)) {
      const key = str(raw.key);
      const file = str(raw.file);
      const line = num(raw.line);
      if (key.length === 0 || file.length === 0 || line < 1) continue;
      const isExternal = bool(raw.external) || externalNames.has(str(raw.to));
      const peer = repoOf.get(str(raw.to)) ?? repoOf.get(str(raw.from));
      const remote = {
        key,
        transport: str(raw.transport, 'unspecified'),
        repo: peer ? str(peer.repo) || undefined : undefined,
        language: peer ? str(peer.language) || undefined : undefined,
        answered: raw.remote != null,
      };

      // `from` empty means the call sits in a file no service glob claims
      const outbound = str(raw.to).length > 0 || raw.remote == null;
      if (cfg.hints.outbound && outbound) {
        addOutbound(index, ctx, file, line, {
          toService: str(raw.to) || '(unanswered)',
          symbol: key,
          kind: 'call',
          declared: false,
          detected: raw.remote != null,
          targetFile: raw.remote ? str(raw.remote.file) : undefined,
          targetLine: raw.remote ? num(raw.remote.line) : undefined,
          targetFunction: raw.remote ? str(raw.remote.function) : undefined,
          source: 'crossing',
          remote: isExternal || !remote.answered ? remote : undefined,
        });
      }
    }

    // a crossing's handler is here when a peer calls in, at the key's target when we call out
    if (cfg.hints.inbound) {
      for (const raw of arr<Crossing>(services.crossings)) {
        if (raw.remote == null) continue;
        const fromPeer = externalNames.has(str(raw.from));
        const peer = repoOf.get(fromPeer ? str(raw.from) : str(raw.to));
        const remoteMeta = {
          key: str(raw.key),
          transport: str(raw.transport, 'unspecified'),
          repo: peer ? str(peer.repo) || undefined : undefined,
          language: peer ? str(peer.language) || undefined : undefined,
        };

        if (fromPeer) {
          // the peer published that it calls a key we serve - the handler is here
          const file = str(raw.file);
          const line = num(raw.line);
          if (file.length === 0 || line < 1) continue;
          addInbound(index, ctx, file, line, {
            fromService: str(raw.from),
            symbol: str(raw.key),
            callerFn: str(raw.remote.function) || undefined,
            callerFile: str(raw.remote.file) || undefined,
            callerLine: num(raw.remote.line) || undefined,
            declared: false,
            source: 'crossing',
            remote: remoteMeta,
          });
          continue;
        }

        // we call out: mark the handler only when it is in this repo - a peer's file has no line here
        if (bool(raw.external) || externalNames.has(str(raw.to))) continue;
        const file = str(raw.remote.file);
        const line = num(raw.remote.line);
        if (file.length === 0 || line < 1) continue;
        addInbound(index, ctx, file, line, {
          fromService: str(raw.from),
          symbol: str(raw.key),
          callerFn: str(raw.function) || undefined,
          callerFile: str(raw.file) || undefined,
          callerLine: num(raw.line) || undefined,
          declared: false,
          source: 'crossing',
        });
      }
    }

    if (cfg.hints.inbound) {
      for (const edge of arr<ServicesEdge>(services.edges)) {
        const from = str(edge.from);
        const to = str(edge.to);
        if (from.length === 0 || from === to) continue;
        for (const site of arr<ServicesEdge['sites'][number]>(edge.sites)) {
          // crossings are handled above, and their target may be a file that does not exist here
          if (bool(site.external) || str(site.via) === 'annotation') continue;
          const file = str(site.target_file);
          const line = num(site.target_line);
          if (file.length === 0 || line < 1) continue;
          addInbound(index, ctx, file, line, {
            fromService: from,
            symbol: str(site.symbol),
            callerFn: str(site.caller) || undefined,
            callerFile: str(site.caller_file) || undefined,
            callerLine: num(site.caller_line) || undefined,
            declared: bool(edge.declared),
            source: 'services',
          });
        }
      }
    }
  }

  // hot paths
  if (cfg.hints.hotPaths && payload.hot) {
    for (const hot of collectHot(payload.hot, cfg)) {
      const hint = hintAt(index, ctx, hot.file, hot.line);
      if (!hint) continue;
      hint.hot = hot.facts;
      hint.kinds.add(hot.facts.reasons[0]?.kind === 'cycle' ? 'cycle' : 'hot');
      index.counts.hot += 1;
    }
  }

  finalise(index, cfg);
  return index;
}

// fold the five `hot` views into one entry per function - the strongest reason drives the badge
function collectHot(hot: HotSection, cfg: Cfg): { file: string; line: number; facts: Hot }[] {
  const byKey = new Map<string, { file: string; line: number; facts: Hot }>();

  const entry = (name: string, file: string, line: number, row?: HotRow) => {
    if (name.length === 0 || file.length === 0 || line < 1) return undefined;
    const key = `${file}::${name}`;
    let found = byKey.get(key);
    if (!found) {
      found = { file, line, facts: { fn: name, file, reasons: [] } };
      byKey.set(key, found);
    }
    if (row && !found.facts.row) found.facts.row = row;
    return found;
  };

  const views: [HotReasonKind, HotRow[], (r: HotRow) => number][] = [
    ['most_called', arr<HotRow>(hot.most_called), (r) => num(r.callers)],
    ['most_complex', arr<HotRow>(hot.most_complex), (r) => num(r.complexity)],
    ['widest', arr<HotRow>(hot.widest), (r) => num(r.calls)],
  ];
  for (const [kind, rows, value] of views) {
    rows.forEach((raw, i) => {
      const row = normaliseHotRow(raw);
      if (!row) return;
      // the analyser's call-graph verdict beats any path heuristic - it sees an inline `mod tests`
      if (row.test && !cfg.hints.includeTestFiles) return;
      if (isTestPath(row.file) && !cfg.hints.includeTestFiles) return;
      const found = entry(row.name, row.file, row.line, row);
      found?.facts.reasons.push({ kind, rank: i + 1, value: value(row) });
    });
  }

  // only the head of a deep chain is marked - painting every node would light up half the file
  arr<HotChain>(hot.deepest_chains).forEach((chain) => {
    const head = arr<{ name: string; file: string; line: number }>(chain.chain)[0];
    if (!head) return;
    const file = str(head.file);
    if (isTestPath(file) && !cfg.hints.includeTestFiles) return;
    const found = entry(str(head.name), file, num(head.line));
    found?.facts.reasons.push({ kind: 'deep_chain', value: num(chain.depth) });
  });

  for (const cycle of arr<HotCycle>(hot.cycles)) {
    const members = arr<{ name: string; file: string; line: number }>(cycle.members).map((m) => ({
      name: str(m.name),
      file: str(m.file),
      line: num(m.line),
    }));
    for (const member of members) {
      if (isTestPath(member.file) && !cfg.hints.includeTestFiles) continue;
      const found = entry(member.name, member.file, member.line);
      found?.facts.reasons.push({
        kind: 'cycle',
        value: num(cycle.size, members.length),
        members: members.filter((m) => m.name !== member.name || m.file !== member.file),
      });
    }
  }

  for (const found of byKey.values()) {
    found.facts.reasons.sort((a, b) => HOT_RANK[a.kind] - HOT_RANK[b.kind] || (a.rank ?? 99) - (b.rank ?? 99));
  }
  return [...byKey.values()].filter((e) => e.facts.reasons.length > 0);
}

// strongest signal first - drives which reason the badge shows
const HOT_RANK: Record<HotReasonKind, number> = {
  cycle: 0,
  most_called: 1,
  most_complex: 2,
  widest: 3,
  deep_chain: 4,
};

function normaliseHotRow(raw: HotRow): HotRow | undefined {
  const name = str(raw?.name);
  const file = str(raw?.file);
  const line = num(raw?.line);
  if (name.length === 0 || file.length === 0 || line < 1) return undefined;
  return {
    name,
    file,
    line,
    callers: num(raw.callers),
    call_sites: num(raw.call_sites),
    calls: num(raw.calls),
    lines: num(raw.lines),
    complexity: num(raw.complexity),
    loop_depth: num(raw.loop_depth),
    recursive: bool(raw.recursive),
    language: str(raw.language),
    test: bool(raw.test),
  };
}

// classifier - three states, not a boolean - `tested` with an empty `tested_by` means the function is test code
function classify(cf: ChangedFunction, selfTests: Set<string>): CoverageStatus {
  if (isTestPath(cf.file) || selfTests.has(`${cf.file}::${cf.function}`)) return 'test-code';
  if (!cf.tested) return 'untested';
  return cf.tested_by.length > 0 ? 'tested' : 'test-code';
}

// join changed function -> covering tests - `tested_by_sites` is exact, bare names collide
function linkTests(
  cf: ChangedFunction,
  runByName: Map<string, TestRun[]>,
  runByIdentity: Map<string, TestRun>,
): TestLink[] {
  const sites = arr<TestedBySite>(cf.tested_by_sites);
  if (sites.length > 0) {
    const links = sites.map((site): TestLink => {
      const run = runByIdentity.get(`${str(site.file)}::${str(site.test)}`);
      return {
        name: str(site.test),
        file: str(site.file) || undefined,
        line: num(site.line) || undefined,
        language: str(site.language) || undefined,
        distance: run && num(run.distance, -1) >= 0 ? num(run.distance) : undefined,
        reason: run ? str(run.reason) || undefined : undefined,
        confidence: 'exact',
        evidence: str(site.evidence) || undefined,
      };
    });
    links.sort((a, b) => (a.distance ?? 99) - (b.distance ?? 99) || a.name.localeCompare(b.name));
    return links;
  }

  const links: TestLink[] = [];
  for (const name of cf.tested_by) {
    const all = runByName.get(name) ?? [];
    if (all.length === 0) {
      links.push({ name, confidence: 'unlocated' });
      continue;
    }
    // `covers` is capped at 8, so absence is not disproof - keep every same-named candidate
    const narrowed = all.filter((r) => arr<string>(r.covers).includes(cf.function));
    const pick = narrowed.length > 0 ? narrowed : all;
    const confidence: TestConfidence =
      pick.length > 1 ? 'ambiguous' : narrowed.length > 0 ? 'exact' : 'by-name';
    for (const run of pick) {
      links.push({
        name,
        file: str(run.file) || undefined,
        line: num(run.line) || undefined,
        language: str(run.language) || undefined,
        distance: num(run.distance, -1) >= 0 ? num(run.distance) : undefined,
        reason: str(run.reason) || undefined,
        confidence,
      });
    }
  }
  links.sort((a, b) => (a.distance ?? 99) - (b.distance ?? 99) || a.name.localeCompare(b.name));
  return links;
}

function hintAt(index: HintIndex, ctx: BuildContext, rel: string, line: number): LineHint | undefined {
  if (line < 1) return undefined;
  const abs = joinPath(ctx.rootPath, rel);
  const key = keyOfPath(abs);
  let file = index.files.get(key);
  if (!file) {
    file = { rel, abs, lines: new Map() };
    index.files.set(key, file);
  }
  let hint = file.lines.get(line);
  if (!hint) {
    hint = {
      line,
      anchor: { line },
      kinds: new Set(),
      primary: 'tested',
      badge: '',
      outbound: [],
      inbound: [],
      refined: false,
    };
    file.lines.set(line, hint);
  }
  return hint;
}

function addOutbound(
  index: HintIndex,
  ctx: BuildContext,
  rel: string,
  line: number,
  ref: OutboundRef,
): void {
  const hint = hintAt(index, ctx, rel, line);
  if (!hint) return;
  const existing = hint.outbound.find((o) => o.toService === ref.toService && o.symbol === ref.symbol);
  if (existing) {
    // the same call arrives twice by design - keep the strongest evidence and the known target
    if (rank(ref.via) < rank(existing.via)) existing.via = ref.via;
    existing.targetFile ??= ref.targetFile;
    existing.targetLine ??= ref.targetLine;
    existing.targetFunction ??= ref.targetFunction;
    existing.remote ??= ref.remote;
    existing.declared ||= ref.declared;
    existing.detected ||= ref.detected;
    if (ref.source === 'crossing') existing.source = 'crossing';
    return;
  }
  hint.outbound.push(ref);
  hint.kinds.add('outbound');
  index.counts.outbound += 1;
}

function addInbound(index: HintIndex, ctx: BuildContext, rel: string, line: number, ref: InboundRef): void {
  const hint = hintAt(index, ctx, rel, line);
  if (!hint) return;
  const existing = hint.inbound.find(
    (i) => i.fromService === ref.fromService && i.symbol === ref.symbol && i.callerFn === ref.callerFn,
  );
  if (existing) {
    existing.declared ||= ref.declared;
    return;
  }
  hint.inbound.push(ref);
  hint.kinds.add('inbound');
  index.counts.inbound += 1;
}

function rememberCoverage(index: HintIndex, rel: string, fn: string, coverage: Coverage): void {
  let byName = index.coverageByFile.get(rel);
  if (!byName) {
    byName = new Map();
    index.coverageByFile.set(rel, byName);
  }
  byName.set(fn, coverage);
}

// drop empty hints, then pick each line's primary category and badge text
function finalise(index: HintIndex, cfg: Cfg): void {
  for (const [key, file] of index.files) {
    for (const [line, hint] of file.lines) {
      if (hint.kinds.size === 0) {
        file.lines.delete(line);
        continue;
      }
      hint.primary = PRECEDENCE.find((k) => hint.kinds.has(k)) ?? 'tested';
      hint.badge = badgeFor(hint, cfg.decorations.badgeMaxLength, index.serviceMode);
    }
    if (file.lines.size === 0) index.files.delete(key);
  }
}

// segments in a fixed order, truncated from the right - a clipped badge leads with what matters
function badgeFor(hint: LineHint, maxLength: number, mode: ServiceMode): string {
  const segments: string[] = [];
  const cov = hint.coverage;
  if (cov) {
    if (cov.status === 'untested')
      segments.push(`${cov.fromTargetsOnly ? '◇' : '✗'} ${missingTestPhrase(cov.target?.kind)}`);
    else if (cov.status === 'tested')
      segments.push(`✓ ${cov.tests.length} ${cov.tests.length === 1 ? 'test' : 'tests'}`);
    else segments.push('⌾ test');
  }
  if (hint.outbound.length > 0) {
    const names = unique(hint.outbound.map((o) => shortService(o.toService, mode)));
    segments.push(`→ ${names[0]}${names.length > 1 ? ` +${names.length - 1}` : ''}`);
  }
  if (hint.inbound.length > 0) {
    const names = unique(hint.inbound.map((i) => shortService(i.fromService, mode)));
    segments.push(`← ${names[0]}${names.length > 1 ? ` +${names.length - 1}` : ''}`);
  }
  const reason = hint.hot?.reasons[0];
  if (reason) segments.push(hotBadge(reason));
  const text = segments.join(' · ');
  if (text.length <= maxLength) return text;
  return `${text.slice(0, Math.max(1, maxLength - 1))}…`;
}

// one segment for the strongest reason - glyphs not emoji, which render at unpredictable widths
function hotBadge(reason: HotReason): string {
  switch (reason.kind) {
    case 'cycle':
      return `↻ cycle of ${reason.value}`;
    case 'most_called':
      return `▲ ${reason.value} caller${reason.value === 1 ? '' : 's'}`;
    case 'most_complex':
      return `▲ complexity ${reason.value}`;
    case 'widest':
      return `▲ calls ${reason.value}`;
    case 'deep_chain':
      return `▲ ${reason.value} deep`;
  }
}

// a derived service is a directory and a per-file one is a path - a badge cannot show either whole
function shortService(name: string, mode: ServiceMode): string {
  if (mode === 'configured') return name;
  const parts = name.split('/');
  return parts[parts.length - 1] ?? name;
}

// narrow anchors from the definition row to the name token - pure polish, a 404 leaves them put
export function refineAnchors(
  hints: FileHints,
  structure: { funcs: { line: number; col: number; name: string; span: [number, number] }[] },
  cfg: Cfg,
  mode: ServiceMode,
): void {
  const funcs = arr<{ line: number; col: number; name: string; span: [number, number] }>(structure.funcs);
  if (funcs.length === 0) return;

  // collect the moves and apply after the walk - re-keying mid-walk would disturb the iteration
  const moves: Array<[number, LineHint]> = [];
  for (const hint of hints.lines.values()) {
    if (hint.refined) continue;
    const wanted = hint.coverage?.fn;
    const match = funcs.find((f) => {
      const span = lineSpan(f.span);
      if (!span) return false;
      if (hint.line < span[0] || hint.line > span[1]) return false;
      return wanted === undefined ? span[0] === hint.line : f.name === wanted;
    });
    if (!match) continue;
    const line = num(match.line, hint.line);
    const col = num(match.col, 1);
    hint.anchor = { line, startCol: col, endCol: col + match.name.length };
    hint.refined = true;
    if (hint.line !== line) moves.push([line, hint]);
  }

  for (const [line, hint] of moves) {
    const existing = hints.lines.get(line);
    if (existing && existing !== hint) {
      // both hints are the same function anchored differently - fold them rather than show it twice
      mergeInto(existing, hint, cfg, mode);
      hints.lines.delete(hint.line);
      continue;
    }
    hints.lines.delete(hint.line);
    hint.line = line;
    hints.lines.set(line, hint);
  }
}

// fold `from` into `into`, then rebuild the parts that depend on both
function mergeInto(into: LineHint, from: LineHint, cfg: Cfg, mode: ServiceMode): void {
  into.coverage ??= from.coverage;
  into.hot ??= from.hot;
  for (const ref of from.outbound) {
    if (!into.outbound.some((o) => o.toService === ref.toService && o.symbol === ref.symbol)) {
      into.outbound.push(ref);
    }
  }
  for (const ref of from.inbound) {
    if (!into.inbound.some((i) => i.fromService === ref.fromService && i.symbol === ref.symbol)) {
      into.inbound.push(ref);
    }
  }
  for (const kind of from.kinds) into.kinds.add(kind);
  into.primary = PRECEDENCE.find((k) => into.kinds.has(k)) ?? into.primary;
  into.badge = badgeFor(into, cfg.decorations.badgeMaxLength, mode);
}

// the innermost function containing a line - among containing spans, the latest start wins
export function enclosingFunction<T extends { span: [number, number]; name: string }>(
  funcs: T[],
  line: number,
): T | undefined {
  let best: T | undefined;
  for (const f of funcs) {
    const span = lineSpan(f.span);
    if (!span) continue;
    if (line < span[0] || line > span[1]) continue;
    if (!best || span[0] > (lineSpan(best.span)?.[0] ?? 0)) best = f;
  }
  return best;
}

export function targetId(file: string, fn: string): string {
  return `${file}::${fn}`;
}

function siteKey(from: string, to: string, symbol: string): string {
  return `${from}\u0000${to}\u0000${symbol}`;
}

function buildSiteIndex(services: ServicesSection): Map<string, ServicesEdge['sites'][number]> {
  const map = new Map<string, ServicesEdge['sites'][number]>();
  for (const edge of arr<ServicesEdge>(services.edges)) {
    for (const site of arr<ServicesEdge['sites'][number]>(edge.sites)) {
      map.set(siteKey(str(edge.from), str(edge.to), str(site.symbol)), site);
    }
  }
  return map;
}

function modeOf(source: string): ServiceMode {
  if (source.startsWith('.ccc/map.json')) return 'configured';
  if (source.startsWith('one unit per file')) return 'per-file';
  return 'derived';
}

function readChanges(payload: InsightsPayload): ChangesSection | undefined {
  const value = payload.changes;
  if (!value || isUnavailable(value)) return undefined;
  return value as ChangesSection;
}

function readTriggers(payload: InsightsPayload): TriggersSection | undefined {
  const value = payload.test_triggers;
  if (!value || isUnavailable(value)) return undefined;
  return value as TriggersSection;
}

function readServices(payload: InsightsPayload): ServicesSection | undefined {
  const value = payload.services;
  return value && Array.isArray(value.edges) ? value : undefined;
}

function unavailableOf(value: unknown): Unavailable {
  if (isUnavailable(value)) {
    return { available: false, reason: str(value.reason), hint: str(value.hint) };
  }
  return { available: false, reason: 'the analyser returned no data for this section', hint: '' };
}

function normaliseChangedFunction(raw: ChangedFunction): ChangedFunction | undefined {
  const file = str(raw.file);
  const fn = str(raw.function);
  const lines = lineSpan(raw.lines);
  if (file.length === 0 || fn.length === 0 || !lines) return undefined;
  return {
    services: arr<string>(raw.services).map((s) => str(s)),
    file,
    function: fn,
    lines,
    tested: bool(raw.tested),
    tested_by: arr<string>(raw.tested_by).map((s) => str(s)),
    tested_by_sites: arr<TestedBySite>(raw.tested_by_sites)
      .map((s) => ({
        test: str(s.test),
        file: str(s.file),
        line: num(s.line),
        language: str(s.language),
        evidence: str(s.evidence),
      }))
      .filter((s) => s.test.length > 0 && s.file.length > 0),
    called_from: arr<string>(raw.called_from).map((s) => str(s)),
  };
}

// the analyser's numbers clamped to what the view can draw - an out-of-range band picks no glyph
function normaliseComplexityRow(raw: ComplexityRow): ComplexityRow {
  const arity = str(raw.arity);
  return {
    id: str(raw.id),
    function: str(raw.function),
    file: str(raw.file),
    line: Math.max(1, num(raw.line, 1)),
    language: str(raw.language),
    service: raw.service === null || raw.service === undefined ? null : str(raw.service),
    complexity: Math.max(0, num(raw.complexity)),
    score: Math.min(10, Math.max(1, num(raw.score, 1))),
    params: Math.max(0, num(raw.params)),
    arity: ARITIES.includes(arity as Arity) ? (arity as Arity) : 'variadic',
    loop_depth: Math.max(0, num(raw.loop_depth)),
    lines: Math.max(0, num(raw.lines)),
    recursive: bool(raw.recursive),
    test: bool(raw.test),
  };
}

export const ARITIES: Arity[] = ['niladic', 'monadic', 'dyadic', 'variadic'];

function groupBy<T>(items: T[], key: (item: T) => string): Map<string, T[]> {
  const map = new Map<string, T[]>();
  for (const item of items) {
    const k = key(item);
    const list = map.get(k);
    if (list) list.push(item);
    else map.set(k, [item]);
  }
  return map;
}

function unique(values: string[]): string[] {
  return [...new Set(values)];
}

function rank(via: Via | undefined): number {
  return via ? VIA_RANK[via] : 99;
}

function joinPath(root: string, rel: string): string {
  return path.join(root, ...rel.split('/').filter((s) => s.length > 0));
}

export type { TestKind };

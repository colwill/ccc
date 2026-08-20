// raw ccc payload shapes mirroring the server - the massaging happens in model.ts

export interface Health {
  ok: boolean;
  files: number;
  // "YYYYMMDD-HH-MM-SS" one-second resolution - compare by equality only
  generated: string;
  watch_secs: number | null;
  version: string;
}

export interface RefreshResult {
  files_before: number;
  files_after: number;
  generated: string;
}

// `changes` and `test_triggers` degrade to this when git cannot resolve a base
export interface Unavailable {
  available: false;
  reason: string;
  hint: string;
}

export function isUnavailable(value: unknown): value is Unavailable {
  return isRecord(value) && value['available'] === false;
}

export type Via = 'receiver-type' | 'qualifier' | 'project' | 'import' | 'type-reference' | 'name-only';
export const VIA_RANK: Record<Via, number> = {
  'receiver-type': 0,
  qualifier: 1,
  project: 2,
  import: 3,
  'type-reference': 4,
  'name-only': 5,
};

export interface ChangedFunction {
  services: string[];
  // repo-relative, '/' separated
  file: string;
  function: string;
  lines: [number, number];
  tested: boolean;
  tested_by: string[];
  tested_by_sites?: TestedBySite[];
  called_from: string[];
}

export type Arity = 'niladic' | 'monadic' | 'dyadic' | 'variadic';
export interface ComplexityRow {
  // "<file>::<function>"
  id: string;
  function: string;
  file: string;
  line: number;
  language: string;
  service: string | null;
  // cyclomatic-style count
  complexity: number;
  score: number;
  params: number;
  arity: Arity;
  loop_depth: number;
  lines: number;
  recursive: boolean;
  test: boolean;
}

export interface ComplexitySection {
  functions: ComplexityRow[];
  total: number;
  truncated: boolean;
  note?: string;
}

export interface TestedBySite {
  test: string;
  file: string;
  line: number;
  language: string;
  // receiver-type | same-file | same-package | import | qualifier | name-only
  evidence: string;
}

export interface ChangedFile {
  path: string;
  status: string;
  services: string[];
  uncommitted: boolean;
}

export interface ChangesEdgeSymbol {
  symbol: string;
  // the caller's file
  file: string;
  // the actual call line
  line: number;
  via: Via;
  kind: 'call' | 'type';
}

export interface ChangesEdge {
  from: string;
  to: string;
  declared: boolean;
  detected: boolean;
  // objects, capped at 100 per edge
  symbols: ChangesEdgeSymbol[];
}

export interface ChangesSection {
  schema: string;
  root: string;
  base: string;
  base_sha: string;
  head_sha: string;
  services: string[];
  changed_files: ChangedFile[];
  changed_functions: ChangedFunction[];
  edges: ChangesEdge[];
  services_to_test: string[];
  untested: ChangedFunction[];
  unassigned_files: string[];
  counts: Record<string, number>;
}

export interface ServiceSite {
  symbol: string;
  // the callee's file and DEFINITION line
  target_file: string | null;
  target_line: number | null;
  caller: string;
  caller_file: string;
  caller_line: number;
  calls_on: { name: string; file: string; line: number; service: string | null }[];
  // set on annotated crossings only
  transport?: string;
  // the far side is a peer repository, not a service in this repo
  external?: boolean;
  target_function?: string | null;
  via?: string;
}

// a peer repository named in `.ccc/map.json` `externals`
export interface ExternalRepo {
  name: string;
  repo: string | null;
  language: string | null;
  // how it was reached: "path ../billing" or "surface https://…"
  source: string;
  resolved: boolean;
  error: string | null;
  generated: string | null;
  provides: number;
  consumes: number;
}

// a `ccc:calls` joined to a `ccc:serves` - here or in another repository
export interface Crossing {
  key: string;
  transport: string;
  from: string;
  to: string;
  file: string;
  line: number;
  function: string;
  external: boolean;
  // null when nothing anywhere serves this key
  remote: { function: string; file: string; line: number; service: string | null } | null;
}

export interface ServicesEdge {
  from: string;
  to: string;
  declared: boolean;
  detected: boolean;
  // strings, capped at 12; `count` carries the real total
  symbols: string[];
  count: number;
  // one representative site per target symbol, capped at 60
  sites: ServiceSite[];
}

export interface ServicesSection {
  // grouping provenance; the only signal of how services were derived
  source: string;
  declared_deps: Record<string, string[]>;
  services: { name: string; globs: string[]; files: number; funcs: number; paths: string[] }[];
  edges: ServicesEdge[];
  unassigned_files: string[];
  externals?: ExternalRepo[];
  external_names?: string[];
  crossings?: Crossing[];
}

export interface TestRun {
  test: string;
  // the test function's own definition site
  file: string;
  line: number;
  language: string;
  // call hops from the test to the change; 0 is a direct reference
  distance: number;
  // changed-function NAMES this test reaches, capped at 8
  covers: string[];
  reason: string;
}

export interface TriggerAdd {
  // "<file>::<function>"
  target: string;
  // whether test_targets ranked it
  resolved: boolean;
  lines: [number, number];
}

export interface TestCommand {
  language: string;
  command: string;
  selects: number;
  caveat?: string;
}

export interface TriggersSection {
  available: true;
  base: string;
  base_sha: string;
  head_sha: string;
  uncommitted_files: string[];
  services_to_test: string[];
  run: TestRun[];
  add: TriggerAdd[];
  commands: TestCommand[];
  total_tests: number;
  full_suite_advised: boolean;
  counts: Record<string, number>;
  note?: string;
  changed_note?: string;
}

export type TestKind = 'smoke-test' | 'integration-test' | 'contract-test' | 'perf-test' | 'load-test';

export interface TestTarget {
  // "<file>::<function>" - joins exactly with TriggerAdd.target
  id: string;
  function: string;
  file: string;
  line: number;
  language: string;
  service: string;
  kind: TestKind;
  also: string[];
  priority: number;
  covered: boolean;
  covered_by: string[];
  suggest: string;
  why: { factor: string; value: number | string; detail: string }[];
  semantics: string[];
  signals: Record<string, number | boolean>;
}

export interface TargetsSection {
  targets: TestTarget[];
  truncated: boolean;
  summary: Record<string, unknown>;
  note?: string;
}

// a structurally significant function
export interface HotRow {
  name: string;
  file: string;
  line: number;
  // distinct functions that call this one
  callers: number;
  // call sites reaching it
  call_sites: number;
  // distinct functions it calls (fan-out)
  calls: number;
  lines: number;
  complexity: number;
  loop_depth: number;
  recursive: boolean;
  language: string;
  test: boolean;
}

export interface HotNode {
  name: string;
  file: string;
  line: number;
  complexity?: number;
}

export interface HotChain {
  depth: number;
  call_sites: number;
  chain: HotNode[];
}

export interface HotCycle {
  size: number;
  members: HotNode[];
}

// `hot` is derived from the call graph alone - the one section about a file nobody has touched
export interface HotSection {
  most_called: HotRow[];
  widest: HotRow[];
  most_complex: HotRow[];
  deepest_chains: HotChain[];
  cycles: HotCycle[];
  note?: string;
}

export interface InsightsPayload {
  schema: string;
  root: string;
  generated: string;
  totals?: Record<string, number>;
  services?: ServicesSection;
  changes?: ChangesSection | Unavailable;
  test_triggers?: TriggersSection | Unavailable;
  test_targets?: TargetsSection;
  hot?: HotSection;
  complexity?: ComplexitySection;
}

// `GET /file?path=` - the only source of name-token positions
export interface FileStructure {
  path: string;
  language: string;
  cache_name: string;
  funcs: {
    // the row the function's NAME is on
    line: number;
    // 1-based column of the name token
    col: number;
    name: string;
    ret: string | null;
    doc: string | null;
    // [start_line, end_line] of the whole definition
    span: [number, number];
    // cyclomatic-style count: 1 path, plus one per decision point and loop
    complexity?: number;
    // the same count banded onto 1-10, which is what the margin can draw
    complexity_score?: number;
    branches?: number;
    loop_depth?: number;
    body_lines?: number;
  }[];
  refs: { caller: string; call_line: number; target: string; target_line: number }[];
}

export interface ReferenceHit {
  kind: string;
  file: string;
  line: number;
  caller?: string;
  qualifier?: string;
  test_ctx?: boolean;
}

export interface ReferencesResult {
  symbol: string;
  definitions: ReferenceHit[];
  references: ReferenceHit[];
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function arr<T>(value: unknown): T[] {
  return Array.isArray(value) ? (value as T[]) : [];
}

export function str(value: unknown, fallback = ''): string {
  return typeof value === 'string' ? value : fallback;
}

export function num(value: unknown, fallback = 0): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

export function bool(value: unknown, fallback = false): boolean {
  return typeof value === 'boolean' ? value : fallback;
}

export function lineSpan(value: unknown): [number, number] | undefined {
  if (!Array.isArray(value)) return undefined;
  const start = num(value[0], 0);
  const end = num(value[1], 0);
  if (start < 1) return undefined;
  return [start, Math.max(start, end)];
}

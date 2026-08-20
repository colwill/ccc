import * as vscode from 'vscode';
import type { TraceLevel } from './log';

export type CrossServiceMode = 'auto' | 'always' | 'off';
export type MinEvidence = 'any' | 'evidence';
export type DecorationStyle = 'badge+gutter' | 'badge' | 'gutter';

export interface Cfg {
  enable: boolean;
  binaryPath: string;
  baseRef: string | undefined;
  server: {
    autoStart: boolean;
    address: string;
    port: number;
    watchIntervalSec: number;
    startupTimeoutMs: number;
    extraArgs: string[];
  };
  hints: {
    testTriggers: boolean;
    untested: boolean;
    outbound: boolean;
    inbound: boolean;
    hotPaths: boolean;
    codeLens: boolean;
    includeTestFiles: boolean;
    crossServiceMode: CrossServiceMode;
    minEvidence: MinEvidence;
  };
  untested: {
    showUncoveredTargets: boolean;
    minPriority: number;
  };
  decorations: {
    style: DecorationStyle;
    badgeMaxLength: number;
    dimWhenDirty: boolean;
    overviewRuler: boolean;
  };
  complexity: {
    enabled: boolean;
    // the lowest band still worth drawing; below it the mark is omitted
    minScore: number;
  };
  refresh: {
    onSave: boolean;
    onWindowFocus: boolean;
    intervalSec: number;
    debounceMs: number;
  };
  trace: TraceLevel;
}

// read the config for a scope - settings are per workspace folder so multi-root folders can differ
export function readConfig(scope?: vscode.ConfigurationScope): Cfg {
  const c = vscode.workspace.getConfiguration('ccc', scope);
  const baseRef = c.get<string>('baseRef', '').trim();
  return {
    enable: c.get<boolean>('enable', true),
    binaryPath: c.get<string>('binaryPath', '').trim(),
    baseRef: baseRef.length > 0 ? baseRef : undefined,
    server: {
      autoStart: c.get<boolean>('server.autoStart', true),
      address: c.get<string>('server.address', '127.0.0.1').trim() || '127.0.0.1',
      port: clampInt(c.get<number>('server.port', 0), 0, 65535, 0),
      watchIntervalSec: clampInt(c.get<number>('server.watchIntervalSec', 0), 0, 3600, 0),
      startupTimeoutMs: clampInt(c.get<number>('server.startupTimeoutMs', 30000), 1000, 600000, 30000),
      extraArgs: c.get<string[]>('server.extraArgs', []).filter((a) => typeof a === 'string'),
    },
    hints: {
      testTriggers: c.get<boolean>('hints.testTriggers', true),
      untested: c.get<boolean>('hints.untested', true),
      outbound: c.get<boolean>('hints.outbound', true),
      inbound: c.get<boolean>('hints.inbound', true),
      hotPaths: c.get<boolean>('hints.hotPaths', true),
      codeLens: c.get<boolean>('hints.codeLens', true),
      includeTestFiles: c.get<boolean>('hints.includeTestFiles', false),
      crossServiceMode: c.get<CrossServiceMode>('hints.crossServiceMode', 'auto'),
      minEvidence: c.get<MinEvidence>('hints.minEvidence', 'evidence'),
    },
    untested: {
      showUncoveredTargets: c.get<boolean>('untested.showUncoveredTargets', false),
      minPriority: c.get<number>('untested.minPriority', 0),
    },
    decorations: {
      style: c.get<DecorationStyle>('decorations.style', 'gutter'),
      badgeMaxLength: clampInt(c.get<number>('decorations.badgeMaxLength', 60), 8, 400, 60),
      dimWhenDirty: c.get<boolean>('decorations.dimWhenDirty', true),
      overviewRuler: c.get<boolean>('decorations.overviewRuler', true),
    },
    complexity: {
      enabled: c.get<boolean>('complexity.enabled', true),
      minScore: clampInt(c.get<number>('complexity.minScore', 1), 1, 10, 1),
    },
    refresh: {
      onSave: c.get<boolean>('refresh.onSave', true),
      onWindowFocus: c.get<boolean>('refresh.onWindowFocus', true),
      intervalSec: clampInt(c.get<number>('refresh.intervalSec', 0), 0, 86400, 0),
      debounceMs: clampInt(c.get<number>('refresh.debounceMs', 400), 0, 60000, 400),
    },
    trace: c.get<TraceLevel>('trace', 'off'),
  };
}

// settings that can only be honoured by restarting the analyser process
export function needsServerRestart(a: Cfg, b: Cfg): boolean {
  return (
    a.binaryPath !== b.binaryPath ||
    a.server.address !== b.server.address ||
    a.server.port !== b.server.port ||
    a.server.watchIntervalSec !== b.server.watchIntervalSec ||
    a.server.extraArgs.join('\u0000') !== b.server.extraArgs.join('\u0000')
  );
}

// settings that change the hint index but not the payload - a rebuild from the cache is enough
export function needsRebuild(a: Cfg, b: Cfg): boolean {
  return (
    JSON.stringify(a.hints) !== JSON.stringify(b.hints) ||
    JSON.stringify(a.untested) !== JSON.stringify(b.untested) ||
    a.decorations.badgeMaxLength !== b.decorations.badgeMaxLength
  );
}

// settings that require the decoration types themselves to be recreated
export function needsDecorationReload(a: Cfg, b: Cfg): boolean {
  return (
    a.decorations.style !== b.decorations.style ||
    a.decorations.dimWhenDirty !== b.decorations.dimWhenDirty ||
    a.decorations.overviewRuler !== b.decorations.overviewRuler
  );
}

function clampInt(value: number, min: number, max: number, fallback: number): number {
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, Math.trunc(value)));
}

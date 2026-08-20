import { execFile } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vscode from 'vscode';
import type { Cfg } from './config';
import type { Log } from './log';

export type BinarySource = 'config' | 'path' | 'target-release' | 'target-debug';

export interface BinaryResolution {
  path: string;
  source: BinarySource;
  version?: string;
}

export class CccBinaryError extends Error {
  constructor(
    message: string,
    readonly searched: string[],
  ) {
    super(message);
    this.name = 'CccBinaryError';
  }
}

const EXE = process.platform === 'win32' ? '.exe' : '';

// find a usable `ccc` binary - a broken `ccc.binaryPath` errors rather than silently falling through
export async function resolveCccBinary(folder: vscode.Uri, cfg: Cfg, log: Log): Promise<BinaryResolution> {
  const searched: string[] = [];

  if (cfg.binaryPath.length > 0) {
    const configured = path.isAbsolute(cfg.binaryPath)
      ? cfg.binaryPath
      : path.join(folder.fsPath, cfg.binaryPath);
    searched.push(`ccc.binaryPath (${configured})`);
    const version = await probe(configured);
    if (version === undefined) {
      throw new CccBinaryError(
        `ccc.binaryPath points at \`${configured}\`, which is not an executable ccc binary.`,
        searched,
      );
    }
    log.info(`using ccc from ccc.binaryPath: ${configured} (${version})`);
    return { path: configured, source: 'config', version };
  }

  const onPath = `ccc${EXE}`;
  searched.push('PATH');
  const pathVersion = await probe(onPath);
  if (pathVersion !== undefined) {
    log.info(`using ccc from PATH (${pathVersion})`);
    return { path: onPath, source: 'path', version: pathVersion };
  }

  const candidates: Array<[BinarySource, string]> = [
    ['target-release', path.join(folder.fsPath, 'target', 'release', `ccc${EXE}`)],
    ['target-debug', path.join(folder.fsPath, 'target', 'debug', `ccc${EXE}`)],
  ];
  for (const [source, candidate] of candidates) {
    searched.push(candidate);
    if (!fs.existsSync(candidate)) continue;
    const version = await probe(candidate);
    if (version === undefined) continue;
    log.info(`using ccc from ${source}: ${candidate} (${version})`);
    return { path: candidate, source, version };
  }

  throw new CccBinaryError(
    'could not find the `ccc` binary. Install it with `./install.sh` or `cargo build --release` ' +
      'in the codecache repo, or set `ccc.binaryPath`.',
    searched,
  );
}

// run `<bin> --version`; undefined means "not a usable ccc binary"
function probe(bin: string): Promise<string | undefined> {
  return new Promise((resolve) => {
    execFile(bin, ['--version'], { timeout: 5000, windowsHide: true }, (err, stdout) => {
      if (err) {
        resolve(undefined);
        return;
      }
      const text = stdout.trim();
      resolve(text.length > 0 ? text : 'unknown');
    });
  });
}

import { execFile } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vscode from 'vscode';
import type { Cfg } from './config';
import type { Log } from './log';

export type BinarySource = 'config' | 'path' | 'target-release' | 'target-debug' | 'managed';

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

// release assets are one binary per platform, named the way install.sh names them
const REPO = 'https://github.com/colwill/ccc';

// activation waits on the download, so it must not be able to hang for ever
const DOWNLOAD_TIMEOUT_MS = 60_000;

// one download at a time: a multi-root window resolves the binary once per folder,
// and two writers racing on the same file is the one way to install a broken copy
let installing: Promise<BinaryResolution | undefined> | undefined;

// find a usable `ccc` binary - a broken `ccc.binaryPath` errors rather than silently falling through
export async function resolveCccBinary(
  folder: vscode.Uri,
  cfg: Cfg,
  log: Log,
  // where a downloaded binary lives; omitted, auto-install is unavailable
  storage?: vscode.Uri,
): Promise<BinaryResolution> {
  const searched: string[] = [];
  const found = await findCcc([folder], cfg, log, storage, searched);
  if (found) return found;

  if (storage !== undefined && cfg.autoInstall) {
    const installed = await installCccBinary(storage, log);
    if (installed !== undefined) return installed;
    note(searched, `${REPO}/releases/latest (download failed - see the ccc output channel)`);
  }

  throw missingBinary(cfg, searched);
}

// The install-time path, run once when the extension activates: make sure a
// working ccc exists before anything asks to spawn one. Searches every
// workspace folder before paying for a download, and refreshes a copy this
// extension installed for an older version of itself
export async function bootstrapCccBinary(
  folders: readonly vscode.Uri[],
  cfg: Cfg,
  log: Log,
  storage: vscode.Uri,
  // this extension's version, on the first activation after an install or an
  // update; the binary ships from the same repo on the same version, so a
  // managed copy that does not match it is out of date. Undefined skips the check
  wantVersion?: string,
): Promise<BinaryResolution> {
  const searched: string[] = [];
  const found = await findCcc(folders, cfg, log, storage, searched);

  // only a copy we installed is ours to replace - a build or an install the
  // user manages is theirs, whatever version it reports
  const stale =
    found !== undefined &&
    found.source === 'managed' &&
    wantVersion !== undefined &&
    !versionMatches(found.version, wantVersion);
  if (found && !stale) return found;

  if (!cfg.autoInstall) {
    if (found) return found;
    throw missingBinary(cfg, searched);
  }
  if (stale) log.info(`the installed ${found?.version ?? 'ccc'} predates this extension (${wantVersion})`);

  const installed = await installCccBinary(storage, log);
  if (installed !== undefined) return installed;
  // a stale copy that still runs beats no analyser at all
  if (found) {
    log.warn(`could not refresh the installed ccc; keeping ${found.version ?? found.path}`);
    return found;
  }
  note(searched, `${REPO}/releases/latest (download failed - see the ccc output channel)`);
  throw missingBinary(cfg, searched);
}

// search the places a ccc may already be, in order of how much the user meant it.
// Records every place looked at in `searched`, for the error message
async function findCcc(
  folders: readonly vscode.Uri[],
  cfg: Cfg,
  log: Log,
  storage: vscode.Uri | undefined,
  searched: string[],
): Promise<BinaryResolution | undefined> {
  const configured =
    cfg.binaryPath.length === 0
      ? []
      : path.isAbsolute(cfg.binaryPath)
        ? [cfg.binaryPath]
        : // a relative binaryPath with no folder open resolves to nothing, so the search goes on
          folders.map((folder) => path.join(folder.fsPath, cfg.binaryPath));
  if (configured.length > 0) {
    for (const candidate of configured) {
      note(searched, `ccc.binaryPath (${candidate})`);
      const version = await probe(candidate);
      if (version === undefined) continue;
      log.info(`using ccc from ccc.binaryPath: ${candidate} (${version})`);
      return { path: candidate, source: 'config', version };
    }
    throw new CccBinaryError(
      `ccc.binaryPath points at \`${cfg.binaryPath}\`, which is not an executable ccc binary.`,
      searched,
    );
  }

  const onPath = `ccc${EXE}`;
  note(searched, 'PATH');
  const pathVersion = await probe(onPath);
  if (pathVersion !== undefined) {
    log.info(`using ccc from PATH (${pathVersion})`);
    return { path: onPath, source: 'path', version: pathVersion };
  }

  for (const folder of folders) {
    const candidates: Array<[BinarySource, string]> = [
      ['target-release', path.join(folder.fsPath, 'target', 'release', `ccc${EXE}`)],
      ['target-debug', path.join(folder.fsPath, 'target', 'debug', `ccc${EXE}`)],
    ];
    for (const [source, candidate] of candidates) {
      note(searched, candidate);
      if (!fs.existsSync(candidate)) continue;
      const version = await probe(candidate);
      if (version === undefined) continue;
      log.info(`using ccc from ${source}: ${candidate} (${version})`);
      return { path: candidate, source, version };
    }
  }

  // a copy this extension installed earlier, before paying for another download
  if (storage !== undefined) {
    const managed = managedPath(storage);
    note(searched, managed);
    const version = await probe(managed);
    if (version !== undefined) {
      log.info(`using ccc installed by the extension: ${managed} (${version})`);
      return { path: managed, source: 'managed', version };
    }
  }

  return undefined;
}

function missingBinary(cfg: Cfg, searched: string[]): CccBinaryError {
  const hint = cfg.autoInstall
    ? 'Automatic install did not produce a usable binary.'
    : 'Automatic install is off (`ccc.autoInstall`).';
  return new CccBinaryError(
    `could not find the \`ccc\` binary. ${hint} Install it with \`./install.sh\` or ` +
      '`cargo build --release` in the codecache repo, or set `ccc.binaryPath`.',
    searched,
  );
}

// the same place can be reached from several folders - say it once
function note(searched: string[], place: string): void {
  if (!searched.includes(place)) searched.push(place);
}

// `ccc --version` prints "ccc <version>", so compare the version token alone
function versionMatches(reported: string | undefined, want: string): boolean {
  if (reported === undefined) return false;
  return (reported.trim().split(/\s+/).pop() ?? '') === want;
}

// where a downloaded binary is kept: the extension's own storage, so installing
// never has to write to a directory on the user's PATH
function managedPath(storage: vscode.Uri): string {
  return path.join(storage.fsPath, 'bin', `ccc${EXE}`);
}

// the release asset for this machine, named as install.sh names it
function assetName(): string | undefined {
  const os = { linux: 'linux', darwin: 'macos', win32: 'windows' }[process.platform as string];
  const arch = { x64: 'x86_64', arm64: 'aarch64', arm: 'armv7', ia32: 'i686', riscv64: 'riscv64' }[
    process.arch as string
  ];
  if (os === undefined || arch === undefined) return undefined;
  // armv7/i686/riscv64 are published for linux only
  if (os !== 'linux' && arch !== 'x86_64' && arch !== 'aarch64') return undefined;
  return `ccc-${os}-${arch}${os === 'windows' ? '.exe' : ''}`;
}

// Download the matching release into extension storage. Resolves undefined on
// any failure - a missing binary is already handled, and an editor extension
// should not turn a failed download into an unhandled error
export function installCccBinary(storage: vscode.Uri, log: Log): Promise<BinaryResolution | undefined> {
  // callers that arrive while a download is running join it instead of starting a second one
  installing ??= download(storage, log).finally(() => {
    installing = undefined;
  });
  return installing;
}

async function download(storage: vscode.Uri, log: Log): Promise<BinaryResolution | undefined> {
  const asset = assetName();
  if (asset === undefined) {
    log.warn(`no ccc release asset for ${process.platform}/${process.arch}; build from source`);
    return undefined;
  }
  const url = `${REPO}/releases/latest/download/${asset}`;
  const target = managedPath(storage);

  return vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: 'Installing ccc' },
    async (progress) => {
      progress.report({ message: `downloading ${asset}` });
      log.info(`installing ccc from ${url}`);
      // write beside the target and rename, so a half-written file is never left
      // looking like an installed binary. Two windows can activate at once and
      // share this storage, so the staging name is this process's own
      const staged = `${target}.${process.pid}.download`;
      try {
        const body = await fetchAsset(url);
        await fs.promises.mkdir(path.dirname(target), { recursive: true });
        await fs.promises.writeFile(staged, body, { mode: 0o755 });
        await fs.promises.rename(staged, target);
      } catch (err) {
        log.warn(`ccc download failed: ${err instanceof Error ? err.message : String(err)}`);
        // a rename can fail over a binary another window is running - leave nothing behind
        await fs.promises.rm(staged, { force: true }).catch(() => undefined);
        return undefined;
      }
      // catches a wrong-arch asset, or an HTML error page saved as a binary
      const version = await probe(target);
      if (version === undefined) {
        log.warn(`downloaded ccc does not run on this machine (${process.platform}/${process.arch})`);
        await fs.promises.rm(target, { force: true });
        return undefined;
      }
      log.info(`installed ${version} at ${target}`);
      void vscode.window.showInformationMessage(`${version} installed.`);
      return { path: target, source: 'managed' as const, version };
    },
  );
}

// fetch a release asset, following the redirect GitHub serves for `latest`
async function fetchAsset(url: string): Promise<Buffer> {
  const res = await fetch(url, { redirect: 'follow', signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS) });
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText} for ${url}`);
  }
  return Buffer.from(await res.arrayBuffer());
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

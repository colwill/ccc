import { promises as fs } from 'node:fs';
import * as path from 'node:path';
import type * as vscode from 'vscode';
import { describe, type Log } from './log';
import type { ServerAddress } from './server';

// the key both agent configs use for the server this window owns
const SERVER_KEY = 'ccc';

// claude code reads .mcp.json at the workspace root, copilot reads .vscode/mcp.json
// the two schemas differ only in the property holding the server table
interface Target {
  rel: string;
  container: 'mcpServers' | 'servers';
  agent: string;
}

const TARGETS: readonly Target[] = [
  { rel: '.mcp.json', container: 'mcpServers', agent: 'claude code' },
  { rel: path.join('.vscode', 'mcp.json'), container: 'servers', agent: 'copilot' },
];

// a file that exists but does not parse - never clobbered, only reported
const MALFORMED = Symbol('malformed');

// publish the live endpoint so agents can discover the analyser this window started
// the port is ephemeral, so this reruns on every successful start
export async function publishMcpConfig(
  folder: vscode.Uri,
  address: ServerAddress,
  log: Log,
): Promise<void> {
  const url = `${address.base}/mcp`;
  for (const target of TARGETS) {
    const file = path.join(folder.fsPath, target.rel);
    try {
      const wrote = await writeOne(file, target, url);
      if (wrote) log.info(`[mcp] ${target.rel} now points ${target.agent} at ${url}`);
      else log.trace(`[mcp] ${target.rel} already pointed at ${url}`);
    } catch (err) {
      log.warn(`[mcp] could not publish ${target.rel}: ${describe(err)}`);
    }
  }
}

// returns true when the file was rewritten, false when it already said the right thing
async function writeOne(file: string, target: Target, url: string): Promise<boolean> {
  const existing = await readJson(file);
  if (existing === MALFORMED) {
    throw new Error('it exists but is not valid JSON, so it was left untouched');
  }

  const doc = existing ?? {};
  const table = asRecord(doc[target.container]) ?? {};
  // keep every other server the user configured and replace only ours
  const entry = { ...(asRecord(table[SERVER_KEY]) ?? {}), type: 'http', url };
  const next = { ...doc, [target.container]: { ...table, [SERVER_KEY]: entry } };

  const body = `${JSON.stringify(next, null, 2)}\n`;
  if (existing !== undefined && body === `${JSON.stringify(doc, null, 2)}\n`) return false;

  await fs.mkdir(path.dirname(file), { recursive: true });
  // write beside the target and rename so an agent never reads a half-written config
  const tmp = `${file}.${process.pid}.tmp`;
  try {
    await fs.writeFile(tmp, body, 'utf8');
    await fs.rename(tmp, file);
  } catch (err) {
    await fs.rm(tmp, { force: true }).catch(() => undefined);
    throw err;
  }
  return true;
}

async function readJson(
  file: string,
): Promise<Record<string, unknown> | undefined | typeof MALFORMED> {
  let raw: string;
  try {
    raw = await fs.readFile(file, 'utf8');
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    throw err;
  }
  if (raw.trim().length === 0) return undefined;
  try {
    // a jsonc file with comments lands here as malformed, which is the safe outcome
    return asRecord(JSON.parse(raw) as unknown) ?? MALFORMED;
  } catch {
    return MALFORMED;
  }
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}

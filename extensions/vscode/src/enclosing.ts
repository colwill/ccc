import type { CccClient } from './client';
import type { Cfg } from './config';
import { isAborted } from './client';
import type { Log } from './log';
import { enclosingFunction, type FileHints, type HintIndex, refineAnchors } from './model';
import type { FileStructure } from './types';

// per-file structure from `GET /file`
export class FileStructureCache {
  private entries = new Map<string, { generated: string; value: FileStructure | undefined }>();

  constructor(
    private readonly client: CccClient,
    private readonly log: Log,
  ) {}

  async get(rel: string, generated: string, signal?: AbortSignal): Promise<FileStructure | undefined> {
    const cached = this.entries.get(rel);
    if (cached && cached.generated === generated) return cached.value;
    try {
      const value = await this.client.file(rel, signal);
      this.entries.set(rel, { generated, value });
      return value;
    } catch (err) {
      if (!isAborted(err)) this.log.trace(`could not read structure for ${rel}: ${String(err)}`);
      return undefined;
    }
  }

  clear(): void {
    this.entries.clear();
  }
}

// second pass over file hints once its structure is known
export function refineFileHints(
  hints: FileHints,
  index: HintIndex,
  structure: FileStructure,
  cfg: Cfg,
): void {
  refineAnchors(hints, structure, cfg, index.serviceMode);

  const byName = index.coverageByFile.get(hints.rel);
  if (!byName || byName.size === 0) return;

  for (const hint of hints.lines.values()) {
    if (hint.coverage) continue;
    if (hint.outbound.length === 0 && hint.inbound.length === 0) continue;
    const fn = enclosingFunction(structure.funcs, hint.line);
    if (!fn) continue;
    const coverage = byName.get(fn.name);
    if (!coverage) continue;
    hint.coverage = coverage;
  }
}

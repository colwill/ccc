import * as vscode from 'vscode';

export type TraceLevel = 'off' | 'messages' | 'verbose';

const RANK: Record<TraceLevel, number> = { off: 0, messages: 1, verbose: 2 };

// the extension's single output channel - errors and warnings always show, the rest obeys `ccc.trace`
export class Log implements vscode.Disposable {
  private readonly channel: vscode.OutputChannel;
  private level: TraceLevel = 'off';

  constructor() {
    this.channel = vscode.window.createOutputChannel('ccc');
  }

  setLevel(level: TraceLevel): void {
    this.level = level;
  }

  show(): void {
    this.channel.show(true);
  }

  // always written
  error(message: string, err?: unknown): void {
    this.write('error', err ? `${message}: ${describe(err)}` : message);
  }

  // always written
  warn(message: string): void {
    this.write('warn', message);
  }

  // written at `messages` and above
  info(message: string): void {
    if (RANK[this.level] >= RANK.messages) this.write('info', message);
  }

  // written at `verbose` only
  trace(message: string): void {
    if (RANK[this.level] >= RANK.verbose) this.write('trace', message);
  }

  // raw analyser output prefixed so it is distinguishable from our own lines
  server(folder: string, stream: 'out' | 'err', text: string): void {
    for (const line of text.split(/\r?\n/)) {
      if (line.trim().length === 0) continue;
      // stderr always shows: the analyser only writes there when something is wrong
      if (stream === 'err') this.write('server', `[${folder}] ${line}`);
      else this.trace(`[server ${folder}] ${line}`);
    }
  }

  private write(kind: string, message: string): void {
    this.channel.appendLine(`${stamp()} [${kind}] ${message}`);
  }

  dispose(): void {
    this.channel.dispose();
  }
}

function stamp(): string {
  const d = new Date();
  const p = (n: number, w = 2) => String(n).padStart(w, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

export function describe(err: unknown): string {
  if (err instanceof Error) return err.stack ?? `${err.name}: ${err.message}`;
  if (typeof err === 'string') return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

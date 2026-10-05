import { execFile, spawn } from 'node:child_process';
import * as vscode from 'vscode';
import { resolveCccBinary } from './binary';
import type { Cfg } from './config';
import type { Log } from './log';

// where session recording stands for one repository, as `ccc replay status --json` says
interface RecordingStatus {
  repo: boolean;
  consent: 'unasked' | 'on' | 'off';
  exposure: {
    remote: string;
    url: string;
    host: string;
    // readable without signing in - null when that could not be told
    public: boolean | null;
    // a hosting service, where public means anyone on the internet
    public_host: boolean;
    // `.ccc/map.json` allows replays to a remote anyone can read
    allowed: boolean;
    // `.ccc/map.json` encrypts replays to the team's runccc key, so only the team opens them
    sealed?: boolean;
  } | null;
  // the runccc project `.ccc/map.json` encrypts replays to, and who on this machine is signed in to save them
  encrypt?: { project?: string; service?: string; login?: string | null; error?: string } | null;
}

// what `ccc login --json` prints - the code to approve, then who it signed in
interface LoginLine {
  user_code?: string;
  verification_uri?: string;
  verification_uri_complete?: string;
  login?: string;
}

const RECORD = 'Record sessions';
const KEEP_LOCAL = 'Record, keep them here';
const NOT_HERE = 'Not for this repo';
// what `.ccc/map.json` says to let replays go to a remote anyone can read
const ALLOW_PUBLIC = '"replays": { "allow_public_remote": true }';
const STOP = 'Stop recording';
const SIGN_IN = 'Sign in to Runccc Teams';
const SIGN_IN_AND_RECORD = 'Sign in and record';
const TRY_AGAIN = 'Try again';

// Whether a repository's agent sessions may be recorded as replays beside its
// branches - the question `ccc run` asks on a terminal, its answer kept in the
// same place (`git config ccc.replay`) so either holds for both. Asked once:
// only while nobody has answered, unless `again` - the command - asks it anew.
// What it says depends on who can read the remote, since a replay carries
// prompts and code. Where `.ccc/map.json` encrypts replays through Runccc Teams
// and nobody here has signed in, recording starts with a sign-in - nothing is
// saved without one.
export async function askToRecord(
  folder: vscode.WorkspaceFolder,
  cfg: Cfg,
  log: Log,
  storage: vscode.Uri | undefined,
  again = false,
): Promise<void> {
  const bin = (await resolveCccBinary(folder.uri, cfg, log, storage)).path;
  const cwd = folder.uri.fsPath;
  const status = JSON.parse(await run(bin, ['replay', 'status', '--json', cwd], cwd)) as RecordingStatus;
  if (!status.repo || (status.consent !== 'unasked' && !again)) return;
  // replays here are encrypted to the team's key on Runccc Teams, and nobody on this machine is signed in to save them
  const signedOut = !!status.encrypt?.project && !status.encrypt.login;

  if (status.consent === 'on') {
    const choice = await vscode.window.showInformationMessage(
      `ccc: agent sessions in ${folder.name} are recorded as replays, beside its branches.` +
        (signedOut ? " They are encrypted to your team's key on Runccc Teams, and none is saved until you sign in." : ''),
      ...(signedOut ? [SIGN_IN, STOP] : [STOP]),
    );
    if (choice === STOP) await answer(bin, cwd, false, folder, log);
    if (choice === SIGN_IN) await signIn(bin, cwd, folder, log);
    return;
  }

  const e = status.exposure;
  const intro =
    `ccc can record agent sessions in ${folder.name} as replays, kept beside its branches (refs/ccc) and ` +
    'pushed with them, so your team can watch how each change was made. A replay carries your prompts and ' +
    'the code each step touched; anything that looks like a secret is redacted first.';
  let detail: string;
  let choices: string[];
  if (e?.public && e.sealed) {
    detail = `${e.remote} (${e.url}) answers without signing in, but .ccc/map.json encrypts replays to your team's runccc key, so only the team opens them.`;
    choices = [RECORD, NOT_HERE];
  } else if (e?.public && e.allowed) {
    detail = `${e.remote} (${e.url}) answers without signing in, and .ccc/map.json allows replays to it.`;
    choices = [RECORD, NOT_HERE];
  } else if (e?.public && e.public_host) {
    detail =
      `${e.remote} (${e.url}) can be read by anyone without signing in, so replays would stay on this ` +
      'machine - make the repository private to share them with your team.';
    choices = [KEEP_LOCAL, NOT_HERE];
  } else if (e?.public) {
    detail =
      `${e.remote} (${e.url}) answers without signing in, so replays stay on this machine. If ${e.host} is ` +
      "your company's internal instance, where every repository is readable company-wide, allow it in " +
      `.ccc/map.json, where the team sees it: ${ALLOW_PUBLIC}`;
    choices = [KEEP_LOCAL, NOT_HERE];
  } else {
    detail =
      e?.public === false
        ? `${e.remote} needs a sign-in to read, so replays reach only people who can already read the code - ideal for a team.`
        : 'Ideal for a team on a private repository.';
    choices = [RECORD, NOT_HERE];
  }
  if (signedOut) {
    detail += " Saving them takes a sign-in to Runccc Teams, which holds your team's key.";
    choices = choices.map((c) => (c === RECORD ? SIGN_IN_AND_RECORD : c));
  }
  const choice = await vscode.window.showInformationMessage(`${intro} ${detail}`, ...choices);
  // put aside unanswered, it is asked again next time
  if (!choice) return;
  await answer(bin, cwd, choice !== NOT_HERE, folder, log);
  if (choice === SIGN_IN_AND_RECORD) await signIn(bin, cwd, folder, log);
}

// Signs in to Runccc Teams, which holds the key a repository's replays are
// encrypted to. `ccc login --json` hands over the code and waits while it is
// approved; the page opens through VS Code, so it reaches the browser on this
// machine even when the window is remote. Nothing is kept until it is approved.
export async function signIn(bin: string, cwd: string, folder: vscode.WorkspaceFolder, log: Log): Promise<void> {
  try {
    const who = await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: 'ccc: signing in to Runccc Teams', cancellable: true },
      (progress, cancel) => approve(bin, cwd, progress, cancel),
    );
    if (!who) return;
    log.info(`[${folder.name}] signed in to Runccc Teams as ${who}`);
    void vscode.window.showInformationMessage(
      `ccc: signed in to Runccc Teams as ${who} - replays in ${folder.name} are encrypted to your team's key as they are saved.`,
    );
  } catch (err) {
    const why = err instanceof Error ? err.message : String(err);
    log.warn(`[${folder.name}] could not sign in to Runccc Teams: ${why}`);
    const choice = await vscode.window.showErrorMessage(`ccc: could not sign in to Runccc Teams - ${why}`, TRY_AGAIN);
    if (choice === TRY_AGAIN) await signIn(bin, cwd, folder, log);
  }
}

// `ccc login --json`, run until the code is approved - undefined when cancelled
function approve(
  bin: string,
  cwd: string,
  progress: vscode.Progress<{ message?: string }>,
  cancel: vscode.CancellationToken,
): Promise<string | undefined> {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, ['login', '--json', cwd], { cwd, windowsHide: true });
    let pending = '';
    let said = '';
    let who: string | undefined;
    cancel.onCancellationRequested(() => child.kill());
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk: string) => {
      pending += chunk;
      const lines = pending.split('\n');
      pending = lines.pop() ?? '';
      for (const line of lines) {
        let got: LoginLine;
        try {
          got = JSON.parse(line) as LoginLine;
        } catch {
          continue;
        }
        if (got.login) who = got.login;
        else if (got.user_code && got.verification_uri_complete) show(got, progress);
      }
    });
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk: string) => (said += chunk));
    child.on('error', reject);
    child.on('close', (code) => {
      if (cancel.isCancellationRequested) resolve(undefined);
      else if (code === 0 && who) resolve(who);
      else reject(new Error(said.trim().replace(/^ccc: error: /, '') || `ccc login stopped with exit code ${code}`));
    });
  });
}

// the approval page opened, its code shown to check against it - and where to type it should the page not open
function show(got: LoginLine, progress: vscode.Progress<{ message?: string }>): void {
  progress.report({ message: `approve the code ${got.user_code} in your browser` });
  void vscode.env.openExternal(vscode.Uri.parse(got.verification_uri_complete ?? '')).then((opened) => {
    if (!opened) progress.report({ message: `open ${got.verification_uri} and enter the code ${got.user_code}` });
  });
}

async function answer(bin: string, cwd: string, on: boolean, folder: vscode.WorkspaceFolder, log: Log): Promise<void> {
  log.info(`[${folder.name}] ${(await run(bin, ['replay', on ? 'enable' : 'disable', cwd], cwd)).trim()}`);
}

function run(bin: string, args: string[], cwd: string): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(bin, args, { cwd, timeout: 20_000 }, (err, stdout) => (err ? reject(err) : resolve(stdout)));
  });
}

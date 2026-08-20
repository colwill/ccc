// pure path helpers free of any `vscode` import so the model layer runs outside an extension host

// filesystems that are case-insensitive by default
const CASE_INSENSITIVE = process.platform === 'win32' || process.platform === 'darwin';

// the key a file is indexed under - normalised so `src/Money.rs` and `src/money.rs` cannot both exist
export function keyOfPath(fsPath: string): string {
  const p = fsPath.split('\\').join('/');
  return CASE_INSENSITIVE ? p.toLowerCase() : p;
}

// mirrored from `is_test_path` in src/changes.rs so the extension agrees with what it displays
const TEST_DIRS = new Set(['test', 'tests', '__tests__', 'spec', 'testdata']);

export function isTestPath(rel: string): boolean {
  const segments = rel.split('/');
  const file = (segments[segments.length - 1] ?? '').toLowerCase();
  for (const seg of segments.slice(0, -1)) {
    if (TEST_DIRS.has(seg.toLowerCase())) return true;
  }
  // strip the final extension only, exactly as the analyser does
  const stem = file.includes('.') ? file.slice(0, file.lastIndexOf('.')) : file;
  return (
    stem.startsWith('test_') ||
    stem.endsWith('_test') ||
    stem.endsWith('.test') ||
    stem.endsWith('.spec') ||
    stem.endsWith('_spec')
  );
}

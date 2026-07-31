//! single-file HTML view of a `ccc changes` report.
//!
//! The generated page embeds the report JSON verbatim, renders it with
//! Tailwind (CDN), and carries an HTMX-powered "live query" panel that talks
//! to a running `ccc serve` instance via its `/fragment/*` endpoints - so the
//! same file both *views* the report and *queries* the live map.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

// render the single-file report page. `title` names the report (by
// convention the output file stem, e.g. `ccc-changes-rust`).
pub fn render_changes_html(report: &Value, title: &str) -> String {
    let json = serde_json::to_string(report)
        .unwrap_or_else(|_| "{}".into())
        // keep the inline <script type="application/json"> block unbreakable:
        // `<\/` is a legal JSON escape and defuses any `</script` in strings
        .replace("</", "<\\/");
    TEMPLATE
        .replace("__CCC_TITLE__", &esc(title))
        .replace("__CCC_REPORT__", &json)
}

pub fn write_changes_html(path: &Path, report: &Value, title: &str) -> Result<()> {
    std::fs::write(path, render_changes_html(report, title))
        .with_context(|| format!("writing {}", path.display()))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const TEMPLATE: &str = r####"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__CCC_TITLE__</title>
<script src="https://cdn.tailwindcss.com"></script>
<script src="https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js"></script>
</head>
<body class="bg-slate-950 text-slate-200 min-h-screen antialiased">
<div class="max-w-6xl mx-auto px-6 py-8 space-y-8">

  <header class="space-y-2">
    <div class="flex flex-wrap items-baseline gap-3">
      <h1 class="text-2xl font-semibold tracking-tight text-slate-50">ccc changes</h1>
      <span class="text-lg text-indigo-300 font-mono">__CCC_TITLE__</span>
      <span id="meta-schema" class="text-xs px-2 py-0.5 rounded-full bg-slate-800 text-slate-400 border border-slate-700"></span>
    </div>
    <p id="meta-line" class="text-sm text-slate-400 font-mono"></p>
  </header>

  <section id="tiles" class="grid grid-cols-2 sm:grid-cols-4 gap-3"></section>

  <section class="space-y-3">
    <h2 class="text-sm font-semibold uppercase tracking-wider text-slate-400">Services to test</h2>
    <div id="services-to-test" class="flex flex-wrap gap-2"></div>
    <div id="impact" class="space-y-1"></div>
  </section>

  <section class="space-y-3">
    <h2 class="text-sm font-semibold uppercase tracking-wider text-slate-400">Service edges</h2>
    <div id="edges" class="space-y-2"></div>
  </section>

  <section class="space-y-3">
    <h2 class="text-sm font-semibold uppercase tracking-wider text-slate-400">Changed functions</h2>
    <div id="changed-functions" class="overflow-x-auto rounded-xl border border-slate-800"></div>
  </section>

  <section id="untested-section" class="space-y-3 hidden">
    <h2 class="text-sm font-semibold uppercase tracking-wider text-rose-400">Untested changes</h2>
    <div id="untested" class="rounded-xl border border-rose-900/60 bg-rose-950/30 p-4 space-y-2"></div>
  </section>

  <section class="space-y-3">
    <h2 class="text-sm font-semibold uppercase tracking-wider text-slate-400">Changed files</h2>
    <div id="changed-files" class="overflow-x-auto rounded-xl border border-slate-800"></div>
  </section>

  <section id="unassigned-section" class="space-y-3 hidden">
    <h2 class="text-sm font-semibold uppercase tracking-wider text-amber-400">Unassigned files</h2>
    <div id="unassigned" class="rounded-xl border border-amber-900/60 bg-amber-950/20 p-4 font-mono text-sm text-amber-200"></div>
  </section>

  <section class="space-y-4 pt-4 border-t border-slate-800">
    <div class="flex flex-wrap items-center gap-3">
      <h2 class="text-sm font-semibold uppercase tracking-wider text-slate-400">Live query</h2>
      <input id="server-url" value="http://127.0.0.1:6767" spellcheck="false"
             class="bg-slate-900 border border-slate-700 rounded-lg px-3 py-1 text-sm font-mono text-slate-300 w-64 focus:outline-none focus:border-indigo-500">
      <span id="health-chip" data-frag="/fragment/health" hx-trigger="load, every 5s" hx-swap="innerHTML"
            class="text-xs font-mono text-slate-500">connecting&hellip;</span>
    </div>
    <p class="text-xs text-slate-500">Queries a running <code class="text-slate-400">ccc serve</code> for the <em>current</em> tree - start it in the project root. The report above stays as generated.</p>
    <div class="grid md:grid-cols-3 gap-4">
      <div class="rounded-xl border border-slate-800 bg-slate-900/50 p-4 space-y-3">
        <form data-frag="/fragment/find" hx-target="#find-out" hx-swap="innerHTML" class="space-y-2">
          <div class="text-sm font-medium text-slate-300">find</div>
          <div class="flex gap-2">
            <input name="q" placeholder="substring" spellcheck="false"
                   class="flex-1 min-w-0 bg-slate-950 border border-slate-700 rounded-lg px-2 py-1 text-sm font-mono focus:outline-none focus:border-indigo-500">
            <select name="kind" class="bg-slate-950 border border-slate-700 rounded-lg px-1 py-1 text-sm">
              <option>any</option><option>func</option><option>const</option><option>note</option>
            </select>
          </div>
          <button class="w-full bg-indigo-600 hover:bg-indigo-500 rounded-lg py-1 text-sm font-medium">search</button>
        </form>
        <div id="find-out" class="text-sm"></div>
      </div>
      <div class="rounded-xl border border-slate-800 bg-slate-900/50 p-4 space-y-3">
        <form data-frag="/fragment/references" hx-target="#refs-out" hx-swap="innerHTML" class="space-y-2">
          <div class="text-sm font-medium text-slate-300">references</div>
          <input name="symbol" placeholder="exact symbol name" spellcheck="false"
                 class="w-full bg-slate-950 border border-slate-700 rounded-lg px-2 py-1 text-sm font-mono focus:outline-none focus:border-indigo-500">
          <button class="w-full bg-indigo-600 hover:bg-indigo-500 rounded-lg py-1 text-sm font-medium">look up</button>
        </form>
        <div id="refs-out" class="text-sm"></div>
      </div>
      <div class="rounded-xl border border-slate-800 bg-slate-900/50 p-4 space-y-3">
        <form data-frag="/fragment/dependencies" hx-target="#deps-out" hx-swap="innerHTML" class="space-y-2">
          <div class="text-sm font-medium text-slate-300">dependencies</div>
          <input name="file" placeholder="file path (empty = whole graph)" spellcheck="false"
                 class="w-full bg-slate-950 border border-slate-700 rounded-lg px-2 py-1 text-sm font-mono focus:outline-none focus:border-indigo-500">
          <button class="w-full bg-indigo-600 hover:bg-indigo-500 rounded-lg py-1 text-sm font-medium">resolve</button>
        </form>
        <div id="deps-out" class="text-sm"></div>
      </div>
    </div>
  </section>

  <footer class="text-xs text-slate-600 pb-4">generated by github.com/colwill/<code>ccc changes --html</code> &middot; report embedded below &middot; live panel needs <code>ccc serve</code></footer>
</div>

<script id="ccc-report" type="application/json">__CCC_REPORT__</script>
<script>
const R = JSON.parse(document.getElementById('ccc-report').textContent);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const PALETTE = ['indigo','emerald','sky','fuchsia','amber','rose','teal','violet'];
const svcColor = {};
(R.services || []).forEach((s, i) => { svcColor[s] = PALETTE[i % PALETTE.length]; });
// With no `.ccc/map.json`, changes names the implicit whole-root service `.`,
// which renders as a bare dot and tells the reader nothing. Every place a
// service name is displayed goes through this.
const svcLabel = s => (s === '.' ? 'whole project' : s);
const chip = s => {
  const c = svcColor[s] || 'slate';
  return `<span class="inline-block whitespace-nowrap px-2 py-0.5 rounded-full text-xs font-medium bg-${c}-500/15 text-${c}-300 border border-${c}-500/30">${esc(svcLabel(s))}</span>`;
};
const codechip = (t, title) =>
  `<code title="${esc(title || '')}" class="inline-block whitespace-nowrap px-1.5 py-0.5 rounded bg-slate-800 text-slate-300 text-xs">${esc(t)}</code>`;

// header
document.getElementById('meta-schema').textContent = R.schema || '';
document.getElementById('meta-line').textContent =
  `root ${R.root || '.'}  ·  base ${String(R.base || '').slice(0, 12)}  ·  ${String(R.base_sha || '').slice(0, 9)} → ${String(R.head_sha || '').slice(0, 9)}`;

// tiles
const tile = (label, value, tone) => `
  <div class="rounded-xl border border-slate-800 bg-slate-900/50 p-4">
    <div class="text-3xl font-semibold ${tone}">${value}</div>
    <div class="text-xs text-slate-500 mt-1">${label}</div>
  </div>`;
const c = R.counts || {};
document.getElementById('tiles').innerHTML =
  tile('services to test', c.services_to_test ?? 0, 'text-indigo-300') +
  tile('changed files', c.changed_files ?? 0, 'text-slate-100') +
  tile('changed functions', c.changed_functions ?? 0, 'text-slate-100') +
  tile('untested', c.untested ?? 0, (c.untested ?? 0) > 0 ? 'text-rose-400' : 'text-emerald-400');

// services to test + impact chains
document.getElementById('services-to-test').innerHTML =
  (R.services_to_test || []).map(chip).join(' ') ||
  '<span class="text-sm text-slate-500">nothing to test - no changes vs base</span>';
document.getElementById('impact').innerHTML = (R.impact || [])
  .filter(i => i.reason !== 'changed')
  .map(i => `<div class="text-sm text-slate-400 font-mono">${(i.path || []).map(x => esc(svcLabel(x))).join(' <span class="text-slate-600">→</span> ')} <span class="text-xs text-slate-500">(${esc(i.reason)})</span></div>`)
  .join('');

// edges
document.getElementById('edges').innerHTML = (R.edges || []).map(e => `
  <div class="flex flex-wrap items-center gap-2 rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-2">
    ${chip(e.from)} <span class="text-slate-600">→</span> ${chip(e.to)}
    ${e.declared ? '<span class="inline-block whitespace-nowrap text-xs px-2 py-0.5 rounded-full bg-sky-500/15 text-sky-300 border border-sky-500/30">declared</span>' : ''}
    ${e.detected ? '<span class="inline-block whitespace-nowrap text-xs px-2 py-0.5 rounded-full bg-emerald-500/15 text-emerald-300 border border-emerald-500/30">detected</span>' : ''}
    ${e.declared && !e.detected ? '<span class="inline-block whitespace-nowrap text-xs px-2 py-0.5 rounded-full bg-slate-700/40 text-slate-400 border border-slate-600/40" title="the analysis ran and resolved no calls across this boundary">no calls found</span>' : ''}
    <span class="flex flex-wrap gap-1">${(e.symbols || []).map(s =>
      codechip(s.symbol, `${s.file}:${s.line} — resolved via ${s.via || 'name'}${s.kind === 'type' ? ' (type reference, not a call)' : ''}`)
      + `<span class="text-[10px] ${s.via === 'name-only' ? 'text-amber-400' : 'text-slate-500'} mr-1">${esc(s.via || '')}</span>`
    ).join('')}</span>
  </div>`).join('') || '<p class="text-sm text-slate-500">no cross-service edges</p>';

// changed functions
const fnRows = (R.changed_functions || []).map(f => `
  <tr class="border-t border-slate-800/70">
    <td class="px-3 py-1.5 font-mono text-slate-200">${esc(f.function)}</td>
    <td class="px-3 py-1.5 font-mono text-slate-400">${esc(f.file)}<span class="text-slate-600">:${(f.lines || [])[0]}-${(f.lines || [])[1]}</span></td>
    <td class="px-3 py-1.5">${(f.services || []).map(chip).join(' ')}</td>
    <td class="px-3 py-1.5">${f.tested ? '<span class="text-emerald-400">✓ tested</span>' : '<span class="text-rose-400">✗ untested</span>'}</td>
    <td class="px-3 py-1.5"><span class="flex flex-wrap gap-1">${(f.tested_by || []).map(t => codechip(t)).join('') || '<span class="text-slate-600">-</span>'}</span></td>
    <td class="px-3 py-1.5">${(f.called_from || []).map(chip).join(' ') || '<span class="text-slate-600">-</span>'}</td>
  </tr>`).join('');
document.getElementById('changed-functions').innerHTML = fnRows
  ? `<table class="w-full text-sm"><thead class="bg-slate-900 text-left text-xs text-slate-500">
      <tr><th class="px-3 py-2">function</th><th class="px-3 py-2">where</th><th class="px-3 py-2">services</th><th class="px-3 py-2">tests</th><th class="px-3 py-2">tested by</th><th class="px-3 py-2">called from</th></tr>
     </thead><tbody>${fnRows}</tbody></table>`
  : '<p class="text-sm text-slate-500 p-3">no function-level changes</p>';

// untested
if ((R.untested || []).length) {
  document.getElementById('untested-section').classList.remove('hidden');
  document.getElementById('untested').innerHTML = R.untested.map(f => `
    <div class="text-sm font-mono">${esc(f.function)} <span class="text-slate-500">${esc(f.file)}:${(f.lines || [])[0]}</span>
      ${(f.called_from || []).length ? '<span class="text-xs text-rose-300">← called from ' + f.called_from.map(x => esc(svcLabel(x))).join(', ') + '</span>' : ''}
    </div>`).join('');
}

// calls that matched a definition elsewhere but carried no evidence
const unresolved = R.unresolved_calls || [];
if (unresolved.length) {
  const host = document.getElementById('edges');
  host.insertAdjacentHTML('beforeend', `
    <details class="rounded-xl border border-slate-800 bg-slate-900/40 p-3 mt-2">
      <summary class="text-xs text-amber-400 cursor-pointer">${unresolved.length} unresolved call(s) - matched a definition in another service, but nothing named it</summary>
      <div class="mt-2 space-y-1">${unresolved.map(u => `
        <div class="text-xs font-mono text-slate-400">
          <span class="text-slate-300">${esc(svcLabel(u.from))}</span> → <span class="text-amber-300">${esc(u.symbol)}</span>
          <span class="text-slate-600">${esc(u.file)}:${u.line}</span>
          <span class="text-slate-500">[${esc(u.reason)}]</span>
          ${(u.candidates || []).length ? `<span class="text-slate-600">candidates: ${u.candidates.map(x => esc(svcLabel(x))).join(', ')}</span>` : ''}
        </div>`).join('')}</div>
    </details>`);
}

// changed files
const statusTone = { modified: 'text-amber-300', added: 'text-emerald-300', deleted: 'text-rose-300', renamed: 'text-sky-300', copied: 'text-sky-300' };
const fileRows = (R.changed_files || []).map(f => `
  <tr class="border-t border-slate-800/70">
    <td class="px-3 py-1.5 text-xs ${statusTone[f.status] || 'text-slate-400'}">${esc(f.status)}</td>
    <td class="px-3 py-1.5 font-mono text-slate-300">${esc(f.path)}</td>
    <td class="px-3 py-1.5">${(f.services || []).map(chip).join(' ') || '<span class="text-amber-400 text-xs">unassigned</span>'}</td>
  </tr>`).join('');
document.getElementById('changed-files').innerHTML = fileRows
  ? `<table class="w-full text-sm"><tbody>${fileRows}</tbody></table>`
  : '<p class="text-sm text-slate-500 p-3">no changed files</p>';

// unassigned
if ((R.unassigned_files || []).length) {
  document.getElementById('unassigned-section').classList.remove('hidden');
  document.getElementById('unassigned').innerHTML = R.unassigned_files.map(esc).join('<br>');
}

// live-query wiring: point every [data-frag] element at the server URL
function setBase(base) {
  base = base.replace(/\/+$/, '');
  document.querySelectorAll('[data-frag]').forEach(el => {
    el.setAttribute('hx-get', base + el.dataset.frag);
    htmx.process(el);
  });
}
const urlInput = document.getElementById('server-url');
urlInput.addEventListener('change', () => setBase(urlInput.value));
setBase(urlInput.value);
document.body.addEventListener('htmx:sendError', e => {
  const sel = e.detail.elt.getAttribute('hx-target');
  const out = sel ? document.querySelector(sel) : e.detail.elt;
  if (out) out.innerHTML = out.id === 'health-chip'
    ? '<span class="text-rose-400">● offline</span>'
    : '<p class="text-rose-400 text-xs">server unreachable - is <code>ccc serve</code> running?</p>';
});
</script>
</body>
</html>
"####;

// Render the `/insights` page; two modes from one template.
pub fn render_insights_html(root_label: &str, embed: Option<&Value>) -> String {
    let data = match embed {
        // `<\/` is a legal JSON escape, and defuses any `</script` in a string
        Some(v) => serde_json::to_string(v)
            .unwrap_or_else(|_| "null".into())
            .replace("</", "<\\/"),
        None => "null".to_string(),
    };
    INSIGHTS_TEMPLATE
        .replace("__CCC_ROOT__", &esc(root_label))
        .replace("__CCC_EMBED__", &data)
}

// Write the self-contained page for static hosting, creating the output
// directory if a build step has not made it yet (`public/index.html`).
pub fn write_insights_html(path: &Path, root_label: &str, report: &Value) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, render_insights_html(root_label, Some(report)))
        .with_context(|| format!("writing {}", path.display()))
}

const INSIGHTS_TEMPLATE: &str = r####"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ccc insights - __CCC_ROOT__</title>
<script src="https://cdn.tailwindcss.com"></script>
</head>
<body class="bg-slate-950 text-slate-200 min-h-screen antialiased">
<div class="max-w-7xl mx-auto px-6 py-8 space-y-6">

  <header class="space-y-2">
    <div class="flex flex-wrap items-baseline gap-3">
      <h1 class="text-2xl font-semibold tracking-tight text-slate-50">__CCC_ROOT__</h1>
      <span class="text-lg text-indigo-300 font-mono"></span>
      <span id="meta" class="text-xs text-slate-500 font-mono"></span>
      <button id="refresh" title="re-read the project from disk, then re-run the analysis"
              class="ml-auto text-xs px-3 py-1 rounded-lg border border-slate-700 hover:border-slate-500 text-slate-300 disabled:opacity-50">refresh</button>
    </div>
    <p class="text-xs text-slate-500 max-w-3xl">
      Everything below is read from the syntax tree - the call graph, per-function
      measurements and your service globs. There is no type inference, no data flow
      and no runtime profile behind it, so treat the findings as
      <span class="text-amber-400">advisory</span>, not proofs.
    </p>
  </header>

  <section id="tiles" class="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-3"></section>

  <nav id="tabs" class="flex flex-wrap gap-1 border-b border-slate-800"></nav>
  <main id="panel" class="min-h-[24rem]"></main>

  <footer class="text-xs text-slate-600 pb-6">
    <span id="static-note" class="hidden">
      Static snapshot written by <code>ccc insights --html</code>. The figures are from the
      commit it was generated on; run <code>ccc serve --html</code> locally for a live view.
    </span>
    <span id="live-note">served by <code>ccc serve --html</code> &middot; data at <code>/insights.json</code></span>
  </footer>
</div>

<script id="ccc-data" type="application/json">__CCC_EMBED__</script>
<script>
// Present only in a statically-exported page; `null` when served live.
const EMBEDDED = JSON.parse(document.getElementById('ccc-data').textContent);
const STATIC = EMBEDDED !== null;
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const num = n => (n ?? 0).toLocaleString();
// scale a nanosecond duration to the unit a human would read it in
const dur = ns => {
  if (ns == null) return 'unknown time';
  if (ns >= 1e9) return (ns / 1e9).toFixed(2) + ' s';
  if (ns >= 1e6) return (ns / 1e6).toFixed(1) + ' ms';
  if (ns >= 1e3) return (ns / 1e3).toFixed(1) + ' \u00b5s';
  return Math.round(ns) + ' ns';
};
let R = null, TAB = 'overview';
// which flame group is open, and the service-explorer path
let FLAME_GROUP = 0, EXPLORE = [];

const TABS = [
  ['overview', 'Overview'],
  ['triggers', 'Test triggers'],
  ['flame',    'Call hierarchy'],
  ['hot',      'Hot paths'],
  ['tests',    'Test targets'],
  ['services', 'Service map'],
  ['lints',    'Language insights'],
  ['rules',    'What was checked'],
];

// The landing page: what each view is for, and why you would open it. `stat`
// is read live from the payload so the card says something about *this* map,
// not just what the feature does in general.
const GUIDE = [
  {
    tab: 'triggers', title: 'Test triggers', accent: 'emerald',
    what: 'Which tests must run for the changes on this branch - including uncommitted edits - and which are missing.',
    why: 'The question every commit, CI job and agent actually needs answered: what does this change put at risk? Tests are matched to changed functions through the call graph, so a change deep in the stack still surfaces the tests above it, and each language gets a runnable command.',
    stat: R => {
      const t = R.test_triggers || {};
      if (!t.available) return 'unavailable - open for the reason';
      const c = t.counts || {};
      return `${num(c.tests_to_run)} tests to run · ${num(c.gaps)} gaps · ${num(c.changed_functions)} functions changed`;
    },
  },
  {
    tab: 'flame', title: 'Call hierarchy', accent: 'indigo',
    what: 'A flame view of the static call tree from every entry point, with the frames that cross a service boundary ringed.',
    why: 'Shows how deep your program actually goes. A call at the top of a long chain drags everything below it along, which is why two functions that look alike can cost wildly different amounts - and the ringed frames are where a change stops being local to your service.',
    stat: R => `${num((R.totals || {}).roots)} entry points · ${num((R.totals || {}).edges)} resolved edges`,
  },
  {
    tab: 'hot', title: 'Hot paths', accent: 'sky',
    what: 'Most depended-on functions, deepest call chains, widest fan-out, and recursion cycles.',
    why: 'Tells you where a breaking change would spread furthest. The function 40 things call is the one worth being careful with. Structural, not measured - it ranks by call-graph shape, not by execution frequency.',
    stat: R => {
      const top = ((R.hot || {}).most_called || [])[0];
      return top ? `${top.name} leads with ${num(top.callers)} callers` : 'no call graph resolved';
    },
  },
  {
    tab: 'tests', title: 'Test targets', accent: 'emerald',
    what: 'Which functions have no test mentioning them, and which kind of test each one warrants.',
    why: 'Turns "we need more tests" into a ranked list with a reason attached. Spends the effort where the structural risk is, and tells you whether the gap wants a smoke, integration, contract, performance or load test rather than leaving you to guess.',
    stat: R => {
      const s = (R.test_targets || {}).summary || {};
      return `${num(s.untested)} of ${num(s.functions)} functions untested`;
    },
  },
  {
    tab: 'services', title: 'Service map', accent: 'violet',
    what: 'Your services, the calls between them, and an explorer you click down from the top of the dependency graph.',
    why: 'Answers "if I change this, who finds out?" at the boundary level. Drilling into a hop shows the exact calls that carry it, so a dependency stops being an arrow on a diagram and becomes a list of functions you can go and read.',
    stat: R => {
      const s = R.services || {};
      return `${num((s.services || []).length)} services · ${num((s.edges || []).length)} edges`;
    },
  },
  {
    tab: 'lints', title: 'Language insights', accent: 'amber',
    what: 'Per-language heuristics: unreleased resources, deep loop nests, unrollable loops, inlining candidates, dead code.',
    why: 'Catches the things that are cheap to see in the syntax tree and expensive to find by reading - a malloc with no free on one path, three nested loops in a function nobody profiles. Every finding cites the measurement behind it so you can check it in seconds.',
    stat: R => {
      const f = ((R.lints || {}).findings || []);
      return `${num(f.filter(x => x.severity === 'warn').length)} warnings · ${num(f.length)} findings`;
    },
  },
  {
    tab: 'rules', title: 'What was checked', accent: 'rose',
    what: 'Every rule, the evidence it uses, and what it cannot know.',
    why: 'The limits matter as much as the findings. ccc reads a syntax tree - there is no type inference, data flow or runtime profile behind any of this. Read this before acting on anything above.',
    stat: R => `${num(((R.lints || {}).rules || []).length)} rules documented`,
  },
];

const ACCENT = {
  indigo: 'border-indigo-500/40 hover:border-indigo-400 text-indigo-300',
  sky: 'border-sky-500/40 hover:border-sky-400 text-sky-300',
  emerald: 'border-emerald-500/40 hover:border-emerald-400 text-emerald-300',
  violet: 'border-violet-500/40 hover:border-violet-400 text-violet-300',
  amber: 'border-amber-500/40 hover:border-amber-400 text-amber-300',
  rose: 'border-rose-500/40 hover:border-rose-400 text-rose-300',
};

function tabOverview() {
  const cards = GUIDE.map(g => {
    let stat = '';
    try { stat = g.stat(R) || ''; } catch (e) { stat = ''; }
    return `
    <button data-open="${g.tab}" class="text-left rounded-2xl border bg-slate-900/40 hover:bg-slate-900/70 p-5 space-y-2 transition ${ACCENT[g.accent]}">
      <div class="flex items-baseline gap-2">
        <h3 class="text-lg font-semibold">${esc(g.title)}</h3>
        <span class="ml-auto text-xs opacity-70">open →</span>
      </div>
      <p class="text-sm text-slate-300">${esc(g.what)}</p>
      <p class="text-sm text-slate-400"><span class="text-slate-500">Why it helps:</span> ${esc(g.why)}</p>
      <p class="text-xs font-mono opacity-80 pt-1">${esc(stat)}</p>
    </button>`;
  }).join('');

  setTimeout(() => {
    for (const b of document.querySelectorAll('[data-open]')) {
      b.onclick = () => { TAB = b.dataset.open; location.hash = TAB; render(); };
    }
  }, 0);

  return `<div class="grid gap-4 md:grid-cols-2">${cards}</div>
    <p class="text-xs text-slate-500 mt-4">
      Every view is derived from the syntax tree - the call graph, per-function measurements and your service globs.
      Nothing here runs your code, so treat the findings as advisory, not proofs.
    </p>`;
}

// one tone per recommended test kind, reused by the badge and the filter
const KIND_TONE = {
  'smoke-test':       'text-sky-300 border-sky-500/40 bg-sky-500/10',
  'integration-test': 'text-indigo-300 border-indigo-500/40 bg-indigo-500/10',
  'contract-test':    'text-violet-300 border-violet-500/40 bg-violet-500/10',
  'perf-test': 'text-amber-300 border-amber-500/40 bg-amber-500/10',
  'load-test':        'text-rose-300 border-rose-500/40 bg-rose-500/10',
};
const kindBadge = k => `<span class="inline-block whitespace-nowrap text-[11px] px-1.5 py-0.5 rounded border ${KIND_TONE[k] || KIND_TONE['smoke-test']}">${esc(k)}</span>`;
// A service name, styled as a chip wherever one appears. With no
// `.ccc/map.json`, changes calls the implicit whole-root service `.`, which
// renders as a bare dot in a pill and means nothing to a reader - name it.
const svcLabel = s => (s === '.' ? 'whole project' : s);
const chip = s => `<span class="inline-block whitespace-nowrap px-2 py-0.5 rounded-full text-xs font-medium bg-indigo-500/15 text-indigo-300 border border-indigo-500/30">${esc(svcLabel(s))}</span>`;

const card = (title, body, note) => `
  <section class="rounded-xl border border-slate-800 bg-slate-900/40 p-4 space-y-3">
    <h3 class="text-xs font-semibold uppercase tracking-wider text-slate-400">${esc(title)}</h3>
    ${note ? `<p class="text-xs text-slate-500">${esc(note)}</p>` : ''}
    ${body}
  </section>`;

const loc = (f, l) => `<code class="text-xs text-slate-500">${esc(f)}:${l}</code>`;

// `narrow` names columns that hold a single badge or number: they size to their
// content so the badge never has to wrap, and the prose columns take the slack.
const table = (cols, rows, narrow = []) => {
  const isNarrow = i => narrow.includes(i) || narrow.includes(cols[i]);
  const w = i => isNarrow(i) ? ' w-px whitespace-nowrap' : '';
  return rows.length ? `
  <div class="overflow-x-auto"><table class="w-full text-sm">
    <thead class="text-left text-xs text-slate-500"><tr>${cols.map((c, i) => `<th class="px-2 py-1 font-medium${w(i)}">${esc(c)}</th>`).join('')}</tr></thead>
    <tbody>${rows.map(r => `<tr class="border-t border-slate-800/70">${r.map((c, i) => `<td class="px-2 py-1 align-top${w(i)}">${c}</td>`).join('')}</tr>`).join('')}</tbody>
  </table></div>` : '<p class="text-sm text-slate-500">nothing to show</p>';
};

// ---- flame: nested bars whose width is the child's share of its parent ----
// Static call tree, so width is reachable call sites and not sampled time.
const HUES = ['bg-indigo-500/70','bg-sky-500/70','bg-emerald-500/70','bg-amber-500/70','bg-rose-500/70','bg-fuchsia-500/70','bg-teal-500/70','bg-violet-500/70'];

function frame(node, depth, share) {
  const kids = node.children || [];
  const total = node.value || 1;
  const svc = node.service ? ` · ${node.service}` : '';
  const label = `${node.name} · ${node.value} frame(s) · cx ${node.complexity}${svc}`;
  const tone = HUES[depth % HUES.length];
  const rec = node.recursive ? '<span class="text-[10px] text-amber-300 ml-1">↺</span>' : '';
  // a frame the call reached by leaving its caller's service: the boundary a
  // change has to cross to become someone else's problem
  const cross = node.crosses
    ? ' ring-2 ring-amber-300 ring-offset-1 ring-offset-slate-950'
    : '';
  const crossMark = node.crosses
    ? `<span class="text-[10px] text-slate-900 font-bold ml-1">↳${esc(node.service || '')}</span>`
    : '';
  const title = `${node.file}:${node.line} — ${label}` +
    (node.crosses ? ` (crosses into ${node.service})` : '');
  return `
    <div style="width:${share}%" class="min-w-0">
      <div title="${esc(title)}"
           class="${tone}${cross} hover:brightness-125 cursor-default rounded-sm px-1 mb-0.5 text-[11px] leading-4 truncate text-slate-950 font-medium">
        ${esc(node.name)}${crossMark}${rec}
      </div>
      <div class="flex gap-0.5">
        ${kids.map(k => frame(k, depth + 1, (k.value / total) * 100)).join('')}
      </div>
    </div>`;
}

function tabFlame() {
  const f = R.flame || {};
  const groups = f.groups || [];
  if (!groups.length) return card('Call hierarchy', '<p class="text-sm text-slate-500">no call trees</p>');
  if (FLAME_GROUP >= groups.length) FLAME_GROUP = 0;
  const g = groups[FLAME_GROUP];

  // one tab per service that declares deps in map.json, plus the whole map
  const picker = `
    <div class="flex flex-wrap gap-1 mb-1">
      ${groups.map((x, i) => `
        <button data-flame="${i}" class="px-2 py-1 rounded text-xs border ${
          i === FLAME_GROUP
            ? 'border-indigo-400 text-slate-100 bg-indigo-500/10'
            : 'border-slate-800 text-slate-500 hover:text-slate-300'
        }">${esc(x.service)}${(x.declares || []).length ? `<span class="text-slate-600 ml-1">→${x.declares.map(esc).join(',')}</span>` : ''}</button>`).join('')}
    </div>
    ${f.groups_truncated ? '<p class="text-xs text-amber-400">More services declare deps than are drawn here.</p>' : ''}`;

  const roots = (g.roots || []).filter(r => (r.value || 1) > 1);
  const shown = roots.slice(0, 40);
  const body = shown.length
    ? `<div class="space-y-3">${shown.map(r => `
        <div class="rounded-lg border border-slate-800 bg-slate-950/60 p-2">
          <div class="flex items-baseline gap-2 mb-1">
            <span class="font-mono text-xs text-slate-300">${esc(r.name)}</span>
            ${loc(r.file, r.line)}
            <span class="text-[11px] text-slate-500">${r.value} frames deep-reachable</span>
          </div>
          ${frame(r, 0, 100)}
        </div>`).join('')}</div>`
    : `<p class="text-sm text-slate-500">no multi-frame call trees for <span class="font-mono">${esc(g.service)}</span> - its entry points may all be leaves, or calls could not be resolved with evidence</p>`;

  const cut = g.truncated ? ' Output was capped; deeper frames are not shown.' : '';
  const legend = '<span class="inline-block align-middle w-3 h-3 rounded-sm bg-indigo-500/70 ring-2 ring-amber-300 mr-2"></span> ringed frames are calls that cross into another service.';
  setTimeout(() => {
    for (const b of document.querySelectorAll('[data-flame]')) {
      b.onclick = () => { FLAME_GROUP = +b.dataset.flame; render(); };
    }
  }, 0);
  return card('Call hierarchy (static flame)', picker + body, (f.note || '') + cut)
    + `<p class="text-xs text-slate-500 mt-2">${legend}</p>`
    + (roots.length > shown.length ? `<p class="text-xs text-slate-500">${roots.length - shown.length} more entry point(s) not drawn.</p>` : '');
}

function tabHot() {
  const h = R.hot || {};
  const fnRow = r => [
    `<span class="font-mono text-slate-200">${esc(r.name)}</span>${r.test ? ' <span class="text-[10px] text-slate-600">test</span>' : ''}`,
    loc(r.file, r.line),
    num(r.callers), num(r.call_sites), num(r.calls), num(r.complexity),
    r.loop_depth ? `<span class="text-amber-300">${r.loop_depth}</span>` : '<span class="text-slate-600">0</span>',
  ];
  const cols = ['function', 'where', 'callers', 'call sites', 'calls out', 'complexity', 'loop depth'];
  const chains = (h.deepest_chains || []).map(c => `
    <div class="text-xs font-mono text-slate-400 py-1 border-t border-slate-800/70">
      <span class="text-slate-600">${c.depth} deep · ${c.call_sites} call site(s)</span><br>
      ${(c.chain || []).map(s => `<span title="${esc(s.file)}:${s.line}" class="text-slate-300">${esc(s.name)}</span>`).join(' <span class="text-slate-600">→</span> ')}
    </div>`).join('');
  const cyc = (h.cycles || []).map(c => `
    <div class="text-xs font-mono text-amber-300/90 py-1">${c.size} mutually recursive:
      ${(c.members || []).map(m => esc(m.name)).join(', ')}</div>`).join('')
    || '<p class="text-sm text-slate-500">no call cycles</p>';

  return [
    card('Most depended on', table(cols, (h.most_called || []).map(fnRow)), h.note),
    card('Deepest call chains', chains || '<p class="text-sm text-slate-500">no chains longer than one frame</p>',
         'A change at the end of a long chain ripples the furthest.'),
    card('Widest fan-out', table(cols, (h.widest || []).map(fnRow))),
    card('Most decision points', table(cols, (h.most_complex || []).map(fnRow))),
    card('Recursion cycles', cyc),
  ].join('<div class="h-3"></div>');
}

function tabServices() {
  const s = R.services || {};
  const svc = (s.services || []).map(x => [
    `<span class="font-mono text-indigo-300">${esc(x.name)}</span>`,
    `<code class="text-xs text-slate-500">${(x.globs || []).map(esc).join(', ')}</code>`,
    num(x.files), num(x.funcs),
  ]);
  // detected and declared edges share one graph; the badge says which is which
  const edges = (s.edges || []).map(e => `
    <div class="flex flex-wrap items-center gap-2 py-1 text-sm">
      <span class="font-mono text-indigo-300">${esc(e.from)}</span>
      <span class="text-slate-600">→</span>
      <span class="font-mono text-emerald-300">${esc(e.to)}</span>
      ${edgeBadges(e)}
      <span class="text-xs text-slate-500">${(e.symbols || []).map(esc).join(', ')}${e.count > (e.symbols || []).length ? ` +${e.count - e.symbols.length}` : ''}</span>
    </div>`).join('') || '<p class="text-sm text-slate-500">no cross-service edges</p>';
  const orphans = s.unassigned_files || [];
  const orphanCard = orphans.length ? card('Files in no service',
    `<div class="text-xs font-mono text-amber-300/90 space-y-0.5">${orphans.map(esc).join('<br>')}</div>`,
    'These match none of the globs above, so they are missing from the service graph - widen a glob in .ccc/map.json to include them.') : '';

  return [
    exploreCard(s),
    card('Services', table(['service', 'globs', 'files', 'funcs'], svc), `grouped from ${s.source || 'unknown'}`),
    card('Service edges', edges,
      'Detected edges are rolled up from the call graph (import or qualifier evidence only). Declared edges come from `deps` in .ccc/map.json - the HTTP/RPC/queue links static analysis cannot see.'),
    orphanCard,
  ].join('<div class="h-3"></div>');
}

// Walk the dependency graph from the top down. Start at the services nothing
// depends on, then click through each hop to see the calls that carry it.
function exploreCard(s) {
  const edges = s.edges || [];
  const all = (s.services || []).map(x => x.name);
  const hasIncoming = new Set(edges.map(e => e.to));
  // the top of the graph: services nobody depends on. If everything is
  // depended on (a cycle), fall back to listing them all rather than nothing.
  let tops = all.filter(n => !hasIncoming.has(n));
  if (!tops.length) tops = all;

  const outOf = name => edges.filter(e => e.from === name);
  // separators only *between* crumbs, never after the "start:" label
  const crumbs = '<span class="text-slate-600">start:</span>' + EXPLORE.map((n, i) =>
    `${i ? '<span class="text-slate-600 mx-1">→</span>' : '<span class="mx-1"></span>'}` +
    `<button data-crumb="${i}" class="font-mono text-indigo-300 hover:underline">${esc(n)}</button>`
  ).join('');

  let body;
  if (!EXPLORE.length) {
    body = `<div class="flex flex-wrap gap-2">${tops.map(n =>
      `<button data-go="${esc(n)}" class="px-3 py-1.5 rounded-lg border border-slate-700 hover:border-indigo-400 font-mono text-sm text-indigo-300">${esc(n)}</button>`
    ).join('')}</div>
    <p class="text-xs text-slate-500">${tops.length === all.length && all.length > 1
      ? 'every service is depended on by another, so all of them are shown as entry points'
      : 'services nothing else depends on'}</p>`;
  } else {
    const cur = EXPLORE[EXPLORE.length - 1];
    const deps = outOf(cur);
    // the hop that got us here, and the calls that carry it
    const prev = EXPLORE.length > 1 ? EXPLORE[EXPLORE.length - 2] : null;
    const hop = prev ? edges.find(e => e.from === prev && e.to === cur) : null;
    const invoked = hop && (hop.sites || []).length
      ? `<div class="rounded-lg border border-slate-800 bg-slate-950/50 p-3 space-y-1">
           <p class="text-xs text-slate-400">what <span class="font-mono text-indigo-300">${esc(prev)}</span> invokes in <span class="font-mono text-emerald-300">${esc(cur)}</span></p>
           ${hop.sites.map(site => `
             <div class="text-xs font-mono flex flex-wrap gap-2 items-baseline border-t border-slate-800/60 pt-1">
               <span class="text-slate-200">${esc(site.symbol)}</span>
               <span class="text-slate-600">${esc(site.target_file)}:${site.target_line}</span>
               <span class="text-slate-600">called by ${esc(site.caller)} (${esc(site.caller_file)}:${site.caller_line})</span>
               ${(site.calls_on || []).length ? `<span class="w-full text-slate-500 pl-3">↳ then calls ${site.calls_on.map(c =>
                 `<span class="${c.service && c.service !== cur ? 'text-amber-300' : 'text-slate-400'}" title="${esc(c.file)}:${c.line}${c.service ? ' — ' + esc(c.service) : ''}">${esc(c.name)}</span>`
               ).join(', ')}</span>` : ''}
             </div>`).join('')}
         </div>`
      : (prev ? `<p class="text-xs text-slate-500">${hop && hop.declared
          ? 'Declared in <code>.ccc/map.json</code>. Dependency and type resolution still ran across this boundary and resolved no calls - the expected shape for an HTTP, RPC or queue link.'
          : 'no call sites recorded for this hop'}</p>` : '');

    const next = deps.length
      ? `<div class="space-y-1">
           <p class="text-xs text-slate-400"><span class="font-mono text-indigo-300">${esc(cur)}</span> depends on</p>
           ${deps.map(e => `
             <button data-go="${esc(e.to)}" class="w-full text-left rounded-lg border border-slate-800 hover:border-indigo-400 px-3 py-1.5">
               <span class="font-mono text-sm text-emerald-300">${esc(e.to)}</span>
               ${edgeBadges(e)}
               ${EXPLORE.includes(e.to) ? '<span class="text-[10px] text-amber-300 ml-1">↺ already on this path</span>' : ''}
               <span class="block text-xs text-slate-500 font-mono truncate">${(e.symbols || []).map(esc).join(', ') || 'no call sites'}</span>
             </button>`).join('')}
         </div>`
      : `<p class="text-sm text-slate-500"><span class="font-mono">${esc(cur)}</span> depends on nothing else - this is a leaf</p>`;
    body = invoked + '<div class="h-2"></div>' + next;
  }

  setTimeout(() => {
    for (const b of document.querySelectorAll('[data-go]')) {
      b.onclick = () => { EXPLORE.push(b.dataset.go); render(); };
    }
    for (const b of document.querySelectorAll('[data-crumb]')) {
      b.onclick = () => { EXPLORE = EXPLORE.slice(0, +b.dataset.crumb + 1); render(); };
    }
    const reset = document.getElementById('explore-reset');
    if (reset) reset.onclick = () => { EXPLORE = []; render(); };
  }, 0);

  return card('Explore',
    `<div class="flex flex-wrap items-baseline gap-2 text-sm">
       ${crumbs}
       ${EXPLORE.length ? '<button id="explore-reset" class="ml-auto text-xs text-slate-500 hover:text-slate-300">reset</button>' : ''}
     </div>
     <div class="h-2"></div>${body}`,
    'Click down the dependency graph from the top to see exactly which calls carry each hop.');
}

// `declared` and `detected` are independent facts, so they get a badge each.
// Rendering them as alternatives made a declared dependency look like one the
// analyser had skipped, when its calls had in fact been resolved.
const edgeBadges = e => {
  const pill = (t, tone, title) =>
    `<span title="${esc(title)}" class="inline-block whitespace-nowrap text-[10px] px-1.5 py-0.5 rounded-full ${tone}">${t}</span>`;
  const out = [];
  if (e.declared) out.push(pill('declared', 'bg-sky-500/15 text-sky-300 border border-sky-500/30',
    'listed under `deps` in .ccc/map.json'));
  if (e.detected) out.push(pill('detected', 'bg-emerald-500/15 text-emerald-300 border border-emerald-500/30',
    'calls resolved by static analysis'));
  if (e.declared && !e.detected) out.push(pill('no calls found', 'bg-slate-700/40 text-slate-400 border border-slate-600/40',
    'the analysis ran and resolved no calls across this boundary - expected for HTTP, RPC or queue links'));
  return out.join(' ');
};

const SEV = { warn: 'text-amber-300 border-amber-500/40 bg-amber-500/10', info: 'text-sky-300 border-sky-500/40 bg-sky-500/10' };

function tabLints() {
  const l = R.lints || {};
  const all = l.findings || [];
  const langs = [...new Set(all.map(f => f.language))].sort();
  const rules = [...new Set(all.map(f => f.rule))].sort();
  const sel = document.getElementById('lint-lang')?.value || '';
  const selRule = document.getElementById('lint-rule')?.value || '';
  const rows = all.filter(f => (!sel || f.language === sel) && (!selRule || f.rule === selRule));

  const controls = `
    <div class="flex flex-wrap gap-2 items-center text-xs">
      <select id="lint-lang" class="bg-slate-900 border border-slate-700 rounded px-2 py-1">
        <option value="">all languages</option>
        ${langs.map(x => `<option ${x === sel ? 'selected' : ''}>${esc(x)}</option>`).join('')}
      </select>
      <select id="lint-rule" class="bg-slate-900 border border-slate-700 rounded px-2 py-1">
        <option value="">all rules</option>
        ${rules.map(x => `<option ${x === selRule ? 'selected' : ''}>${esc(x)}</option>`).join('')}
      </select>
      <span class="text-slate-500">${rows.length} of ${all.length} finding(s)</span>
    </div>`;

  const list = rows.map(f => `
    <div class="border-t border-slate-800/70 py-2 flex flex-wrap items-baseline gap-2">
      <span class="inline-block whitespace-nowrap text-[11px] px-1.5 py-0.5 rounded border ${SEV[f.severity] || SEV.info}">${esc(f.rule)}</span>
      <span class="font-mono text-sm text-slate-200">${esc(f.function)}</span>
      ${loc(f.file, f.line)}
      <span class="text-[10px] text-slate-600">${esc(f.language)}</span>
      <div class="w-full text-xs text-slate-400">${esc(f.message)}</div>
      <div class="w-full text-xs text-slate-500 italic">${esc(f.hint)}</div>
    </div>`).join('') || '<p class="text-sm text-slate-500 pt-2">nothing flagged</p>';

  const cut = l.truncated ? ' Findings were capped - fix some and refresh to see the rest.' : '';
  const html = card('Language insights', controls + list, (l.note || '') + cut);
  setTimeout(() => {
    for (const id of ['lint-lang', 'lint-rule']) {
      const el = document.getElementById(id);
      if (el) el.onchange = () => render();
    }
  }, 0);
  return html;
}

function tabRules() {
  const rules = (R.lints || {}).rules || [];
  return card('What was checked, and what it cannot know',
    table(['rule', 'looks for', 'evidence', 'limits'], rules.map(r => [
      `<span class="inline-block whitespace-nowrap text-[11px] px-1.5 py-0.5 rounded border ${SEV[r.severity] || SEV.info}">${esc(r.rule)}</span>`,
      `<span class="text-slate-300">${esc(r.what)}</span>`,
      `<span class="text-slate-400 text-xs">${esc(r.evidence)}</span>`,
      `<span class="text-amber-300/80 text-xs">${esc(r.limits)}</span>`,
    ]), ['rule']),
    'Rules are language-aware: a rule with no pairs defined for a language simply never fires there.')
    + '<div class="h-3"></div>'
    + card('Languages in this map',
      table(['language', 'files', 'lines', 'functions', 'avg complexity'],
        (R.languages || []).map(l => [
          `<span class="font-mono text-indigo-300">${esc(l.language)}</span>`,
          num(l.files), num(l.lines), num(l.funcs), (l.avg_complexity || 0).toFixed(1),
        ]), ['language']));
}

// Where the analyser thinks a test is missing, and which kind it should be.
function tabTests() {
  const t = R.test_targets || {};
  const all = t.targets || [];
  const sum = t.summary || {};
  const kinds = [...new Set(all.map(x => x.kind))].sort();
  const langs = [...new Set(all.map(x => x.language))].sort();

  const selKind = document.getElementById('tt-kind')?.value || '';
  const selLang = document.getElementById('tt-lang')?.value || '';
  // the point of the tab is gaps, so untested is the default view
  const selCov = document.getElementById('tt-cov')?.value ?? 'untested';

  const rows = all.filter(x =>
    (!selKind || x.kind === selKind) &&
    (!selLang || x.language === selLang) &&
    (selCov === 'all' || (selCov === 'untested' ? !x.covered : x.covered)));

  const spread = Object.entries(sum.by_kind || {})
    .sort((a, b) => b[1] - a[1])
    .map(([k, v]) => `${kindBadge(k)}<span class="text-xs text-slate-500 mr-3 ml-1">${v}</span>`).join('');

  const controls = `
    <div class="flex flex-wrap gap-2 items-center text-xs">
      <select id="tt-cov" class="bg-slate-900 border border-slate-700 rounded px-2 py-1">
        <option value="untested" ${selCov === 'untested' ? 'selected' : ''}>no test mentions it</option>
        <option value="covered" ${selCov === 'covered' ? 'selected' : ''}>a test mentions it</option>
        <option value="all" ${selCov === 'all' ? 'selected' : ''}>all</option>
      </select>
      <select id="tt-kind" class="bg-slate-900 border border-slate-700 rounded px-2 py-1">
        <option value="">all kinds</option>
        ${kinds.map(k => `<option ${k === selKind ? 'selected' : ''}>${esc(k)}</option>`).join('')}
      </select>
      <select id="tt-lang" class="bg-slate-900 border border-slate-700 rounded px-2 py-1">
        <option value="">all languages</option>
        ${langs.map(l => `<option ${l === selLang ? 'selected' : ''}>${esc(l)}</option>`).join('')}
      </select>
      <span class="text-slate-500">${rows.length} of ${all.length} shown · ${sum.untested ?? 0} of ${sum.functions ?? 0} functions have no test mentioning them</span>
    </div>
    <div class="flex flex-wrap items-center mt-1">${spread}</div>`;

  const sig = s => [
    ['complexity', s.complexity], ['call depth', s.call_depth], ['loop depth', s.loop_depth],
    ['call sites', s.call_sites], ['call-outs', s.call_outs], ['callers', s.callers], ['lines', s.lines],
  ].map(([k, v]) => `<span class="text-[10px] text-slate-500 mr-2">${k} <span class="${v ? 'text-slate-300' : 'text-slate-600'}">${v}</span></span>`).join('')
    + (s.recursive ? '<span class="text-[10px] text-amber-300">recursive</span>' : '');

  const list = rows.map(x => `
    <div class="border-t border-slate-800/70 py-2 space-y-1">
      <div class="flex flex-wrap items-baseline gap-2">
        ${kindBadge(x.kind)}
        ${(x.also || []).map(k => `<span class="text-[10px] text-slate-500">+${esc(k)}</span>`).join('')}
        <span class="font-mono text-sm text-slate-200">${esc(x.function)}</span>
        ${loc(x.file, x.line)}
        ${x.service ? `<span class="text-[10px] text-indigo-300">${esc(svcLabel(x.service))}</span>` : ''}
        <span class="text-[10px] text-slate-600">${esc(x.language)}</span>
        <span class="ml-auto text-[10px] ${x.covered ? 'text-emerald-400' : 'text-rose-400'}">${
          x.covered ? '✓ mentioned by ' + (x.covered_by || []).slice(0, 3).map(esc).join(', ') : '✗ no test mentions it'}</span>
      </div>
      <div class="text-sm text-slate-300">${esc(x.suggest)}</div>
      ${(x.why || []).length ? `<div class="text-xs text-slate-500">because: ${
        x.why.map(w => `<span class="text-slate-400">${esc(w.detail)}</span>`).join(' · ')}</div>` : ''}
      ${(x.semantics || []).map(sm => `<div class="text-xs text-slate-500 italic">${esc(sm)}</div>`).join('')}
      <div>${sig(x.signals || {})}</div>
    </div>`).join('') || '<p class="text-sm text-slate-500 pt-2">nothing matches these filters</p>';

  setTimeout(() => {
    for (const id of ['tt-kind', 'tt-lang', 'tt-cov']) {
      const el = document.getElementById(id);
      if (el) el.onchange = () => render();
    }
  }, 0);

  const cut = t.truncated ? ' Only the highest-priority targets are listed.' : '';
  return card('Test targets', controls + list, (t.note || '') + cut)
    + '<div class="h-3"></div>'
    + card('How a kind is chosen',
        table(['kind', 'answers', 'chosen when', 'signals'], (t.rubric || []).map(r => [
          kindBadge(r.kind),
          `<span class="text-slate-300">${esc(r.for)}</span>`,
          `<span class="text-slate-400 text-xs">${esc(r.chosen_when)}</span>`,
          `<span class="text-slate-500 text-xs">${esc(r.signals)}</span>`,
        ]), ['kind']),
        'Each kind is scored from the measurements above and the strongest wins; anything close behind is listed as a secondary (+kind).');
}

// Which tests the current branch makes necessary. The most operational view
// here: an engineer, a CI job and an agent all want the same answer.
function tabTriggers() {
  const t = R.test_triggers || {};
  if (!t.available) {
    return card('Test triggers',
      `<p class="text-sm text-amber-400">${esc(t.reason || 'unavailable')}</p>
       <p class="text-xs text-slate-500 mt-2">${esc(t.hint || '')}</p>`,
      'Triggers diff this branch against its base, so they need git history.');
  }
  const c = t.counts || {};
  const dirty = t.uncommitted_files || [];

  const head = `
    <div class="flex flex-wrap items-baseline gap-3 text-xs">
      <span class="text-slate-400">base <span class="font-mono text-indigo-300">${esc(t.base)}</span></span>
      <span class="font-mono text-slate-600">${esc(String(t.base_sha).slice(0, 9))} → ${esc(String(t.head_sha).slice(0, 9))}</span>
      ${dirty.length
        ? `<span class="inline-block whitespace-nowrap text-[10px] px-1.5 py-0.5 rounded-full bg-amber-500/15 text-amber-300 border border-amber-500/30" title="${esc(dirty.slice(0, 20).join(', '))}">${dirty.length} uncommitted file(s) included</span>`
        : '<span class="inline-block whitespace-nowrap text-[10px] px-1.5 py-0.5 rounded-full bg-slate-700/40 text-slate-400 border border-slate-600/40">working tree clean</span>'}
      <span class="text-slate-500">${num(c.changed_functions)} function(s) changed in ${num(c.changed_files)} file(s)</span>
    </div>`;

  const cmds = (t.commands || []).map((x, i) => `
    <div class="rounded-lg border border-slate-800 bg-slate-950/60 p-3 space-y-1">
      <div class="flex items-baseline gap-2">
        <span class="inline-block whitespace-nowrap text-[10px] px-1.5 py-0.5 rounded border border-slate-700 text-slate-400">${esc(x.language)}</span>
        <span class="text-xs text-slate-500">selects ${num(x.selects)} test(s)</span>
        <button data-copy="${i}" class="ml-auto text-xs text-slate-500 hover:text-slate-200">copy</button>
      </div>
      <pre id="cmd-${i}" class="text-xs font-mono text-emerald-300 whitespace-pre-wrap break-all">${esc(x.command)}</pre>
      ${x.caveat ? `<p class="text-[11px] text-slate-500 italic">${esc(x.caveat)}</p>` : ''}
    </div>`).join('') || '<p class="text-sm text-slate-500">no runnable command - nothing to run</p>';

  const runRows = (t.run || []).map(r => `
    <div class="border-t border-slate-800/70 py-1.5 flex flex-wrap items-baseline gap-2">
      <span class="inline-block whitespace-nowrap text-[10px] px-1.5 py-0.5 rounded border ${
        r.distance === 0
          ? 'border-emerald-500/40 bg-emerald-500/10 text-emerald-300'
          : 'border-slate-600/40 bg-slate-700/30 text-slate-400'
      }">${r.distance === 0 ? 'direct' : r.distance + ' hop' + (r.distance > 1 ? 's' : '')}</span>
      <span class="font-mono text-sm text-slate-200">${esc(r.test)}</span>
      ${loc(r.file, r.line)}
      <span class="w-full text-xs text-slate-500">${esc(r.reason)}</span>
    </div>`).join('') || '<p class="text-sm text-slate-500 pt-2">no existing test reaches these changes</p>';

  // A gap cites a `test_targets` row by id rather than carrying its own copy
  // of the recommendation, so the row is looked up here. `must_keep` in the
  // analyser guarantees a cited row survived truncation.
  const byId = new Map(((R.test_targets || {}).targets || []).map(t => [t.id, t]));
  const addRows = (t.add || []).map(a => {
    const rec = byId.get(a.target) || {};
    // id is `<file>::<function>` - fall back to it if the row is missing
    const [file, fn] = String(a.target).split('::');
    return `
    <div class="border-t border-slate-800/70 py-1.5 space-y-0.5">
      <div class="flex flex-wrap items-baseline gap-2">
        ${kindBadge(rec.kind || 'smoke-test')}
        <span class="font-mono text-sm text-slate-200">${esc(rec.function || fn || '')}</span>
        ${loc(rec.file || file, (a.lines || [])[0])}
        ${rec.service ? chip(rec.service) : ''}
      </div>
      <div class="text-xs text-slate-400">${esc(rec.suggest
        || `Nothing exercises \`${fn}\`; cover it before this lands.`)}</div>
      ${(rec.why || []).length ? `<div class="text-[11px] text-slate-500">because: ${rec.why.map(w => esc(w.detail)).join(' · ')}</div>` : ''}
    </div>`;
  }).join('') || '<p class="text-sm text-slate-500 pt-2">every changed function is reached by a test</p>';

  setTimeout(() => {
    for (const b of document.querySelectorAll('[data-copy]')) {
      b.onclick = async () => {
        const pre = document.getElementById('cmd-' + b.dataset.copy);
        try { await navigator.clipboard.writeText(pre.textContent); b.textContent = 'copied'; }
        catch (e) { b.textContent = 'select manually'; }
        setTimeout(() => { b.textContent = 'copy'; }, 1500);
      };
    }
  }, 0);

  const suite = t.full_suite_advised
    ? `<p class="text-xs text-amber-300 mb-2">${num(c.tests_to_run)} of ${num(t.total_tests)} tests in the map trigger - at that share, running the whole suite is simpler and less fragile than a name filter.</p>`
    : `<p class="text-xs text-slate-500 mb-2">${num(c.tests_to_run)} of ${num(t.total_tests)} tests in the map trigger.</p>`;

  return [
    card('What changed', head +
      `<div class="flex flex-wrap gap-2 mt-2">${(t.services_to_test || []).map(chip).join(' ') || '<span class="text-xs text-slate-500">no services affected</span>'}</div>`,
      t.changed_note),
    card(`Run these tests (${num(c.tests_to_run)} · ${num(c.direct)} direct)`,
      suite + cmds + '<div class="h-2"></div>' + runRows, t.note),
    card(`Missing coverage (${num(c.gaps)})`, addRows,
      'Changed functions no test reaches, with the kind of test the signals justify. A CI gate can fail on this list.'),
  ].join('<div class="h-3"></div>');
}

const RENDER = { overview: tabOverview, triggers: tabTriggers, flame: tabFlame, hot: tabHot, tests: tabTests, services: tabServices, lints: tabLints, rules: tabRules };

function render() {
  document.getElementById('tabs').innerHTML = TABS.map(([id, label]) => `
    <button data-tab="${id}" class="px-3 py-2 text-sm border-b-2 -mb-px ${
      id === TAB ? 'border-indigo-400 text-slate-100' : 'border-transparent text-slate-500 hover:text-slate-300'
    }">${esc(label)}</button>`).join('');
  for (const b of document.querySelectorAll('#tabs button')) {
    b.onclick = () => { TAB = b.dataset.tab; location.hash = TAB; render(); };
  }
  document.getElementById('panel').innerHTML = (RENDER[TAB] || tabOverview)();
}

const tile = (label, value, tone) => `
  <div class="rounded-xl border border-slate-800 bg-slate-900/50 p-4">
    <div class="text-2xl font-semibold ${tone}">${value}</div>
    <div class="text-xs text-slate-500 mt-1">${esc(label)}</div>
  </div>`;

// `rescan` re-parses the project first, so refresh means "re-read the source",
// not just "re-render what the server already had". Without it the page would
// keep showing a stale map whenever the watcher is off or has not fired yet.
async function load(rescan) {
  const panel = document.getElementById('panel');
  const btn = document.getElementById('refresh');
  if (STATIC) {
    // a snapshot: the data is already here and there is no server to ask
    R = EMBEDDED;
  } else {
    if (btn) { btn.disabled = true; btn.textContent = rescan ? 'rescanning…' : 'loading…'; }
    panel.innerHTML = `<p class="text-sm text-slate-500">${rescan ? 're-reading the project…' : 'analysing…'}</p>`;
    try {
      if (rescan) {
        const r = await fetch('/refresh', { method: 'POST' });
        if (!r.ok) throw new Error(`rescan failed: server said ${r.status}`);
      }
      const res = await fetch('/insights.json', { headers: { 'Accept': 'application/json' } });
      if (!res.ok) throw new Error(`server said ${res.status}`);
      R = await res.json();
    } catch (e) {
      panel.innerHTML = `<p class="text-rose-400 text-sm">could not refresh — ${esc(e.message)}</p>`;
      return;
    } finally {
      if (btn) { btn.disabled = false; btn.textContent = 'refresh'; }
    }
  }
  const t = R.totals || {};
  const warns = ((R.lints || {}).findings || []).filter(f => f.severity === 'warn').length;
  document.getElementById('meta').textContent =
    `${R.schema} · generated in ${dur(R.took_ns)} on ${R.generated}`;
  document.getElementById('tiles').innerHTML =
    tile('files', num(t.files), 'text-slate-100') +
    tile('lines of code', num(t.lines), 'text-slate-100') +
    tile('functions', num(t.functions), 'text-slate-100') +
    tile('resolved call edges', num(t.edges), 'text-indigo-300') +
    tile('entry points', num(t.roots), 'text-emerald-300') +
    tile('warnings', num(warns), warns ? 'text-amber-400' : 'text-emerald-400');
  render();
}

function tabFromHash() {
  const t = (location.hash || '#overview').slice(1);
  return RENDER[t] ? t : 'overview';
}
// deep links and the browser's back/forward buttons both move tabs
window.addEventListener('hashchange', () => { TAB = tabFromHash(); if (R) render(); });
TAB = tabFromHash();
// a static page has nothing to refresh against, so say what it is instead
if (STATIC) {
  const btn = document.getElementById('refresh');
  btn.remove();
  document.getElementById('static-note').classList.remove('hidden');
  document.getElementById('live-note').remove();
} else {
  document.getElementById('refresh').onclick = () => load(true);
}
load(false);
</script>
</body>
</html>
"####;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embeds_report_and_title() {
        let report = json!({
            "schema": "ccc-changes/1",
            "services_to_test": ["api", "lib"],
            "counts": {"untested": 1},
        });
        let html = render_changes_html(&report, "ccc-changes-rust");
        assert!(html.contains("<title>ccc-changes-rust</title>"));
        // the embedded block parses back to the same report
        let start = html.find(r#"<script id="ccc-report" type="application/json">"#).unwrap();
        let rest = &html[start..];
        let open = rest.find('>').unwrap() + 1;
        let close = rest.find("</script>").unwrap();
        let parsed: Value = serde_json::from_str(&rest[open..close]).unwrap();
        assert_eq!(parsed, report);
        // stack of record: tailwind + htmx + fragments wired
        assert!(html.contains("cdn.tailwindcss.com"));
        assert!(html.contains("htmx.org"));
        assert!(html.contains("/fragment/find"));
    }

    // A statically exported page has to stand on its own: the analysis is
    // inlined, and the controls that need a server are gone.
    #[test]
    fn static_insights_page_embeds_its_data_and_drops_the_server_controls() {
        let report = json!({
            "schema": "ccc-insights/v1",
            "totals": {"files": 2, "lines": 40},
            // a string that would end the script block if it were not escaped
            "root": "</script><script>alert(1)</script>",
        });
        let html = render_insights_html("demo", Some(&report));

        // the payload round-trips out of the inline block
        let start = html.find(r#"<script id="ccc-data" type="application/json">"#).unwrap();
        let rest = &html[start..];
        let open = rest.find('>').unwrap() + 1;
        let close = rest.find("</script>").unwrap();
        let parsed: Value = serde_json::from_str(&rest[open..close]).unwrap();
        assert_eq!(parsed, report);
        // and the breakout attempt never appears raw inside it
        assert!(!rest[open..close].contains("</script>"));

        // served live, the block is empty and the page fetches instead
        let live = render_insights_html("demo", None);
        assert!(live.contains(r#"<script id="ccc-data" type="application/json">null</script>"#));
        assert!(live.contains("/insights.json"));
    }

    // Both pages render service names
    #[test]
    fn every_template_labels_the_implicit_root_service() {
        for (name, tpl) in [("changes", TEMPLATE), ("insights", INSIGHTS_TEMPLATE)] {
            assert!(
                tpl.contains("const svcLabel"),
                "{name} template renders service names without svcLabel"
            );
            // the un-labelled chip body: a service name escaped straight in
            assert!(
                !tpl.contains("${esc(s)}</span>`;"),
                "{name} template has a chip that skips svcLabel"
            );
        }
    }

    #[test]
    fn script_breakout_is_defused() {
        let report = json!({"root": "</script><script>alert(1)</script>"});
        let html = render_changes_html(&report, "t");
        // the raw close tag from the data must not appear inside the JSON block
        let start = html.find(r#"<script id="ccc-report""#).unwrap();
        let rest = &html[start..];
        let close = rest.find("</script>").unwrap();
        let block = &rest[..close];
        assert!(!block.contains("</script>"));
        assert!(block.contains(r"<\/script>"));
        // and it still round-trips
        let open = rest.find('>').unwrap() + 1;
        let parsed: Value = serde_json::from_str(&rest[open..close]).unwrap();
        assert_eq!(parsed["root"], "</script><script>alert(1)</script>");
    }
}

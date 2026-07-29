//! single-file HTML view of a `ccc surf` report.
//!
//! The generated page embeds the report JSON verbatim, renders it with
//! Tailwind (CDN), and carries an HTMX-powered "live query" panel that talks
//! to a running `ccc serve` instance via its `/fragment/*` endpoints - so the
//! same file both *views* the report and *queries* the live map.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

// render the single-file report page. `title` names the report (by
// convention the output file stem, e.g. `ccc-surf-rust`).
pub fn render_surf_html(report: &Value, title: &str) -> String {
    let json = serde_json::to_string(report)
        .unwrap_or_else(|_| "{}".into())
        // keep the inline <script type="application/json"> block unbreakable:
        // `<\/` is a legal JSON escape and defuses any `</script` in strings
        .replace("</", "<\\/");
    TEMPLATE
        .replace("__CCC_TITLE__", &esc(title))
        .replace("__CCC_REPORT__", &json)
}

pub fn write_surf_html(path: &Path, report: &Value, title: &str) -> Result<()> {
    std::fs::write(path, render_surf_html(report, title))
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
      <h1 class="text-2xl font-semibold tracking-tight text-slate-50">ccc surf</h1>
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

  <footer class="text-xs text-slate-600 pb-4">generated by <code>ccc surf --html</code> &middot; report embedded below &middot; live panel needs <code>ccc serve</code></footer>
</div>

<script id="ccc-report" type="application/json">__CCC_REPORT__</script>
<script>
const R = JSON.parse(document.getElementById('ccc-report').textContent);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const PALETTE = ['indigo','emerald','sky','fuchsia','amber','rose','teal','violet'];
const svcColor = {};
(R.services || []).forEach((s, i) => { svcColor[s] = PALETTE[i % PALETTE.length]; });
const chip = s => {
  const c = svcColor[s] || 'slate';
  return `<span class="px-2 py-0.5 rounded-full text-xs font-medium bg-${c}-500/15 text-${c}-300 border border-${c}-500/30">${esc(s)}</span>`;
};
const codechip = (t, title) =>
  `<code title="${esc(title || '')}" class="px-1.5 py-0.5 rounded bg-slate-800 text-slate-300 text-xs">${esc(t)}</code>`;

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
  .map(i => `<div class="text-sm text-slate-400 font-mono">${(i.path || []).map(esc).join(' <span class="text-slate-600">→</span> ')} <span class="text-xs text-slate-500">(${esc(i.reason)})</span></div>`)
  .join('');

// edges
document.getElementById('edges').innerHTML = (R.edges || []).map(e => `
  <div class="flex flex-wrap items-center gap-2 rounded-xl border border-slate-800 bg-slate-900/50 px-4 py-2">
    ${chip(e.from)} <span class="text-slate-600">→</span> ${chip(e.to)}
    <span class="text-xs px-2 py-0.5 rounded-full ${e.declared ? 'bg-sky-500/15 text-sky-300 border border-sky-500/30' : 'bg-emerald-500/15 text-emerald-300 border border-emerald-500/30'}">${e.declared ? 'declared' : 'detected'}</span>
    <span class="flex flex-wrap gap-1">${(e.symbols || []).map(s => codechip(s.symbol, s.file + ':' + s.line)).join('')}</span>
  </div>`).join('') || '<p class="text-sm text-slate-500">no cross-service edges</p>';

// changed functions
const fnRows = (R.changed_functions || []).map(f => `
  <tr class="border-t border-slate-800/70">
    <td class="px-3 py-1.5 font-mono text-slate-200">${esc(f.function)}</td>
    <td class="px-3 py-1.5 font-mono text-slate-400">${esc(f.file)}<span class="text-slate-600">:${(f.lines || [])[0]}-${(f.lines || [])[1]}</span></td>
    <td class="px-3 py-1.5">${(f.services || []).map(chip).join(' ')}</td>
    <td class="px-3 py-1.5">${f.tested ? '<span class="text-emerald-400">✓ tested</span>' : '<span class="text-rose-400">✗ untested</span>'}</td>
    <td class="px-3 py-1.5">${(f.called_from || []).map(chip).join(' ') || '<span class="text-slate-600">-</span>'}</td>
  </tr>`).join('');
document.getElementById('changed-functions').innerHTML = fnRows
  ? `<table class="w-full text-sm"><thead class="bg-slate-900 text-left text-xs text-slate-500">
      <tr><th class="px-3 py-2">function</th><th class="px-3 py-2">where</th><th class="px-3 py-2">services</th><th class="px-3 py-2">tests</th><th class="px-3 py-2">called from</th></tr>
     </thead><tbody>${fnRows}</tbody></table>`
  : '<p class="text-sm text-slate-500 p-3">no function-level changes</p>';

// untested
if ((R.untested || []).length) {
  document.getElementById('untested-section').classList.remove('hidden');
  document.getElementById('untested').innerHTML = R.untested.map(f => `
    <div class="text-sm font-mono">${esc(f.function)} <span class="text-slate-500">${esc(f.file)}:${(f.lines || [])[0]}</span>
      ${(f.called_from || []).length ? '<span class="text-xs text-rose-300">← called from ' + f.called_from.map(esc).join(', ') + '</span>' : ''}
    </div>`).join('');
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embeds_report_and_title() {
        let report = json!({
            "schema": "ccc-surf/1",
            "services_to_test": ["api", "lib"],
            "counts": {"untested": 1},
        });
        let html = render_surf_html(&report, "ccc-surf-rust");
        assert!(html.contains("<title>ccc-surf-rust</title>"));
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

    #[test]
    fn script_breakout_is_defused() {
        let report = json!({"root": "</script><script>alert(1)</script>"});
        let html = render_surf_html(&report, "t");
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

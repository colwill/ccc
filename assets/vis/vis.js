// ccc vis - the architecture visualiser behind `ccc run --vis`.
//
// Five levels, each read from the server's map: the system and what it talks
// to, its containers, one container's components, one component's code, and
// one function's logic drawn as a node graph. The page knows no language -
// every level arrives language-agnostic from /vis.json, /vis/code and
// /vis/flow - and keeps nothing the server does not, beyond where you dragged
// things and which toggles you left on.

(() => {
  'use strict';

  // ------------------------------------------------------------- helpers

  const $ = id => document.getElementById(id);

  function h(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }

  function sv(tag, attrs) {
    const e = document.createElementNS('http://www.w3.org/2000/svg', tag);
    for (const [k, v] of Object.entries(attrs || {})) if (v != null) e.setAttribute(k, v);
    return e;
  }

  // browser storage can be missing or refuse writes; nothing depends on it
  const store = {
    get(key, fallback) {
      try {
        const v = localStorage.getItem('ccc-vis:' + key);
        return v == null ? fallback : JSON.parse(v);
      } catch {
        return fallback;
      }
    },
    set(key, value) {
      try {
        localStorage.setItem('ccc-vis:' + key, JSON.stringify(value));
      } catch {
        // not kept - the page still works
      }
    },
  };

  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
  const base = p => p.split('/').pop();

  function fmt(n) {
    if (n >= 100000) return Math.round(n / 1000) + 'k';
    if (n >= 10000) return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'k';
    return String(n);
  }

  const plural = (n, one, many) => `${fmt(n)} ${n === 1 ? one : many || one + 's'}`;

  // the complexity band `model::FuncMetrics::complexity_score` draws
  function band(c) {
    if (c <= 1) return 1;
    if (c <= 3) return c;
    if (c <= 5) return 4;
    if (c <= 7) return 5;
    if (c <= 10) return 6;
    if (c <= 15) return 7;
    if (c <= 20) return 8;
    if (c <= 30) return 9;
    return 10;
  }

  const LANGS = {
    rust: ['Rust', 'var(--orange)'],
    python: ['Python', 'var(--blue)'],
    javascript: ['JavaScript', 'var(--yellow)'],
    typescript: ['TypeScript', 'var(--teal)'],
    tsx: ['TSX', 'var(--teal)'],
    go: ['Go', 'var(--green)'],
    c: ['C', 'var(--purple)'],
    cpp: ['C++', 'var(--purple)'],
    csharp: ['C#', 'var(--pink)'],
    zig: ['Zig', 'var(--red)'],
    odin: ['Odin', 'var(--slate)'],
    proto: ['Protobuf', 'var(--grey)'],
  };
  const langName = l => (LANGS[l] || [l])[0];
  const langColour = l => (LANGS[l] || [null, 'var(--grey)'])[1];

  // a stable colour per type name, for data pins
  const PIN_COLOURS = ['var(--blue)', 'var(--teal)', 'var(--green)', 'var(--orange)', 'var(--pink)', 'var(--purple)', 'var(--yellow)', 'var(--red)'];
  function typeColour(t) {
    let x = 0;
    for (const ch of String(t)) x = (x * 31 + ch.charCodeAt(0)) >>> 0;
    return PIN_COLOURS[x % PIN_COLOURS.length];
  }

  async function getJSON(url) {
    const r = await fetch(url, { cache: 'no-store' });
    let body = null;
    try {
      body = await r.json();
    } catch {
      // not json - the status says enough
    }
    if (!r.ok) throw new Error((body && body.error) || `${r.status} ${r.statusText}`);
    return body;
  }

  // ------------------------------------------------------------- state

  const S = {
    overview: null,
    generated: null,
    route: null,
    scene: null,
    byId: new Map(),
    selected: null,
    zoom: 1,
    panX: 0,
    panY: 0,
    // library calls in a flow: folded into one node, drawn one by one, or left out
    lib: store.get('lib', 'compact'),
    tests: store.get('tests', false),
    // light unless the viewer chose otherwise; `auto` follows the system
    theme: store.get('theme', 'light'),
  };

  const LEVELS = {
    system: 'Context',
    containers: 'Containers',
    components: 'Components',
    code: 'Code',
    flow: 'Logic',
  };

  // ------------------------------------------------------------- routing

  function parseRoute() {
    const parts = location.hash.replace(/^#\/?/, '').split('/');
    const arg = parts[1] != null ? decodeURIComponent(parts.slice(1).join('/')) : '';
    switch (parts[0]) {
      case 'containers':
        return { level: 'containers' };
      case 'container':
        return { level: 'components', container: arg };
      case 'code':
        return { level: 'code', file: arg };
      case 'flow': {
        const m = arg.match(/^(.*)@(\d+)(?:@(.*))?$/);
        return m ? { level: 'flow', file: m[1], line: +m[2], name: m[3] || null } : { level: 'system' };
      }
      default:
        return { level: 'system' };
    }
  }

  function href(r) {
    switch (r.level) {
      case 'containers':
        return '#/containers';
      case 'components':
        return '#/container/' + encodeURIComponent(r.container);
      case 'code':
        return '#/code/' + encodeURIComponent(r.file);
      case 'flow':
        return '#/flow/' + encodeURIComponent(`${r.file}@${r.line}${r.name ? '@' + r.name : ''}`);
      default:
        return '#/';
    }
  }

  function go(r) {
    const target = href(r);
    if (location.hash === target) render(false);
    else location.hash = target;
  }

  const containerOf = file => (S.overview?.components.find(c => c.id === file) || {}).container;

  function up() {
    const r = S.route;
    if (!r) return;
    if (r.level === 'flow') go({ level: 'code', file: r.file });
    else if (r.level === 'code') {
      const c = containerOf(r.file);
      go(c ? { level: 'components', container: c } : { level: 'containers' });
    } else if (r.level === 'components') go({ level: 'containers' });
    else if (r.level === 'containers') go({ level: 'system' });
  }

  // ------------------------------------------------------------- render

  let renderSeq = 0;

  async function render(keepView) {
    const seq = ++renderSeq;
    const route = parseRoute();
    const same = S.route && href(S.route) === href(route);
    S.route = route;
    try {
      if (!S.overview) {
        S.overview = await getJSON('/vis.json');
        S.generated = S.overview.generated;
      }
      let scene;
      if (route.level === 'containers') scene = sceneContainers();
      else if (route.level === 'components') scene = sceneComponents(route.container);
      else if (route.level === 'code') {
        scene = sceneCode(await getJSON('/vis/code?file=' + encodeURIComponent(route.file)));
      } else if (route.level === 'flow') {
        const q = `file=${encodeURIComponent(route.file)}&line=${route.line}` + (route.name ? '&name=' + encodeURIComponent(route.name) : '');
        scene = sceneFlow(await getJSON('/vis/flow?' + q));
      } else scene = sceneSystem();
      if (seq !== renderSeq) return; // a later navigation won
      mount(scene, keepView && same);
    } catch (e) {
      if (seq !== renderSeq) return;
      clearCanvas();
      showMessage('Could not draw this level', e.message);
    }
    crumbs();
  }

  // ------------------------------------------------------------- node builders

  // a C4 element, drawn the way the doc canvas draws a box
  function card({ what, name, tech, note, accent, langs, cls, badge }) {
    const n = h('div', 'node card' + (cls ? ' ' + cls : ''));
    if (accent) {
      n.style.setProperty('--accent', accent);
      n.append(h('i', 'stripe'));
    }
    const top = h('div', 'row');
    top.append(h('span', 'what', what));
    if (badge) top.append(badge);
    n.append(top, h('div', 'name', name));
    if (tech) n.append(h('div', 'tech', tech));
    if (note) n.append(h('div', 'note', note));
    if (langs && langs.length) {
      const bar = h('div', 'langs');
      const total = langs.reduce((a, l) => a + l.n, 0) || 1;
      for (const l of langs) {
        const seg = h('i');
        seg.style.width = (100 * l.n) / total + '%';
        seg.style.background = langColour(l.language);
        seg.title = `${langName(l.language)}: ${fmt(l.n)}`;
        bar.append(seg);
      }
      n.append(bar);
    }
    return n;
  }

  function badge(cls, text, title) {
    const b = h('span', 'badge ' + cls, text);
    if (title) b.title = title;
    return b;
  }

  const cxBadge = c => badge('cx' + band(c), String(c), `complexity ${c} - one path plus one per branch and loop`);

  function pinEl(p, side) {
    if (!p) {
      // keeps the two pin columns aligned row by row
      const gap = h('div', 'pin ' + side);
      gap.style.visibility = 'hidden';
      return gap;
    }
    const e = h('div', `pin ${side} ${p.data ? 'data' : p.more ? 'more' : 'exec'}`);
    if (p.pin) e.dataset.pin = p.pin;
    if (p.colour) e.style.setProperty('--pin', p.colour);
    if (p.title) e.title = p.title;
    e.append(h('i'), h('span', 'lbl', p.label || ''));
    return e;
  }

  // a node in the blueprint style: a header in the node's colour, then rows of
  // input pins on the left and output pins on the right
  function bp({ accent, k, t, q, badges, ins, outs, body, cls }) {
    const n = h('div', 'node bp' + (cls ? ' ' + cls : ''));
    n.style.setProperty('--accent', accent || 'var(--grey)');
    const head = h('div', 'head');
    if (k) head.append(h('span', 'k', k));
    head.append(h('span', 't', t));
    if (q) head.append(h('span', 'q', q));
    if (badges && badges.length) {
      const b = h('span', 'badges');
      b.append(...badges);
      head.append(b);
    }
    n.append(head);
    if (body) n.append(h('div', 'body', body));
    const rows = h('div', 'rows');
    const left = ins || [];
    const right = outs || [];
    for (let i = 0; i < Math.max(left.length, right.length); i++) rows.append(pinEl(left[i], 'in'), pinEl(right[i], 'out'));
    n.append(rows);
    return n;
  }

  // ------------------------------------------------------------- level 1: context

  function externalNodes(o) {
    const out = [];
    for (const p of o.peers || []) {
      out.push({
        id: 'peer:' + p.name,
        kind: 'peer',
        data: p,
        side: 'out',
        el: card({
          what: 'Peer repository',
          name: p.name,
          cls: 'external dashed',
          accent: 'var(--grey)',
          tech: p.language ? langName(p.language) : p.repo || '',
          note: p.resolved ? `provides ${p.provides} · consumes ${p.consumes}` : p.error ? `not resolved: ${p.error}` : 'not resolved',
        }),
      });
    }
    for (const g of o.outside || []) {
      const inbound = g.direction === 'in';
      out.push({
        id: g.id,
        kind: 'outside',
        data: g,
        side: inbound ? 'in' : 'out',
        el: card({
          what: inbound ? 'Callers outside this map' : 'Endpoints outside this map',
          name: g.transport === 'unspecified' ? 'unnamed transport' : g.transport.toUpperCase(),
          cls: 'external',
          accent: 'var(--grey)',
          note: `${plural(g.key_count, 'key')}: ${g.keys.join(', ')}${g.key_count > g.keys.length ? ', …' : ''}`,
        }),
      });
    }
    return out;
  }

  function systemCard(sys) {
    return card({
      what: 'Software system',
      name: sys.name,
      cls: 'system',
      accent: langColour(sys.languages[0] && sys.languages[0].language),
      tech: sys.languages.slice(0, 5).map(l => langName(l.language)).join(' · '),
      note: `${plural(sys.files, 'file')} · ${plural(sys.lines, 'line')}\n${plural(sys.functions, 'function')} · ${plural(sys.edges, 'resolved call')}`,
      langs: sys.languages.map(l => ({ language: l.language, n: l.lines })),
    });
  }

  function sceneSystem() {
    const o = S.overview;
    const nodes = [{ id: 'system', kind: 'system', data: o.system, el: systemCard(o.system), drill: { level: 'containers' } }];
    const edges = [];
    const ext = externalNodes(o);
    nodes.push(...ext);
    for (const x of ext) {
      if (x.kind === 'outside') {
        const label = `${x.data.transport} ×${x.data.key_count}`;
        edges.push(x.side === 'in' ? { from: x.id, to: 'system', label, data: x.data } : { from: 'system', to: x.id, label, data: x.data });
      }
    }
    // calls into a peer, whichever container makes them, are the system's
    const toPeer = new Map();
    for (const e of o.container_edges) {
      if (!e.external) continue;
      const t = toPeer.get(e.to) || { count: 0, transports: new Set(), declared: false, detected: false };
      t.count += e.count || 0;
      e.transports.forEach(x => t.transports.add(x));
      t.declared ||= e.declared;
      t.detected ||= e.detected;
      toPeer.set(e.to, t);
    }
    for (const [peer, t] of toPeer) {
      edges.push({
        from: 'system',
        to: 'peer:' + peer,
        label: t.detected ? [...t.transports].join(', ') || `calls ×${t.count}` : 'declared',
        cls: t.detected ? '' : 'declared',
        data: { from: o.system.name, to: peer, ...t, transports: [...t.transports] },
      });
    }
    return {
      key: 'system',
      kind: 'c4',
      layout: 'context',
      nodes,
      edges,
      note: ext.length
        ? null
        : 'Nothing in the map crosses this system’s boundary. Mark calls that leave the process with `ccc:calls <transport> <key>` and their handlers with `ccc:serves`, or name peer repositories under `externals` in .ccc/map.json.',
      legend: c4Legend(),
    };
  }

  // ------------------------------------------------------------- level 2: containers

  function sceneContainers() {
    const o = S.overview;
    const shown = o.containers.filter(c => S.tests || !(c.files > 0 && c.tests === c.files));
    const ids = new Set(shown.map(c => c.id));
    const nodes = shown.map(c => ({
      id: 'c:' + c.id,
      kind: 'container',
      data: c,
      drill: { level: 'components', container: c.id },
      el: card({
        what: 'Container',
        name: c.name === '(unassigned)' ? 'unassigned files' : c.name,
        cls: 'dashed',
        accent: langColour(c.languages[0] && c.languages[0].language),
        tech: c.languages.map(l => langName(l.language)).join(' · ') || 'no source files',
        note: `${plural(c.files, 'file')} · ${plural(c.funcs, 'function')}` + (c.entries ? `\n${plural(c.entries, 'entry point')}` : ''),
        langs: c.languages.map(l => ({ language: l.language, n: l.files })),
      }),
    }));
    const ext = externalNodes(o).map(x => ({ ...x, pin: x.side === 'in' ? 'first' : 'last' }));
    nodes.push(...ext);
    const extIds = new Set(ext.map(x => x.id));
    const edges = [];
    for (const e of o.container_edges) {
      const from = 'c:' + e.from;
      const to = e.external ? 'peer:' + e.to : 'c:' + e.to;
      if (!ids.has(e.from) || !(ids.has(e.to) || extIds.has(to))) continue;
      edges.push({
        from,
        to,
        label: e.detected ? (e.transports.length ? e.transports.join(', ') : `calls ×${e.count}`) : 'declared',
        title: e.symbols.join(', '),
        cls: e.detected ? '' : 'declared',
        data: e,
      });
    }
    for (const g of o.outside || []) {
      for (const c of g.containers) {
        if (!ids.has(c.container)) continue;
        const label = `${g.transport} ×${c.count}`;
        edges.push(g.direction === 'in' ? { from: g.id, to: 'c:' + c.container, label, data: g } : { from: 'c:' + c.container, to: g.id, label, data: g });
      }
    }
    return {
      key: 'containers',
      kind: 'c4',
      layout: 'layered',
      nodes,
      edges,
      persist: true,
      frames: [{ title: o.system.name, sub: 'software system', members: nodes.filter(n => n.kind === 'container').map(n => n.id) }],
      note:
        shown.length === 1
          ? `One container holds everything (${o.system.grouping}). Name your services with \`services\` in .ccc/map.json - \`ccc init\` writes a starter.`
          : `containers: ${o.system.grouping}`,
      legend: c4Legend(),
    };
  }

  // ------------------------------------------------------------- level 3: components

  function sceneComponents(name) {
    const o = S.overview;
    const container = o.containers.find(c => c.id === name);
    if (!container) throw new Error(`there is no container called “${name}” in the map`);
    const comps = o.components.filter(c => c.container === name && (S.tests || !c.test));
    const mine = new Set(comps.map(c => c.id));
    const where = new Map(o.components.map(c => [c.id, c.container]));
    const nodes = comps.map(c => ({
      id: 'm:' + c.id,
      kind: 'component',
      data: c,
      group: c.dir,
      drill: { level: 'code', file: c.id },
      el: card({
        what: c.dir || './',
        name: c.name,
        cls: c.test ? 'test' : '',
        accent: langColour(c.language),
        tech: langName(c.language) + (c.types ? ` · ${plural(c.types, 'type')}` : ''),
        note: `${plural(c.funcs, 'function')} · ${plural(c.lines, 'line')}` + (c.entries ? ` · ${plural(c.entries, 'entry')}` : ''),
        badge: c.max_complexity > 1 ? cxBadge(c.max_complexity) : null,
      }),
    }));
    const edges = [];
    // a call that leaves the container lands on the container it reaches, as C4 draws it
    const outer = new Map();
    const touch = (other, dir, e) => {
      const x = outer.get(other) || { name: other, calls: 0, callers: 0, symbols: new Set() };
      if (dir === 'out') x.calls += e.count;
      else x.callers += e.count;
      e.symbols.forEach(s => x.symbols.add(s));
      outer.set(other, x);
      return 'x:' + other;
    };
    const agg = new Map();
    for (const e of o.component_edges) {
      const a = mine.has(e.from);
      const b = mine.has(e.to);
      if (a && b) {
        edges.push({ from: 'm:' + e.from, to: 'm:' + e.to, label: `×${e.count}`, title: e.symbols.join(', '), data: e });
        continue;
      }
      if (!a && !b) continue;
      const other = where.get(a ? e.to : e.from);
      if (!other) continue;
      const ext = touch(other, a ? 'out' : 'in', e);
      const key = a ? `m:${e.from}>${ext}` : `${ext}>m:${e.to}`;
      const g = agg.get(key) || { from: a ? 'm:' + e.from : ext, to: a ? ext : 'm:' + e.to, count: 0, symbols: [] };
      g.count += e.count;
      g.symbols.push(...e.symbols);
      agg.set(key, g);
    }
    for (const g of agg.values()) edges.push({ from: g.from, to: g.to, label: `×${g.count}`, title: [...new Set(g.symbols)].join(', '), data: g });
    for (const x of outer.values()) {
      nodes.push({
        id: 'x:' + x.name,
        kind: 'outer',
        data: x,
        pin: x.calls ? 'last' : 'first',
        drill: { level: 'components', container: x.name },
        el: card({
          what: 'Container',
          name: x.name,
          cls: 'external dashed',
          accent: 'var(--grey)',
          note: [x.calls && `called ×${x.calls}`, x.callers && `calls in ×${x.callers}`].filter(Boolean).join(' · '),
        }),
      });
    }
    return {
      key: 'components:' + name,
      kind: 'c4',
      layout: 'layered',
      nodes,
      edges,
      persist: true,
      quietLabels: edges.length > 40,
      order: n => (n.group || '') + '/' + n.id,
      frames: [{ title: name, sub: 'container', members: nodes.filter(n => n.kind === 'component').map(n => n.id) }],
      empty: comps.length ? null : ['No components here', S.tests ? 'This container has no mapped files.' : 'Every file here is a test - switch tests on to see them.'],
      legend: c4Legend(),
    };
  }

  function c4Legend() {
    const langs = (S.overview.system.languages || []).map(l => [langName(l.language), langColour(l.language)]);
    return [['container', 'var(--grey)', 'dash'], ['outside', 'var(--grey)', 'ext'], ['calls', 'var(--edge)', 'line'], ...langs];
  }

  // ------------------------------------------------------------- level 4: code

  function fnAccent(f) {
    if (f.module) return 'var(--purple)';
    if (f.test) return 'var(--grey)';
    if (f.entry) return 'var(--red)';
    return 'var(--blue)';
  }

  function sceneCode(d) {
    const fns = d.functions.filter(f => S.tests || !f.test);
    const ids = new Set(fns.map(f => f.id));
    const proxies = new Map(d.proxies.map(p => [p.id, p]));
    const nodes = [];
    const edges = [];
    const usedProxy = new Map();

    for (const f of fns) {
      const calls = f.calls.filter(c => ids.has(c.to) || proxies.has(c.to));
      const ins = [{ pin: 'in', label: f.callers_total ? `${f.callers_total} caller${f.callers_total === 1 ? '' : 's'}` : '' }];
      for (const t of f.param_types) ins.push({ data: true, label: t, colour: typeColour(t) });
      if (f.params > f.param_types.length) ins.push({ data: true, label: `+${f.params - f.param_types.length} untyped`, colour: 'var(--grey)' });
      const outs = calls.map(c => {
        const target = d.functions.find(x => x.id === c.to) || proxies.get(c.to);
        return { pin: 'c' + c.to, label: target ? target.name : '?', title: `line ${c.line}` };
      });
      if (f.calls_total > calls.length) outs.push({ more: true, label: `+${f.calls_total - calls.length} more` });
      if (f.ret) outs.push({ data: true, label: f.ret, colour: typeColour(f.ret), pin: 'ret' });
      const badges = [];
      if (f.entry && !f.module && !f.test) badges.push(badge('mode entry', 'entry', 'nothing else in the map calls it'));
      if (f.recursive) badges.push(badge('mode', '↻', 'calls itself'));
      if (f.complexity > 1) badges.push(cxBadge(f.complexity));
      nodes.push({
        id: 'f' + f.id,
        key: `${f.owner || ''}::${f.name}`,
        kind: 'function',
        data: f,
        drill: { level: 'flow', file: f.file, line: f.line, name: f.name },
        el: bp({
          accent: fnAccent(f),
          k: f.module ? 'top level' : f.owner ? f.owner + ' ::' : f.test ? 'test' : 'function',
          t: f.module ? base(f.file) : f.name,
          badges,
          ins,
          outs,
        }),
      });
      for (const c of calls) {
        edges.push({ from: 'f' + f.id, fromPin: 'c' + c.to, to: 'f' + c.to, toPin: 'in', cls: 'exec', data: { line: c.line } });
        if (proxies.has(c.to)) usedProxy.set(c.to, (usedProxy.get(c.to) || '') + 'callee');
      }
      for (const c of f.callers) {
        if (proxies.has(c)) {
          edges.push({ from: 'f' + c, fromPin: 'out', to: 'f' + f.id, toPin: 'in', cls: 'exec' });
          usedProxy.set(c, (usedProxy.get(c) || '') + 'caller');
        }
      }
    }
    for (const [id, how] of usedProxy) {
      const p = proxies.get(id);
      const callee = how.includes('callee');
      const caller = how.includes('caller');
      nodes.push({
        id: 'f' + id,
        key: `${p.file}::${p.owner || ''}::${p.name}`,
        kind: 'proxy',
        data: p,
        pin: callee ? 'last' : 'first',
        drill: { level: 'flow', file: p.file, line: p.line, name: p.name },
        el: bp({
          cls: 'proxy',
          accent: 'var(--grey)',
          k: p.file,
          t: (p.owner ? p.owner + '::' : '') + (p.module ? 'top level' : p.name),
          ins: callee ? [{ pin: 'in', label: '' }] : [],
          outs: caller ? [{ pin: 'out', label: '' }] : [],
        }),
      });
    }
    const types = d.types.map(t => t.name);
    return {
      key: 'code:' + d.file,
      kind: 'bp',
      layout: 'layered',
      gapX: 110,
      nodes,
      edges,
      persist: true,
      file: d,
      note: `${d.file} · ${langName(d.language)} · ${plural(d.functions.length, 'function')}` + (types.length ? ` · types: ${types.slice(0, 8).join(', ')}${types.length > 8 ? ', …' : ''}` : '') + (d.proxies_truncated ? ' · outside callers trimmed' : ''),
      empty: fns.length ? null : ['No functions here', d.functions.length ? 'Every function here is a test - switch tests on to see them.' : types.length ? `This file defines types only: ${types.join(', ')}.` : 'The map holds no functions for this file.'],
      legend: [['entry point', 'var(--red)'], ['function', 'var(--blue)'], ['test', 'var(--grey)'], ['top level', 'var(--purple)'], ['outside this file', 'var(--grey)', 'dash'], ['exec wire', 'var(--faint)', 'line']],
    };
  }

  // ------------------------------------------------------------- level 5: logic

  const MODES = { await: 'await', defer: 'deferred', go: 'goroutine', try: '? propagates', new: 'constructs' };
  const LOOPS = { for: 'For each', while: 'While', loop: 'Loop', do: 'Do while', comprehension: 'Comprehension' };

  function sceneFlow(d) {
    const f = d.function;
    const nodes = [];
    let seq = 0;
    const add = (el, kind, data, drill) => {
      const n = { id: 'n' + seq++, kind, data, el, drill };
      nodes.push(n);
      return n;
    };

    const entryBadges = [];
    if (f.complexity > 1) entryBadges.push(cxBadge(f.complexity));
    const entry = add(
      bp({
        accent: fnAccent(f),
        k: f.module ? 'top level' : (f.owner ? f.owner + ' :: ' : '') + (f.entry ? 'entry point' : 'function'),
        t: f.module ? base(f.file) : f.name,
        q: `${f.file}:${f.line}`,
        badges: entryBadges,
        body: d.callers_total ? `called by ${d.callers.slice(0, 3).map(c => c.name).join(', ')}${d.callers_total > 3 ? ` +${d.callers_total - 3}` : ''}` : f.module ? null : 'nothing in the map calls it',
        outs: [{ pin: 'out', label: 'then' }, ...d.params.map((p, i) => ({ pin: 'p' + i, data: true, label: p, colour: typeColour(p.split(/[:\s]/).pop()) }))].concat(f.ret ? [{ data: true, label: '→ ' + f.ret, colour: typeColour(f.ret) }] : []),
      }),
      'entry',
      { ...d, step: null }
    );

    function call(st) {
      const resolved = !!st.target;
      const badges = st.mode ? [badge('mode ' + st.mode, st.mode === 'try' ? '?' : st.mode, MODES[st.mode])] : [];
      return add(
        bp({
          accent: resolved ? 'var(--blue)' : 'var(--grey)',
          k: resolved ? (st.target.owner ? st.target.owner + ' ::' : 'call') : 'library call',
          t: st.name,
          q: st.qual || null,
          badges,
          ins: [{ pin: 'in', label: '' }, ...st.args.map(a => ({ data: true, label: a, colour: 'var(--grey)' }))],
          outs: [{ pin: 'out', label: '' }],
        }),
        'call',
        st,
        resolved ? { level: 'flow', file: st.target.file, line: st.target.line, name: st.target.name } : null
      );
    }

    function compact(run) {
      const n = h('div', 'node bp compact');
      n.style.setProperty('--accent', 'var(--grey)');
      const rows = h('div', 'rows');
      rows.append(pinEl({ pin: 'in' }, 'in'), h('span', 'chain', run.map(s => (s.mode === 'try' ? s.name + '?' : s.name)).join(' → ')), pinEl({ pin: 'out' }, 'out'));
      n.append(rows);
      n.title = run.map(s => `${s.qual ? s.qual + '.' : ''}${s.name}(${s.args.join(', ')})  line ${s.line}`).join('\n');
      return add(n, 'calls', { t: 'calls', calls: run, line: run[0].line });
    }

    function branch(st) {
      const many = st.kind !== 'if' && st.kind !== 'ternary';
      const title = { try: 'Try', match: 'Match', select: 'Select', switch: 'Switch', ternary: 'Select', if: 'Branch' }[st.kind] || 'Branch';
      const ins = [{ pin: 'in', label: '' }];
      if (st.kind !== 'try') ins.push({ data: true, label: st.label, colour: many ? 'var(--teal)' : 'var(--red)', title: st.label });
      const outs = st.arms.map((a, i) => ({ pin: 'a' + i, label: many ? a.label : a.label === 'true' ? 'True' : 'False', title: a.implicit ? 'no else written - carries straight on' : null }));
      return add(bp({ accent: 'var(--slate)', k: many ? st.kind : 'branch', t: title, ins, outs }), 'branch', st);
    }

    const loop = st => add(bp({ accent: 'var(--teal)', k: 'loop', t: LOOPS[st.kind] || 'Loop', body: st.label, ins: [{ pin: 'in', label: '' }], outs: [{ pin: 'body', label: 'Loop body' }, { pin: 'done', label: 'Completed' }] }), 'loop', st);

    function exit(st) {
      const accent = { return: 'var(--purple)', throw: 'var(--red)', raise: 'var(--red)' }[st.kind] || 'var(--grey)';
      const t = { return: 'Return', throw: 'Throw', raise: 'Raise', break: 'Break', continue: 'Continue' }[st.kind] || st.kind;
      return add(bp({ cls: 'terminal', accent, k: st.kind === 'break' || st.kind === 'continue' ? 'jump' : 'exit', t, body: st.label === st.kind ? null : st.label, ins: [{ pin: 'in', label: '' }] }), 'exit', st);
    }

    const closure = st => add(bp({ accent: 'var(--pink)', k: 'closure', t: 'Closure', body: st.label.replace(/^closure\s*/, '') || null, ins: [{ pin: 'in', label: '' }], outs: [{ pin: 'body', label: 'Body' }, { pin: 'then', label: 'then' }] }), 'closure', st);

    const def = st =>
      add(bp({ accent: 'var(--green)', k: 'defines', t: st.label, ins: [{ pin: 'in', label: '' }], outs: [{ pin: 'out', label: '' }] }), 'def', st, st.target ? { level: 'flow', file: st.target.file, line: st.target.line, name: st.target.name } : null);

    // steps into blocks: a node, or a node whose arms each hold a sequence
    function blocks(steps) {
      const out = [];
      let run = [];
      const flush = () => {
        if (!run.length) return;
        if (S.lib === 'compact') out.push({ node: compact(run) });
        else if (S.lib === 'full') run.forEach(st => out.push({ node: call(st) }));
        run = [];
      };
      for (const st of steps) {
        if (st.t === 'call' && !st.target && S.lib !== 'full') {
          run.push(st);
          continue;
        }
        flush();
        if (st.t === 'call') out.push({ node: call(st) });
        else if (st.t === 'def') out.push({ node: def(st) });
        else if (st.t === 'exit') out.push({ node: exit(st), terminal: true });
        else if (st.t === 'branch') out.push({ node: branch(st), arms: st.arms.map((a, i) => ({ pin: 'a' + i, seq: blocks(a.steps) })) });
        else if (st.t === 'loop') out.push({ node: loop(st), arms: [{ pin: 'body', seq: blocks(st.body), loops: true }], cont: 'done' });
        else if (st.t === 'closure') out.push({ node: closure(st), arms: [{ pin: 'body', seq: blocks(st.body) }], cont: 'then' });
      }
      flush();
      return out;
    }

    const tree = blocks(d.steps);
    const end = add(bp({ cls: 'terminal', accent: 'var(--faint)', k: 'end', t: f.ret ? 'Falls off the end' : 'End', ins: [{ pin: 'in', label: '' }] }), 'end', { t: 'end' });
    return {
      key: 'flow',
      kind: 'bp',
      layout: 'flow',
      nodes,
      edges: [],
      entry,
      end,
      tree,
      note:
        `${f.file}:${f.start}-${f.end} · ${d.step_count} step${d.step_count === 1 ? '' : 's'}` +
        (d.truncated ? ' · cut short: very long or deeply nested' : '') +
        ' · read off the syntax tree, not a trace',
      legend: [
        ['call', 'var(--blue)'],
        ['library call', 'var(--grey)'],
        ['branch', 'var(--slate)'],
        ['loop', 'var(--teal)'],
        ['return', 'var(--purple)'],
        ['throw', 'var(--red)'],
        ['closure', 'var(--pink)'],
        ['defines', 'var(--green)'],
      ],
    };
  }

  // ------------------------------------------------------------- layout

  // Layered, left to right: callers before what they call. Cycles are broken
  // for layering only, rows are ordered by barycentre sweeps, and a layer
  // taller than `maxCol` wraps into several columns.
  function layered(nodes, edges, opts) {
    const gapX = opts.gapX || 90;
    const gapY = opts.gapY || 26;
    const maxCol = opts.maxCol || 18;
    const N = nodes.length;
    const index = new Map(nodes.map((n, i) => [n.id, i]));
    const out = nodes.map(() => []);
    const inn = nodes.map(() => []);
    const seen = new Set();
    for (const e of edges) {
      const a = index.get(e.from);
      const b = index.get(e.to);
      if (a == null || b == null || a === b || seen.has(a * N + b)) continue;
      seen.add(a * N + b);
      out[a].push(b);
      inn[b].push(a);
    }
    const pinned = v => nodes[v].pin;
    const loose = [];
    const placed = [];
    for (let v = 0; v < N; v++) (out[v].length || inn[v].length || pinned(v) ? placed : loose).push(v);

    const mark = new Uint8Array(N);
    const back = new Set();
    for (const s of placed) {
      if (mark[s]) continue;
      mark[s] = 1;
      const stack = [[s, 0]];
      while (stack.length) {
        const top = stack[stack.length - 1];
        const v = top[0];
        if (top[1] < out[v].length) {
          const w = out[v][top[1]++];
          if (mark[w] === 1) back.add(v * N + w);
          else if (!mark[w]) {
            mark[w] = 1;
            stack.push([w, 0]);
          }
        } else {
          mark[v] = 2;
          stack.pop();
        }
      }
    }
    const layer = new Int32Array(N);
    const indeg = new Int32Array(N);
    const fwd = out.map((o, v) => (pinned(v) ? [] : o.filter(w => !pinned(w) && !back.has(v * N + w))));
    for (const v of placed) for (const w of fwd[v]) indeg[w]++;
    const queue = placed.filter(v => !pinned(v) && !indeg[v]);
    for (let qi = 0; qi < queue.length; qi++) {
      const v = queue[qi];
      for (const w of fwd[v]) {
        layer[w] = Math.max(layer[w], layer[v] + 1);
        if (--indeg[w] === 0) queue.push(w);
      }
    }
    let maxL = 0;
    for (const v of placed) if (!pinned(v)) maxL = Math.max(maxL, layer[v]);
    for (const v of placed) {
      if (pinned(v) === 'first') layer[v] = -1;
      else if (pinned(v) === 'last') layer[v] = maxL + 1;
    }
    const byLayer = new Map();
    for (const v of placed) {
      if (!byLayer.has(layer[v])) byLayer.set(layer[v], []);
      byLayer.get(layer[v]).push(v);
    }
    const layers = [...byLayer.keys()].sort((a, b) => a - b).map(k => byLayer.get(k));
    const key = v => String(opts.order ? opts.order(nodes[v]) : nodes[v].id);
    for (const l of layers) l.sort((a, b) => key(a).localeCompare(key(b)));

    const pos = new Float64Array(N);
    const index1 = l => l.forEach((v, i) => (pos[v] = l.length > 1 ? i / (l.length - 1) : 0.5));
    layers.forEach(index1);
    for (let it = 0; it < 8; it++) {
      const down = it % 2 === 0;
      for (const l of down ? layers : [...layers].reverse()) {
        const bc = new Map();
        for (const v of l) {
          const nb = down ? inn[v] : out[v];
          bc.set(v, nb.length ? nb.reduce((a, w) => a + pos[w], 0) / nb.length : pos[v]);
        }
        l.sort((a, b) => bc.get(a) - bc.get(b) || key(a).localeCompare(key(b)));
        index1(l);
      }
    }

    const cols = [];
    for (const l of layers) for (let i = 0; i < l.length; i += maxCol) cols.push(l.slice(i, i + maxCol));
    const colOf = new Int32Array(N);
    const x = new Float64Array(N);
    const y = new Float64Array(N);
    let cx = 0;
    cols.forEach((col, ci) => {
      const w = Math.max(...col.map(v => nodes[v].w));
      for (const v of col) {
        colOf[v] = ci;
        x[v] = cx;
      }
      cx += w + gapX;
    });
    const height = col => col.reduce((a, v) => a + nodes[v].h + gapY, -gapY);
    const tallest = Math.max(0, ...cols.map(height));
    for (const col of cols) {
      let t = (tallest - height(col)) / 2;
      for (const v of col) {
        y[v] = t;
        t += nodes[v].h + gapY;
      }
    }
    const mid = v => y[v] + nodes[v].h / 2;
    for (let it = 0; it < 8; it++) {
      for (const col of it % 2 === 0 ? cols : [...cols].reverse()) {
        const want = col.map(v => {
          const nb = inn[v].concat(out[v]).filter(w => colOf[w] !== colOf[v]);
          return nb.length ? nb.reduce((a, w) => a + mid(w), 0) / nb.length - nodes[v].h / 2 : y[v];
        });
        const top = want.slice();
        for (let i = 1; i < col.length; i++) top[i] = Math.max(top[i], top[i - 1] + nodes[col[i - 1]].h + gapY);
        for (let i = col.length - 2; i >= 0; i--) top[i] = Math.min(top[i], top[i + 1] - nodes[col[i]].h - gapY);
        const drift = want.reduce((a, w, i) => a + (w - top[i]), 0) / col.length;
        col.forEach((v, i) => (y[v] = top[i] + drift));
      }
    }

    const result = new Map();
    let bottom = 0;
    let right = 0;
    for (const v of placed) {
      result.set(nodes[v].id, { x: x[v], y: y[v] });
      bottom = Math.max(bottom, y[v] + nodes[v].h);
      right = Math.max(right, x[v] + nodes[v].w);
    }
    // nodes nothing connects sit in a grid under the graph
    if (loose.length) {
      loose.sort((a, b) => key(a).localeCompare(key(b)));
      const width = Math.max(right, 1100);
      let lx = 0;
      let ly = placed.length ? bottom + 90 : 0;
      let row = 0;
      for (const v of loose) {
        if (lx && lx + nodes[v].w > width) {
          lx = 0;
          ly += row + gapY;
          row = 0;
        }
        result.set(nodes[v].id, { x: lx, y: ly });
        lx += nodes[v].w + 28;
        row = Math.max(row, nodes[v].h);
      }
    }
    return result;
  }

  // the context level: the system in the middle, what calls in on its left,
  // what it reaches on its right
  function layoutContext(scene) {
    const sys = scene.nodes.find(n => n.kind === 'system');
    const result = new Map([[sys.id, { x: 0, y: 0 }]]);
    const side = (list, x) => {
      const total = list.reduce((a, n) => a + n.h + 30, -30);
      let y = sys.h / 2 - total / 2;
      for (const n of list) {
        result.set(n.id, { x: x(n), y });
        y += n.h + 30;
      }
    };
    const ins = scene.nodes.filter(n => n.side === 'in');
    const outs = scene.nodes.filter(n => n.side === 'out');
    side(ins, n => -n.w - 180);
    side(outs, () => sys.w + 180);
    return result;
  }

  // A function's logic, laid out the way its code reads: one row per
  // sequence, each arm of a branch on a band of its own to the right of it,
  // and the code after a branch carrying on past its widest arm.
  function layoutFlow(scene) {
    const GX = 54;
    const GY = 30;
    const edges = [];
    const wire = (p, to, extra) => edges.push({ from: p.node ? p.node.id : null, fromPin: p.pin, fromPt: p.pt, to: to.node ? to.node.id : null, toPin: to.pin, toPt: to.pt, cls: 'exec', route: p.below != null ? 'below' : 'run', below: p.below, ...extra });

    function sequence(seq, x, y, ports) {
      let cx = x;
      let bottom = y;
      for (const b of seq) {
        const n = b.node;
        n.x = cx;
        n.y = y;
        for (const p of ports) wire(p, { node: n, pin: 'in' });
        if (!b.arms) {
          cx += n.w + GX;
          bottom = Math.max(bottom, y + n.h);
          ports = b.terminal ? [] : [{ node: n, pin: 'out' }];
          continue;
        }
        const armX = cx + n.w + GX;
        let ay = y;
        let armW = 0;
        const ends = [];
        for (const arm of b.arms) {
          const start = { node: n, pin: arm.pin };
          let r;
          if (arm.seq.length) r = sequence(arm.seq, armX, ay, [start]);
          else {
            // an empty arm carries straight on along a band of its own
            const pt = { pt: { x: armX, y: ay + 14 } };
            wire(start, pt);
            r = { w: 0, h: 28, ends: [pt] };
          }
          if (arm.loops) {
            const floor = ay + r.h + 18;
            for (const e of r.ends) edges.push({ from: e.node ? e.node.id : null, fromPin: e.pin, fromPt: e.pt, to: n.id, toPt: null, toPin: null, cls: 'exec back', route: 'back', below: floor });
          } else ends.push(...r.ends);
          armW = Math.max(armW, r.w);
          ay += Math.max(r.h, 28) + GY;
        }
        const blockBottom = Math.max(y + n.h, ay - GY);
        // a loop's "completed", a closure's "then": past the arms, routed under them
        if (b.cont) ends.push({ node: n, pin: b.cont, below: blockBottom + 18 });
        cx = armX + armW + GX;
        bottom = Math.max(bottom, blockBottom + (b.cont ? 30 : 0));
        ports = ends;
      }
      return { w: Math.max(0, cx - GX - x), h: bottom - y, ends: ports };
    }

    const entry = scene.entry;
    entry.x = 0;
    entry.y = 0;
    const r = sequence(scene.tree, entry.w + GX, 0, [{ node: entry, pin: 'out' }]);
    const end = scene.end;
    if (r.ends.length) {
      end.x = entry.w + GX + r.w + (scene.tree.length ? GX : 0);
      end.y = 0;
      for (const p of r.ends) wire(p, { node: end, pin: 'in' });
    } else {
      end.el.remove();
      scene.nodes = scene.nodes.filter(n => n !== end);
    }
    scene.edges = edges;
    return new Map(scene.nodes.map(n => [n.id, { x: n.x, y: n.y }]));
  }

  // ------------------------------------------------------------- mounting

  const layers = () => ({ nodes: $('nodes'), wires: $('wire-layer'), labels: $('labels'), frames: $('frames') });

  function clearCanvas() {
    const l = layers();
    l.nodes.textContent = '';
    l.wires.textContent = '';
    l.labels.textContent = '';
    l.frames.textContent = '';
    $('legend').textContent = '';
    $('note').hidden = true;
    S.scene = null;
    S.byId = new Map();
  }

  function measurePins(n) {
    n.pins = new Map();
    for (const p of n.el.querySelectorAll('[data-pin]')) {
      const dot = p.querySelector('i') || p;
      let x = dot.offsetWidth / 2;
      let y = dot.offsetHeight / 2;
      let e = dot;
      while (e && e !== n.el) {
        x += e.offsetLeft;
        y += e.offsetTop;
        e = e.offsetParent;
      }
      n.pins.set(p.dataset.pin, { x, y });
    }
  }

  function mount(scene, keepView) {
    clearCanvas();
    hidePanel();
    hideMessage();
    S.scene = scene;
    S.selected = null;
    const l = layers();
    for (const n of scene.nodes) {
      n.el.dataset.id = n.id;
      l.nodes.append(n.el);
    }
    for (const n of scene.nodes) {
      n.w = n.el.offsetWidth;
      n.h = n.el.offsetHeight;
      measurePins(n);
    }
    let pos;
    if (scene.layout === 'flow') pos = layoutFlow(scene);
    else if (scene.layout === 'context') pos = layoutContext(scene);
    else pos = layered(scene.nodes, scene.edges, scene);
    const saved = scene.persist ? store.get('pos:' + scene.key, {}) : {};
    for (const n of scene.nodes) {
      const p = saved[n.key || n.id] || pos.get(n.id) || { x: 0, y: 0 };
      n.x = p.x;
      n.y = p.y;
      place(n);
      S.byId.set(n.id, n);
    }
    drawFrames();
    drawEdges();
    if (scene.note) {
      $('note').textContent = scene.note;
      $('note').hidden = false;
    }
    if (scene.empty) showMessage(...scene.empty);
    legend(scene.legend || []);
    if (keepView) applyView();
    else fit();
  }

  function place(n) {
    n.el.style.left = n.x + 'px';
    n.el.style.top = n.y + 'px';
  }

  function savePos(n) {
    const key = 'pos:' + S.scene.key;
    const all = store.get(key, {});
    all[n.key || n.id] = { x: Math.round(n.x), y: Math.round(n.y) };
    store.set(key, all);
  }

  function drawFrames() {
    const host = $('frames');
    host.textContent = '';
    for (const f of S.scene.frames || []) {
      const members = f.members.map(id => S.byId.get(id)).filter(Boolean);
      if (!members.length) continue;
      const x0 = Math.min(...members.map(n => n.x)) - 28;
      const y0 = Math.min(...members.map(n => n.y)) - 28;
      const x1 = Math.max(...members.map(n => n.x + n.w)) + 28;
      const y1 = Math.max(...members.map(n => n.y + n.h)) + 56;
      const box = h('div', 'frame' + (f.cls ? ' ' + f.cls : ''));
      Object.assign(box.style, { left: x0 + 'px', top: y0 + 'px', width: x1 - x0 + 'px', height: y1 - y0 + 'px' });
      const title = h('div', 'title', f.title);
      if (f.sub) title.append(h('small', null, `[${f.sub}]`));
      box.append(title);
      host.append(box);
      f.box = { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
    }
  }

  // ------------------------------------------------------------- wires

  function pinPos(n, name, fallbackSide) {
    const p = name && n.pins.get(name);
    if (p) return { x: n.x + p.x, y: n.y + p.y };
    return fallbackSide === 'in' ? { x: n.x, y: n.y + 20 } : { x: n.x + n.w, y: n.y + 20 };
  }

  // straight, from the side of one box facing the other - how doc draws a relationship
  function sideAnchors(a, b) {
    const ca = { x: a.x + a.w / 2, y: a.y + a.h / 2 };
    const cb = { x: b.x + b.w / 2, y: b.y + b.h / 2 };
    const at = (n, c, toward) => {
      const dx = toward.x - c.x;
      const dy = toward.y - c.y;
      if (Math.abs(dx) * n.h >= Math.abs(dy) * n.w) return { x: dx > 0 ? n.x + n.w : n.x, y: c.y };
      return { x: c.x, y: dy > 0 ? n.y + n.h : n.y };
    };
    return [at(a, ca, cb), at(b, cb, ca)];
  }

  function curve(p1, p2) {
    const c = Math.max(46, Math.abs(p2.x - p1.x) / 2);
    return `M${p1.x} ${p1.y} C${p1.x + c} ${p1.y} ${p2.x - c} ${p2.y} ${p2.x} ${p2.y}`;
  }

  // level first, then a bend into the target: keeps a wire inside its own band
  function run(p1, p2) {
    if (p2.x - p1.x > 110 && Math.abs(p2.y - p1.y) > 2) {
      const k = p2.x - 64;
      return `M${p1.x} ${p1.y} L${k} ${p1.y} C${k + 34} ${p1.y} ${p2.x - 30} ${p2.y} ${p2.x} ${p2.y}`;
    }
    return curve(p1, p2);
  }

  function below(p1, p2, yb) {
    return `M${p1.x} ${p1.y} C${p1.x + 34} ${p1.y} ${p1.x + 34} ${yb} ${p1.x + 64} ${yb} L${p2.x - 64} ${yb} C${p2.x - 34} ${yb} ${p2.x - 34} ${p2.y} ${p2.x} ${p2.y}`;
  }

  function backTo(p1, n, yb) {
    const tx = n.x + n.w / 2;
    const ty = n.y + n.h;
    return `M${p1.x} ${p1.y} C${p1.x + 40} ${p1.y} ${p1.x + 40} ${yb} ${p1.x} ${yb} L${tx} ${yb} L${tx} ${ty + 2}`;
  }

  function edgePath(e) {
    const a = e.from != null ? S.byId.get(e.from) : null;
    const b = e.to != null ? S.byId.get(e.to) : null;
    if (S.scene.kind === 'c4') {
      if (!a || !b) return null;
      const [p1, p2] = sideAnchors(a, b);
      e.mid = { x: (p1.x + p2.x) / 2, y: (p1.y + p2.y) / 2 };
      return `M${p1.x} ${p1.y} L${p2.x} ${p2.y}`;
    }
    const p1 = e.fromPt || (a && pinPos(a, e.fromPin, 'out'));
    if (!p1) return null;
    if (e.route === 'back') return b ? backTo(p1, b, e.below) : null;
    const p2 = e.toPt || (b && pinPos(b, e.toPin, 'in'));
    if (!p2) return null;
    if (e.route === 'below') return below(p1, p2, e.below);
    if (e.route === 'run') return run(p1, p2);
    return curve(p1, p2);
  }

  function drawEdges() {
    const wires = $('wire-layer');
    const labels = $('labels');
    wires.textContent = '';
    labels.textContent = '';
    const c4 = S.scene.kind === 'c4';
    for (const e of S.scene.edges) {
      const d = edgePath(e);
      if (!d) continue;
      const path = sv('path', { d, class: 'wire ' + (e.cls || ''), 'marker-end': c4 ? 'url(#arrow)' : null });
      // a wider transparent twin catches the pointer, as doc's hit line does
      const hit = sv('path', { d, stroke: 'transparent', 'stroke-width': 14, fill: 'none' });
      hit.addEventListener('click', ev => {
        ev.stopPropagation();
        selectEdge(e);
      });
      wires.append(path, hit);
      e.path = path;
      e.hit = hit;
      if (c4 && e.label && e.mid) {
        const lab = h('div', 'elabel' + (e.cls === 'declared' ? ' declared' : ''), e.label);
        lab.style.left = e.mid.x + 'px';
        lab.style.top = e.mid.y + 'px';
        if (e.title) lab.title = e.title;
        if (S.scene.quietLabels) lab.style.display = 'none';
        lab.addEventListener('click', ev => {
          ev.stopPropagation();
          selectEdge(e);
        });
        labels.append(lab);
        e.labelEl = lab;
      }
    }
    highlight(S.selected);
  }

  // redraw only the wires touching a node being dragged
  function redrawAround(id) {
    for (const e of S.scene.edges) {
      if (e.from !== id && e.to !== id) continue;
      const d = edgePath(e);
      if (!d || !e.path) continue;
      e.path.setAttribute('d', d);
      e.hit.setAttribute('d', d);
      if (e.labelEl && e.mid) {
        e.labelEl.style.left = e.mid.x + 'px';
        e.labelEl.style.top = e.mid.y + 'px';
      }
    }
  }

  function highlight(id) {
    if (!S.scene) return;
    const c4 = S.scene.kind === 'c4';
    const near = new Set();
    for (const e of S.scene.edges) {
      if (!e.path) continue;
      const hot = id != null && (e.from === id || e.to === id);
      e.path.classList.toggle('hot', hot);
      e.path.classList.toggle('dim', id != null && !hot);
      if (c4) e.path.setAttribute('marker-end', hot ? 'url(#arrow-hot)' : 'url(#arrow)');
      if (e.labelEl) {
        e.labelEl.classList.toggle('dim', id != null && !hot);
        if (S.scene.quietLabels) e.labelEl.style.display = hot ? '' : 'none';
      }
      if (hot) {
        near.add(e.from);
        near.add(e.to);
      }
    }
    for (const n of S.scene.nodes) n.el.classList.toggle('hit', id != null && n.id !== id && near.has(n.id));
  }

  // ------------------------------------------------------------- view

  function applyView() {
    const vp = $('viewport');
    vp.style.setProperty('--zoom', S.zoom);
    vp.style.setProperty('--pan-x', S.panX);
    vp.style.setProperty('--pan-y', S.panY);
    $('zoom-level').textContent = Math.round(S.zoom * 100) + '%';
  }

  function bounds() {
    let x0 = Infinity;
    let y0 = Infinity;
    let x1 = -Infinity;
    let y1 = -Infinity;
    const take = (x, y, w, h) => {
      x0 = Math.min(x0, x);
      y0 = Math.min(y0, y);
      x1 = Math.max(x1, x + w);
      y1 = Math.max(y1, y + h);
    };
    for (const n of S.scene ? S.scene.nodes : []) take(n.x, n.y, n.w, n.h);
    for (const f of (S.scene && S.scene.frames) || []) if (f.box) take(f.box.x, f.box.y, f.box.w, f.box.h);
    return x0 === Infinity ? { x: 0, y: 0, w: 1, h: 1 } : { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
  }

  // Everything in view - except a function's logic, which reads left to right
  // and is often far wider than the screen: past the point where its text
  // stops being legible it opens on the entry node instead. `whole` forces
  // the full view.
  function fit(whole) {
    const vp = $('viewport');
    const b = bounds();
    const vw = vp.clientWidth;
    const vh = vp.clientHeight;
    const z = clamp(Math.min((vw - 96) / b.w, (vh - 150) / b.h), 0.08, 1.15);
    const entry = S.scene && S.scene.entry;
    if (whole !== true && entry && z < 0.55) {
      S.zoom = clamp((vh - 150) / b.h, 0.55, 1);
      S.panX = 48 - b.x * S.zoom;
      S.panY = vh / 2 - (entry.y + entry.h / 2) * S.zoom;
    } else {
      S.zoom = z;
      S.panX = (vw - b.w * S.zoom) / 2 - b.x * S.zoom;
      S.panY = (vh - b.h * S.zoom) / 2 - b.y * S.zoom;
    }
    applyView();
  }

  function zoomAt(mx, my, factor) {
    const z = clamp(S.zoom * factor, 0.08, 3);
    S.panX = mx - ((mx - S.panX) * z) / S.zoom;
    S.panY = my - ((my - S.panY) * z) / S.zoom;
    S.zoom = z;
    applyView();
  }

  function zoomBy(factor) {
    const vp = $('viewport');
    zoomAt(vp.clientWidth / 2, vp.clientHeight / 2, factor);
  }

  // ------------------------------------------------------------- selection and panel

  function select(id) {
    for (const n of S.scene ? S.scene.nodes : []) n.el.classList.toggle('selected', n.id === id);
    S.selected = id;
    highlight(id);
    if (id == null) return hidePanel();
    showPanelFor(S.byId.get(id));
    clearOfPanel(S.byId.get(id));
  }

  // the panel opens over the canvas's right side; a node it would cover is
  // panned left, so what was just clicked stays in sight
  function clearOfPanel(n) {
    const p = $('panel');
    if (!n || p.hidden) return;
    const vp = $('viewport').getBoundingClientRect();
    const limit = p.getBoundingClientRect().left - vp.left - 24;
    const right = (n.x + n.w) * S.zoom + S.panX;
    if (right > limit) {
      S.panX -= right - limit;
      applyView();
    }
  }

  function selectEdge(e) {
    select(null);
    for (const x of S.scene.edges) if (x.path) x.path.classList.toggle('hot', x === e);
    panelEdge(e);
  }

  function drill(n) {
    if (n && n.drill) go(n.drill);
  }

  function hidePanel() {
    $('panel').hidden = true;
  }

  function panel(what, title, fill) {
    $('panel-what').textContent = what;
    $('panel-title').textContent = title;
    const body = $('panel-body');
    body.textContent = '';
    fill(body);
    $('panel').hidden = false;
  }

  function dl(pairs) {
    const d = h('dl');
    for (const [k, v] of pairs) {
      if (v == null || v === '' || v === false) continue;
      d.append(h('dt', null, k));
      const dd = h('dd');
      if (v instanceof Node) dd.append(v);
      else dd.textContent = String(v);
      d.append(dd);
    }
    return d;
  }

  function editorHref(file, line) {
    const root = (S.overview && S.overview.root_path) || '';
    return `vscode://file${root.startsWith('/') ? '' : '/'}${root}/${file}${line ? ':' + line : ''}`;
  }

  function actions(list) {
    const box = h('div', 'actions');
    for (const a of list) {
      if (!a) continue;
      if (a.href) {
        const link = h('a', null, a.label);
        link.href = a.href;
        box.append(link);
      } else {
        const b = h('button', null, a.label);
        b.addEventListener('click', a.run);
        box.append(b);
      }
    }
    return box;
  }

  function goBtn(text, route, title) {
    const b = h('button', 'go', text);
    if (title) b.title = title;
    b.addEventListener('click', () => go(route));
    return b;
  }

  function list(items) {
    const ul = h('ul');
    for (const it of items) {
      const li = h('li');
      li.append(it);
      ul.append(li);
    }
    return ul;
  }

  function section(body, title, content) {
    if (!content) return;
    body.append(h('h3', null, title), content);
  }

  function showPanelFor(n) {
    if (!n) return hidePanel();
    const o = S.overview;
    const d = n.data;
    switch (n.kind) {
      case 'system':
        return panel('Software system', d.name, body => {
          body.append(dl([['files', fmt(d.files)], ['lines', fmt(d.lines)], ['functions', fmt(d.functions)], ['resolved calls', fmt(d.edges)], ['containers', o.containers.length], ['grouped by', d.grouping]]));
          section(body, 'Languages', list(d.languages.map(l => `${langName(l.language)} · ${plural(l.files, 'file')} · ${plural(l.lines, 'line')}`)));
          body.append(actions([{ label: 'Open containers', run: () => drill(n) }]));
        });
      case 'container':
        return panel('Container', d.name, body => {
          body.append(dl([['files', d.files], ['functions', d.funcs], ['lines', fmt(d.lines)], ['entry points', d.entries], ['test files', d.tests], ['globs', d.globs.join(', ')]]));
          section(body, 'Technology', list(d.languages.map(l => `${langName(l.language)} · ${plural(l.files, 'file')}`)));
          const outE = o.container_edges.filter(e => e.from === d.id);
          const inE = o.container_edges.filter(e => e.to === d.id);
          if (outE.length) section(body, 'Calls into', list(outE.map(e => `${e.to} - ${e.detected ? `×${e.count}: ${e.symbols.join(', ')}` : 'declared only'}`)));
          if (inE.length) section(body, 'Called from', list(inE.map(e => `${e.from} - ${e.detected ? `×${e.count}: ${e.symbols.join(', ')}` : 'declared only'}`)));
          body.append(actions([{ label: 'Open components', run: () => drill(n) }]));
        });
      case 'component':
        return panel('Component', d.id, body => {
          body.append(dl([['container', d.container], ['language', langName(d.language)], ['lines', d.lines], ['functions', d.funcs], ['types', d.types], ['entry points', d.entries], ['most complex', d.max_complexity > 1 ? d.max_complexity : null], ['withdrawn by ccc:skip', d.withdrawn ? plural(d.withdrawn, 'function') : null]]));
          const outE = o.component_edges.filter(e => e.from === d.id).sort((a, b) => b.count - a.count);
          const inE = o.component_edges.filter(e => e.to === d.id).sort((a, b) => b.count - a.count);
          if (outE.length) section(body, 'Calls into', list(outE.slice(0, 20).map(e => goBtn(`${e.to} ×${e.count}`, { level: 'code', file: e.to }, e.symbols.join(', ')))));
          if (inE.length) section(body, 'Called from', list(inE.slice(0, 20).map(e => goBtn(`${e.from} ×${e.count}`, { level: 'code', file: e.from }, e.symbols.join(', ')))));
          body.append(actions([{ label: 'Open code', run: () => drill(n) }, { label: 'Open in editor', href: editorHref(d.id, 1) }]));
        });
      case 'outer':
        return panel('Container', d.name, body => {
          body.append(dl([['calls from here', d.calls || null], ['calls into here', d.callers || null]]));
          section(body, 'Symbols', h('div', 'mono', [...d.symbols].slice(0, 40).join(', ')));
          body.append(actions([{ label: 'Open components', run: () => drill(n) }]));
        });
      case 'peer':
        return panel('Peer repository', d.name, body => {
          body.append(dl([['repository', d.repo], ['language', d.language && langName(d.language)], ['reached by', d.source], ['resolved', d.resolved ? 'yes' : 'no'], ['error', d.error], ['provides', d.provides], ['consumes', d.consumes]]));
        });
      case 'outside':
        return panel(d.direction === 'in' ? 'Callers outside this map' : 'Endpoints outside this map', d.transport, body => {
          body.append(h('p', 'doc', d.direction === 'in' ? 'Handlers marked `ccc:serves` that nothing in this map calls - something beyond it must.' : 'Calls marked `ccc:calls` that nothing in this map or its peers answers.'));
          section(body, 'Keys', list(d.keys));
          section(body, 'From containers', list(d.containers.map(c => `${c.container} ×${c.count}`)));
        });
      case 'function':
      case 'proxy':
        return panelFunction(n);
      default:
        return panelStep(n);
    }
  }

  function panelFunction(n) {
    const f = n.data;
    const scene = S.scene;
    const fileData = scene.file;
    const all = fileData ? new Map([...fileData.functions.map(x => [x.id, x]), ...fileData.proxies.map(x => [x.id, x])]) : new Map();
    const sig = f.module ? `${f.file} (top level)` : `${f.owner ? f.owner + '::' : ''}${f.name}(${(f.param_types || []).join(', ')})${f.ret ? ' → ' + f.ret : ''}`;
    panel(n.kind === 'proxy' ? 'Outside this file' : f.entry ? 'Entry point' : 'Function', f.module ? 'top level' : f.name, body => {
      body.append(h('div', 'mono', sig));
      if (f.doc) body.append(h('p', 'doc', f.doc));
      body.append(dl([['file', `${f.file}:${f.line}`], ['span', f.start ? `lines ${f.start}-${f.end}` : null], ['complexity', f.complexity], ['branches', f.branches], ['loops', f.loops], ['parameters', f.params], ['recursive', f.recursive ? 'yes' : null], ['test', f.test ? 'yes' : null]]));
      const route = x => ({ level: 'flow', file: x.file, line: x.line, name: x.name });
      if (f.calls && f.calls.length) section(body, `Calls (${f.calls_total})`, list(f.calls.map(c => all.get(c.to)).filter(Boolean).map(x => goBtn(`${x.owner ? x.owner + '::' : ''}${x.name}  ${x.file === f.file ? '' : x.file}`, route(x)))));
      if (f.callers && f.callers.length) section(body, `Called by (${f.callers_total})`, list(f.callers.map(c => all.get(c)).filter(Boolean).map(x => goBtn(`${x.owner ? x.owner + '::' : ''}${x.name}  ${x.file === f.file ? '' : x.file}`, route(x)))));
      body.append(actions([{ label: 'Open logic', run: () => drill(n) }, n.kind === 'proxy' && { label: 'Open its file', run: () => go({ level: 'code', file: f.file }) }, { label: 'Open in editor', href: editorHref(f.file, f.line) }]));
      body.append(h('p', 'hint', 'Double-click a function to open its logic. Drag to move; positions are remembered.'));
    });
  }

  function panelStep(n) {
    const st = n.data;
    const file = S.route.file;
    if (n.kind === 'entry') {
      const f = st.function;
      return panel(f.entry ? 'Entry point' : 'Function', f.module ? 'top level' : f.name, body => {
        body.append(dl([['file', `${f.file}:${f.line}`], ['span', `lines ${f.start}-${f.end}`], ['complexity', f.complexity], ['branches', f.branches], ['loops', f.loops], ['steps drawn', st.step_count]]));
        if (f.doc) body.append(h('p', 'doc', f.doc));
        if (st.params.length) section(body, 'Parameters', list(st.params.map(p => h('code', null, p))));
        if (st.callers.length) section(body, `Called by (${st.callers_total})`, list(st.callers.map(c => goBtn(`${c.owner ? c.owner + '::' : ''}${c.name}  ${c.file}`, { level: 'flow', file: c.file, line: c.line, name: c.name }))));
        body.append(actions([{ label: 'Open its file', run: () => go({ level: 'code', file: f.file }) }, { label: 'Open in editor', href: editorHref(f.file, f.line) }]));
      });
    }
    if (n.kind === 'calls') {
      return panel('Library calls', `${st.calls.length} call${st.calls.length === 1 ? '' : 's'}`, body => {
        body.append(h('p', 'doc', 'Calls that reach no function in this project - the standard library, a dependency, or a name the resolver found no evidence for.'));
        section(body, 'In order', list(st.calls.map(c => h('code', null, `${c.qual ? c.qual + '.' : ''}${c.name}(${c.args.join(', ')})  :${c.line}`))));
        body.append(actions([{ label: 'Open in editor', href: editorHref(file, st.line) }]));
      });
    }
    const title = st.t === 'call' ? st.name : st.label || st.kind;
    panel({ call: 'Call', branch: 'Branch', loop: 'Loop', exit: 'Exit', closure: 'Closure', def: 'Definition', end: 'End' }[st.t] || 'Step', title, body => {
      body.append(dl([['line', st.line], ['kind', st.kind], ['on', st.t === 'call' ? st.qual : null], ['arguments', st.args && st.args.length ? st.args.join(', ') : null], ['mode', st.mode ? MODES[st.mode] : null], ['condition', st.t === 'branch' ? st.label : null], ['header', st.t === 'loop' ? st.label : null]]));
      if (st.arms) section(body, 'Arms', list(st.arms.map(a => `${a.label}: ${a.steps.length ? plural(a.steps.length, 'step') : a.implicit ? 'nothing written - carries on' : 'nothing'}`)));
      if (st.target) section(body, 'Reaches', goBtn(`${st.target.owner ? st.target.owner + '::' : ''}${st.target.name}  ${st.target.file}:${st.target.line}`, n.drill));
      else if (st.t === 'call') body.append(h('p', 'doc', 'No function in this project - a library call, or one the resolver had no evidence for.'));
      body.append(actions([n.drill && { label: 'Open its logic', run: () => drill(n) }, st.line && { label: 'Open in editor', href: editorHref(file, st.line) }]));
    });
  }

  function panelEdge(e) {
    const d = e.data || {};
    const name = id => (S.byId.get(id) ? S.byId.get(id).data.name || S.byId.get(id).data.id || id : id);
    panel('Relationship', `${name(e.from)} → ${name(e.to)}`, body => {
      body.append(dl([['calls', d.count], ['declared in map.json', d.declared ? 'yes' : null], ['detected', d.detected === false ? 'no - declared only' : null], ['transports', d.transports && d.transports.length ? d.transports.join(', ') : null]]));
      if (d.symbols && d.symbols.length) section(body, 'Symbols', h('div', 'mono', [...new Set(d.symbols)].join(', ')));
      if (d.sites && d.sites.length) {
        section(
          body,
          'Call sites',
          list(
            d.sites.map(s => {
              const a = h('a', null, `${s.caller} ${s.caller_file}:${s.caller_line} → ${s.symbol}`);
              a.href = editorHref(s.caller_file, s.caller_line);
              return a;
            })
          )
        );
      }
    });
  }

  // ------------------------------------------------------------- chrome

  function crumbs() {
    const nav = $('crumbs');
    nav.textContent = '';
    const r = S.route;
    if (!r) return;
    nav.append(h('span', 'lvl', LEVELS[r.level]));
    const items = [[S.overview ? S.overview.system.name : 'System', { level: 'system' }]];
    if (r.level !== 'system') items.push(['containers', { level: 'containers' }]);
    const container = r.level === 'components' ? r.container : r.file ? containerOf(r.file) : null;
    if (container) items.push([container, { level: 'components', container }]);
    if (r.file) items.push([base(r.file), { level: 'code', file: r.file }]);
    if (r.level === 'flow') items.push([r.name || `line ${r.line}`, r]);
    items.forEach(([label, route], i) => {
      if (i) nav.append(h('span', 'sep', '›'));
      const b = h('button', null, label);
      b.title = route.file || route.container || '';
      if (i === items.length - 1) b.setAttribute('aria-current', 'page');
      else b.addEventListener('click', () => go(route));
      nav.append(b);
    });
  }

  function legend(items) {
    const box = $('legend');
    box.textContent = '';
    for (const [label, colour, style] of items) {
      const span = h('span');
      const i = h('i', style ? style.replace('ext', '') : '');
      i.style.setProperty('--c', colour);
      if (style === 'line') i.style.borderColor = colour;
      span.append(i, label);
      box.append(span);
    }
    box.hidden = !items.length;
  }

  function showMessage(title, text) {
    const m = $('message');
    m.textContent = '';
    m.append(h('b', null, title), h('span', null, text));
    m.hidden = false;
  }

  function hideMessage() {
    $('message').hidden = true;
  }

  function chips() {
    $('lib').textContent = 'library calls: ' + { compact: 'folded', full: 'shown', hidden: 'hidden' }[S.lib];
    $('lib').setAttribute('aria-pressed', S.lib !== 'hidden');
    $('tests').setAttribute('aria-pressed', S.tests);
    $('theme').textContent = 'theme: ' + S.theme;
    if (S.theme === 'auto') delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = S.theme;
  }

  // ------------------------------------------------------------- search

  let searchSeq = 0;
  let searchTimer = 0;
  let picks = [];
  let pickAt = -1;

  function hideResults() {
    $('results').hidden = true;
    picks = [];
    pickAt = -1;
  }

  async function runSearch() {
    const q = $('search').value.trim();
    const seq = ++searchSeq;
    if (!q) return hideResults();
    const ql = q.toLowerCase();
    const local = [];
    for (const c of (S.overview && S.overview.containers) || []) if (c.name.toLowerCase().includes(ql)) local.push({ k: 'container', n: c.name, p: plural(c.files, 'file'), route: { level: 'components', container: c.id } });
    for (const c of (S.overview && S.overview.components) || []) if (c.id.toLowerCase().includes(ql)) local.push({ k: 'file', n: c.name, p: c.id, route: { level: 'code', file: c.id } });
    let remote = [];
    try {
      const r = await getJSON('/find?q=' + encodeURIComponent(q) + '&kind=func');
      remote = (r.results || []).map(x => ({ k: 'function', n: (x.owner ? x.owner + '::' : '') + x.name, p: `${x.file}:${x.line}`, route: { level: 'flow', file: x.file, line: x.line, name: x.name } }));
    } catch {
      // the local matches still stand
    }
    if (seq !== searchSeq) return;
    picks = local.slice(0, 8).concat(remote).slice(0, 30);
    pickAt = picks.length ? 0 : -1;
    const box = $('results');
    box.textContent = '';
    if (!picks.length) box.append(h('div', 'none', `Nothing in the map matches “${q}”.`));
    picks.forEach((p, i) => {
      const b = h('button', i === pickAt ? 'on' : '');
      b.append(h('span', 'k', p.k), h('span', 'n', p.n), h('span', 'p', p.p));
      b.addEventListener('mousedown', ev => {
        ev.preventDefault();
        choose(i);
      });
      box.append(b);
    });
    box.hidden = false;
  }

  function choose(i) {
    const p = picks[i];
    if (!p) return;
    hideResults();
    $('search').blur();
    go(p.route);
  }

  function movePick(delta) {
    if (!picks.length) return;
    pickAt = (pickAt + delta + picks.length) % picks.length;
    [...$('results').children].forEach((b, i) => b.classList.toggle('on', i === pickAt));
    $('results').children[pickAt].scrollIntoView({ block: 'nearest' });
  }

  // ------------------------------------------------------------- input

  function bindCanvas() {
    const vp = $('viewport');
    let drag = null;
    let frame = 0;

    vp.addEventListener('pointerdown', e => {
      if (e.target.closest('.elabel')) return;
      const nodeEl = e.target.closest('.node');
      if (nodeEl && e.button === 0) {
        const n = S.byId.get(nodeEl.dataset.id);
        if (n) drag = { mode: 'node', n, sx: e.clientX, sy: e.clientY, ox: n.x, oy: n.y, moved: false, id: e.pointerId };
      } else {
        drag = { mode: 'pan', sx: e.clientX, sy: e.clientY, px: S.panX, py: S.panY, moved: false, id: e.pointerId };
      }
    });

    vp.addEventListener('pointermove', e => {
      if (!drag) {
        if (S.selected == null && S.scene) {
          const nodeEl = e.target.closest('.node');
          const id = nodeEl ? nodeEl.dataset.id : null;
          if (id !== S.hover) {
            S.hover = id;
            highlight(id);
          }
        }
        return;
      }
      const dx = e.clientX - drag.sx;
      const dy = e.clientY - drag.sy;
      if (!drag.moved) {
        if (Math.hypot(dx, dy) < 4) return;
        drag.moved = true;
        // captured only once it is a drag, so a plain click keeps its target
        vp.setPointerCapture(drag.id);
        if (drag.mode === 'pan') vp.classList.add('panning');
        else drag.n.el.classList.add('dragging');
      }
      if (drag.mode === 'pan') {
        S.panX = drag.px + dx;
        S.panY = drag.py + dy;
        applyView();
        return;
      }
      const n = drag.n;
      n.x = drag.ox + dx / S.zoom;
      n.y = drag.oy + dy / S.zoom;
      place(n);
      if (!frame) {
        frame = requestAnimationFrame(() => {
          frame = 0;
          redrawAround(n.id);
          if (S.scene.frames) drawFrames();
        });
      }
    });

    const finish = () => {
      vp.classList.remove('panning');
      if (!drag) return;
      const d = drag;
      drag = null;
      if (d.mode === 'node') {
        d.n.el.classList.remove('dragging');
        if (d.moved) {
          if (S.scene.persist) savePos(d.n);
        } else select(d.n.id);
      } else if (!d.moved) select(null);
    };
    vp.addEventListener('pointerup', finish);
    vp.addEventListener('pointercancel', finish);

    vp.addEventListener('dblclick', e => {
      const nodeEl = e.target.closest('.node');
      if (nodeEl) drill(S.byId.get(nodeEl.dataset.id));
    });
    vp.addEventListener('contextmenu', e => e.preventDefault());

    vp.addEventListener(
      'wheel',
      e => {
        e.preventDefault();
        // a mouse wheel and a pinch zoom; a trackpad's two-finger scroll pans
        const wheel = e.deltaMode === 1 || (e.deltaX === 0 && Number.isInteger(e.deltaY) && Math.abs(e.deltaY) >= 40);
        if (e.ctrlKey || wheel) {
          const r = vp.getBoundingClientRect();
          const step = e.deltaMode === 1 ? e.deltaY * 33 : e.deltaY;
          zoomAt(e.clientX - r.left, e.clientY - r.top, Math.exp(-step * (e.ctrlKey ? 0.01 : 0.0015)));
        } else {
          S.panX -= e.deltaX;
          S.panY -= e.deltaY;
          applyView();
        }
      },
      { passive: false }
    );

    window.addEventListener('resize', () => applyView());
  }

  function bindChrome() {
    $('zoom-in').addEventListener('click', () => zoomBy(1.2));
    $('zoom-out').addEventListener('click', () => zoomBy(1 / 1.2));
    $('zoom-fit').addEventListener('click', () => fit(true));
    $('layout-reset').addEventListener('click', () => {
      if (S.scene) store.set('pos:' + S.scene.key, {});
      render(false);
    });
    $('panel-close').addEventListener('click', () => select(null));
    $('lib').addEventListener('click', () => {
      S.lib = { compact: 'full', full: 'hidden', hidden: 'compact' }[S.lib];
      store.set('lib', S.lib);
      chips();
      if (S.route && S.route.level === 'flow') render(true);
    });
    $('tests').addEventListener('click', () => {
      S.tests = !S.tests;
      store.set('tests', S.tests);
      chips();
      render(true);
    });
    $('theme').addEventListener('click', () => {
      S.theme = { light: 'dark', dark: 'auto', auto: 'light' }[S.theme] || 'light';
      store.set('theme', S.theme);
      chips();
    });

    const input = $('search');
    input.addEventListener('input', () => {
      clearTimeout(searchTimer);
      searchTimer = setTimeout(runSearch, 140);
    });
    input.addEventListener('keydown', e => {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        movePick(1);
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        movePick(-1);
      } else if (e.key === 'Enter') {
        e.preventDefault();
        choose(pickAt);
      } else if (e.key === 'Escape') {
        hideResults();
        input.blur();
      }
    });
    input.addEventListener('blur', () => setTimeout(hideResults, 120));

    document.addEventListener('keydown', e => {
      if (e.target.matches('input, textarea')) return;
      if (e.key === '/') {
        e.preventDefault();
        input.focus();
        input.select();
      } else if (e.key === 'Escape') {
        if (!$('panel').hidden) select(null);
        else up();
      } else if (e.key === 'Backspace') up();
      else if (e.key === 'f' || e.key === 'F') fit(true);
      else if (e.key === '+' || e.key === '=') zoomBy(1.2);
      else if (e.key === '-' || e.key === '_') zoomBy(1 / 1.2);
      else if (e.key === 'Enter' && S.selected) drill(S.byId.get(S.selected));
    });

    window.addEventListener('hashchange', () => render(false));
  }

  // ------------------------------------------------------------- refresh

  // the server rescans a few seconds after a save; when the generation moves,
  // redraw the same view without losing the zoom
  async function poll() {
    try {
      const health = await getJSON('/health');
      if (S.generated && health.generated !== S.generated) {
        S.overview = null;
        S.generated = health.generated;
        await render(true);
      }
    } catch {
      // not answering right now - the next poll tries again
    }
    setTimeout(poll, 4000);
  }

  chips();
  bindCanvas();
  bindChrome();
  applyView();
  render(false).then(() => setTimeout(poll, 4000));
})();

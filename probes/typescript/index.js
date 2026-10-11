// ccc's node probe - records which functions ran on behalf of which test fixture and how many times, as ccc-observed/1 lines
// off unless CCC_OBSERVED_DIR names a directory - `ccc probe typescript` puts the calls into a copy of the project and this package in its node_modules
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const hooks = require("node:async_hooks");
// the real process and clock - a test runner's sandbox hands its code copies of both
const proc = require("node:process");
const { performance } = require("node:perf_hooks");
const commit = require("./commit.js");

const SCHEMA = "ccc-observed/1";
// how often a service writes what its fixtures ran - a process killed rather than stopped loses at most this much
const FLUSH_MS = 2000;
const dir = proc.env.CCC_OBSERVED_DIR || "";

// one state per process however many copies of this module a test runner loads - kept on a core module every copy shares
const KEY = Symbol.for("ccc.probe");
const state =
  hooks[KEY] ||
  (hooks[KEY] = {
    als: new hooks.AsyncLocalStorage(),
    // what each fixture ran that is not written yet, by fixture key
    ran: new Map(),
    // this process's own tests still running - each writes its record when it ends
    local: new Map(),
    patched: new WeakSet(),
    ticking: false,
  });

function key(f) {
  return [f.name, f.file, f.language, f.kind].join("\0");
}

// the fixture this work runs for - bound by a test or a request, or this process's one running test
function current() {
  if (!dir) return undefined;
  const f = state.als.getStore();
  if (f) return f;
  if (state.local.size === 1) return state.local.values().next().value.fixture;
  return undefined;
}

// counts one run of a function for the fixture this work runs for
function hit(file, name, line) {
  if (!dir) return;
  tick();
  const f = current();
  if (!f) return;
  const k = key(f);
  let entry = state.ran.get(k);
  if (!entry) {
    entry = { fixture: f, sites: new Map() };
    state.ran.set(k, entry);
  }
  const s = file + "\0" + name + "\0" + line;
  const site = entry.sites.get(s);
  if (site) site.calls++;
  else entry.sites.set(s, { file, name, line, calls: 1 });
}

// a test's callback, run as a fixture - a promise, a done callback or a plain return all end it
function fixture(name, file, kind, fn) {
  if (!dir || typeof fn !== "function") return fn;
  const language = file.endsWith(".tsx") ? "tsx" : "typescript";
  const wrapped = function (...args) {
    const f = { name, file, language, kind };
    const k = key(f);
    const open = state.local.get(k) || { fixture: f, count: 0 };
    open.count++;
    state.local.set(k, open);
    const start = performance.now();
    let ended = false;
    const end = () => {
      if (ended) return;
      ended = true;
      finish(f, start);
    };
    return state.als.run(f, () => {
      // a done-callback test ends when it calls done
      if (fn.length > 0 && typeof args[0] === "function") {
        const done = args[0];
        const finished = Object.assign(function (...a) {
          end();
          return done.apply(this, a);
        }, done);
        try {
          return fn.apply(this, [finished, ...args.slice(1)]);
        } catch (e) {
          end();
          throw e;
        }
      }
      let out;
      try {
        out = fn.apply(this, args);
      } catch (e) {
        end();
        throw e;
      }
      if (out && typeof out.then === "function") {
        return out.then(
          (v) => {
            end();
            return v;
          },
          (e) => {
            end();
            throw e;
          },
        );
      }
      end();
      return out;
    });
  };
  Object.defineProperty(wrapped, "length", { value: fn.length });
  Object.defineProperty(wrapped, "name", { value: fn.name });
  return wrapped;
}

function finish(f, start) {
  const k = key(f);
  const open = state.local.get(k);
  if (open && --open.count <= 0) state.local.delete(k);
  const entry = state.ran.get(k);
  state.ran.delete(k);
  write(f, entry ? entry.sites : new Map(), Math.round(performance.now() - start));
}

// runs fn as work for f
function bind(f, fn) {
  return f ? state.als.run(f, fn) : fn();
}

// the fixture as w3c baggage - the same four members every ccc probe sends
function baggage(f) {
  return [
    "ccc.fixture=" + encodeURIComponent(f.name),
    "ccc.file=" + encodeURIComponent(f.file),
    "ccc.lang=" + encodeURIComponent(f.language),
    "ccc.kind=" + encodeURIComponent(f.kind),
  ].join(",");
}

// the fixture a baggage header names, among whatever other members it carries
function fromBaggage(header) {
  if (Array.isArray(header)) header = header.join(",");
  if (typeof header !== "string" || !header) return undefined;
  const f = {};
  const fields = { "ccc.fixture": "name", "ccc.file": "file", "ccc.lang": "language", "ccc.kind": "kind" };
  for (const member of header.split(",")) {
    const [pair] = member.split(";");
    const at = pair.indexOf("=");
    if (at < 0) continue;
    const field = fields[pair.slice(0, at).trim()];
    if (!field) continue;
    try {
      f[field] = decodeURIComponent(pair.slice(at + 1).trim().replace(/\+/g, " "));
    } catch {
      // a member that is not percent-encoded names nothing
    }
  }
  return f.name && f.kind ? f : undefined;
}

// ccc's members in place of any earlier ones, beside whatever else the header carries
function merge(header, ours) {
  const kept = String(header || "")
    .split(",")
    .map((m) => m.trim())
    .filter((m) => m && !m.startsWith("ccc."));
  return [...kept, ours].join(",");
}

// every request an http or https server takes runs as work for the fixture its baggage names
function patchServer(proto) {
  if (!proto || state.patched.has(proto)) return;
  state.patched.add(proto);
  const emit = proto.emit;
  proto.emit = function (event, req, ...rest) {
    if (event === "request" && req && req.headers) {
      const f = fromBaggage(req.headers.baggage);
      if (f) return state.als.run(f, () => emit.call(this, event, req, ...rest));
    }
    return emit.call(this, event, req, ...rest);
  };
}

// every request http or https sends carries the fixture it is sent for
function patchClient(mod) {
  if (!mod || state.patched.has(mod)) return;
  state.patched.add(mod);
  const request = mod.request;
  mod.request = function (...args) {
    const req = request.apply(this, args);
    const f = current();
    if (f && !req.headersSent) {
      try {
        req.setHeader("baggage", merge(req.getHeader("baggage"), baggage(f)));
      } catch {
        // headers already on the wire carry nothing more
      }
    }
    return req;
  };
  mod.get = function (...args) {
    const req = mod.request(...args);
    req.end();
    return req;
  };
}

function patchFetch() {
  const orig = globalThis.fetch;
  if (typeof orig !== "function" || state.patched.has(orig)) return;
  const patched = function (input, init) {
    const f = current();
    if (!f) return orig.call(this, input, init);
    const from = init && init.headers ? init.headers : typeof Request !== "undefined" && input instanceof Request ? input.headers : undefined;
    const headers = new Headers(from);
    headers.set("baggage", merge(headers.get("baggage"), baggage(f)));
    return orig.call(this, input, { ...init, headers });
  };
  state.patched.add(patched);
  globalThis.fetch = patched;
}

// grpc-js - every call a server takes runs for the fixture its metadata names, every call a client makes carries its own
function patchGrpc(grpc) {
  if (!grpc || !grpc.Server || !grpc.Client || state.patched.has(grpc.Server.prototype)) return;
  state.patched.add(grpc.Server.prototype);
  const addService = grpc.Server.prototype.addService;
  grpc.Server.prototype.addService = function (service, impl) {
    const bound = Object.create(impl || null);
    for (const [name, def] of Object.entries(service || {})) {
      for (const n of [name, def && def.originalName]) {
        const h = n && impl && impl[n];
        if (typeof h !== "function") continue;
        bound[n] = function (call, callback) {
          const md = call && call.metadata;
          const f = md && typeof md.get === "function" ? fromBaggage(md.get("baggage").map(String)) : undefined;
          return bind(f, () => h.call(impl, call, callback));
        };
      }
    }
    return addService.call(this, service, bound);
  };
  const wrap = (method, fixed) => {
    const orig = grpc.Client.prototype[method];
    if (typeof orig !== "function") return;
    grpc.Client.prototype[method] = function (...args) {
      const f = current();
      if (f) {
        const at = args.findIndex((a, i) => i >= fixed && a instanceof grpc.Metadata);
        const md = at >= 0 ? args[at].clone() : new grpc.Metadata();
        md.set("baggage", merge(md.get("baggage").map(String).join(","), baggage(f)));
        if (at >= 0) args[at] = md;
        else args.splice(fixed, 0, md);
      }
      return orig.apply(this, args);
    };
  };
  wrap("makeUnaryRequest", 4);
  wrap("makeServerStreamRequest", 4);
  wrap("makeClientStreamRequest", 3);
  wrap("makeBidiStreamRequest", 3);
}

// what fixtures ran in this process that no test of its own ended - a service's share of a suite driven from outside
function flush() {
  if (!dir) return;
  for (const [k, entry] of state.ran) {
    if (state.local.has(k)) continue;
    state.ran.delete(k);
    write(entry.fixture, entry.sites, undefined);
  }
}

function tick() {
  if (state.ticking) return;
  state.ticking = true;
  const timer = setInterval(flush, FLUSH_MS);
  if (timer.unref) timer.unref();
  proc.on("exit", flush);
  // a signal nobody else handles still writes the records before it ends the process
  for (const signal of ["SIGTERM", "SIGINT"]) {
    if (proc.listenerCount(signal) > 0) continue;
    proc.once(signal, () => {
      flush();
      if (proc.listenerCount(signal) === 0) proc.kill(proc.pid, signal);
    });
  }
}

function service() {
  return proc.env.CCC_SERVICE || path.basename(proc.argv[1] || proc.argv[0] || "node").replace(/\.[cm]?[jt]s$/, "");
}

// one ccc-observed/1 line - a fixture, and the functions it ran in this process with how many times each ran
function write(f, sites, ms) {
  if (sites.size === 0 && ms === undefined) return;
  const byFile = new Map();
  for (const s of sites.values()) {
    if (!byFile.has(s.file)) byFile.set(s.file, []);
    byFile.get(s.file).push({ name: s.name, line: s.line, calls: s.calls });
  }
  const files = [...byFile.keys()].sort().map((p) => ({ path: p, functions: byFile.get(p).sort((a, b) => a.line - b.line) }));
  const svc = { name: service() };
  const at = proc.env.CCC_COMMIT || commit;
  if (at) svc.commit = at;
  const record = {
    schema: SCHEMA,
    fixture: { name: f.name, kind: f.kind, file: f.file, language: f.language },
    run: proc.env.CCC_RUN || "local",
    service: svc,
    files,
  };
  if (ms !== undefined) record.duration_ms = ms;
  try {
    fs.mkdirSync(dir, { recursive: true });
    fs.appendFileSync(path.join(dir, `${service()}-${proc.pid}.jsonl`), JSON.stringify(record) + "\n");
  } catch {
    // a record that cannot be written is lost, never the run
  }
}

if (dir) {
  const http = require("node:http");
  const https = require("node:https");
  patchServer(http.Server && http.Server.prototype);
  patchServer(https.Server && https.Server.prototype);
  patchClient(http);
  patchClient(https);
  patchFetch();
  try {
    require("node:module").syncBuiltinESMExports();
  } catch {
    // older node has no esm view of the builtins to sync
  }
  try {
    patchGrpc(require("@grpc/grpc-js"));
  } catch {
    // a project without grpc-js has nothing to patch
  }
  // grpc-js loaded from somewhere this package cannot see is patched as it loads
  const Module = require("node:module");
  if (typeof Module._load === "function" && !state.patched.has(Module)) {
    state.patched.add(Module);
    const load = Module._load;
    Module._load = function (request) {
      const exports = load.apply(this, arguments);
      if (request === "@grpc/grpc-js") patchGrpc(exports);
      return exports;
    };
  }
}

module.exports = { hit, fixture, bind, current, baggage, fromBaggage, flush };

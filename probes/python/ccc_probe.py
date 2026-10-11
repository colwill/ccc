# ccc's python probe - records which functions ran on behalf of which test fixture and how many times, as ccc-observed/1 lines
# off unless CCC_OBSERVED_DIR names a directory - `ccc probe python` puts the calls into a copy of the project and this module beside it
import atexit
import collections
import contextvars
import importlib.abc
import importlib.util
import inspect
import json
import os
import signal
import sys
import threading
import time
import urllib.parse

SCHEMA = "ccc-observed/1"
DIR = os.environ.get("CCC_OBSERVED_DIR", "")
# how often a service writes what its fixtures ran - a process killed rather than stopped loses at most this much
FLUSH_SECONDS = 2.0
# the copy's root - this file lives at <root>/.ccc-probe/python/ccc_probe.py
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
# the commit the copy was made from, the copy's path inside the project, and each test file's kind - `ccc probe` fills them in
COMMIT = ""
PREFIX = ""
KINDS = {}

# the fixture a request or a thread works for
_bound = contextvars.ContextVar("ccc_fixture", default=None)
# the test this process is running - pytest and unittest run one at a time, and its worker threads work for it too
_test = None
_lock = threading.Lock()
_writing = threading.Lock()
# what each fixture ran that is not written yet
_ran = {}
_pytest = False
_started = False


# one test, named the way its runner selects it
class Fixture(collections.namedtuple("Fixture", ("name", "file", "language", "kind"))):
    __slots__ = ()


# the fixture this work runs for - bound by a request, else this process's running test
def current():
    if not DIR:
        return None
    return _bound.get() or _test


# counts one run of a function for the fixture this work runs for
def hit(file, name, line):
    if not DIR:
        return
    f = _bound.get() or _test
    if f is None:
        return
    site = (file, name, line)
    with _lock:
        sites = _ran.get(f)
        if sites is None:
            sites = _ran[f] = {}
        sites[site] = sites.get(site, 0) + 1


# runs fn as work for f
def bound(f, fn, *args, **kwargs):
    if f is None:
        return fn(*args, **kwargs)
    token = _bound.set(f)
    try:
        return fn(*args, **kwargs)
    finally:
        _bound.reset(token)


# the fixture as w3c baggage - the same four members every ccc probe sends
def baggage(f):
    members = {"fixture": f.name, "file": f.file, "lang": f.language, "kind": f.kind}
    return ",".join(f"ccc.{k}={urllib.parse.quote(v, safe='')}" for k, v in members.items())


# the fixture a baggage header names, among whatever other members it carries
def from_baggage(header):
    if isinstance(header, bytes):
        header = header.decode("latin-1")
    if not header:
        return None
    got = {}
    for member in str(header).split(","):
        k, sep, v = member.split(";", 1)[0].partition("=")
        if sep:
            got[k.strip()] = urllib.parse.unquote_plus(v.strip())
    name, kind = got.get("ccc.fixture"), got.get("ccc.kind")
    if not name or not kind:
        return None
    return Fixture(name, got.get("ccc.file", ""), got.get("ccc.lang", "python"), kind)


# ccc's members in place of any earlier ones, beside whatever else the header carries
def _merge(header, ours):
    kept = [m.strip() for m in str(header or "").split(",") if m.strip() and not m.strip().startswith("ccc.")]
    return ",".join(kept + [ours])


def _start_test(name, path):
    global _test
    path = os.path.abspath(str(path))
    rel = os.path.relpath(path, ROOT).replace(os.sep, "/")
    file = path.replace(os.sep, "/") if rel.startswith("../") else PREFIX + rel
    _test = Fixture(name, file, "python", KINDS.get(rel, "unit"))
    return _test


def _end_test(f, seconds):
    global _test
    if _test == f:
        _test = None
    with _lock:
        sites = _ran.pop(f, {})
    _write(f, sites, round(seconds * 1000))


# writes what fixtures ran in this process that no test of its own is still running
def flush():
    if not DIR:
        return
    with _lock:
        pending = {f: s for f, s in _ran.items() if f != _test}
        for f in pending:
            del _ran[f]
    for f, sites in pending.items():
        _write(f, sites, None)


def _service():
    if os.environ.get("CCC_SERVICE"):
        return os.environ["CCC_SERVICE"]
    if _pytest:
        return "pytest"
    name = os.path.basename(sys.argv[0] if sys.argv and sys.argv[0] else "python")
    return os.path.splitext(name)[0] or "python"


# one ccc-observed/1 line - a fixture, and the functions it ran in this process with how many times each ran
def _write(f, sites, ms):
    if not sites and ms is None:
        return
    files = {}
    for (path, name, line), calls in sites.items():
        files.setdefault(path, []).append({"name": name, "line": line, "calls": calls})
    service = {"name": _service()}
    commit = os.environ.get("CCC_COMMIT", COMMIT)
    if commit:
        service["commit"] = commit
    record = {
        "schema": SCHEMA,
        "fixture": {"name": f.name, "kind": f.kind, "file": f.file, "language": f.language},
        "run": os.environ.get("CCC_RUN", "local"),
        "service": service,
        "files": [{"path": p, "functions": sorted(fns, key=lambda x: x["line"])} for p, fns in sorted(files.items())],
    }
    if ms is not None:
        record["duration_ms"] = ms
    line = json.dumps(record, separators=(",", ":")) + "\n"
    try:
        with _writing:
            os.makedirs(DIR, exist_ok=True)
            with open(os.path.join(DIR, f"{_service()}-{os.getpid()}.jsonl"), "a", encoding="utf-8") as out:
                out.write(line)
    except OSError:
        pass


# registers with the pytest running this process, if one is - every probed test file and conftest calls it as it loads
def attach():
    global _pytest
    if _pytest or not DIR or "_pytest.config" not in sys.modules:
        return
    import gc

    from _pytest.config import Config

    config = next((o for o in gc.get_objects() if isinstance(o, Config)), None)
    if config is None:
        return
    _pytest = True
    if config.pluginmanager.has_plugin("ccc_probe"):
        return
    import pytest

    # each test a fixture from setup to teardown, timed over its own call phase
    class Plugin:
        def __init__(self):
            self.durations = {}

        @pytest.hookimpl(hookwrapper=True)
        def pytest_runtest_protocol(self, item, nextitem):
            path = getattr(item, "path", None) or getattr(item, "fspath", "")
            f = _start_test(getattr(item, "originalname", None) or item.name, path)
            try:
                yield
            finally:
                _end_test(f, self.durations.pop(item.nodeid, 0.0))

        def pytest_runtest_logreport(self, report):
            if report.when == "call":
                self.durations[report.nodeid] = report.duration

    config.pluginmanager.register(Plugin(), "ccc_probe")


# a unittest run outside pytest - each test method a fixture
def _patch_unittest():
    import unittest

    run = unittest.TestCase.run
    if getattr(run, "_ccc", False):
        return

    def patched(self, result=None):
        if _pytest:
            return run(self, result)
        try:
            path = inspect.getsourcefile(type(self)) or ""
        except TypeError:
            path = ""
        f = _start_test(getattr(self, "_testMethodName", type(self).__name__), path)
        start = time.perf_counter()
        try:
            return run(self, result)
        finally:
            _end_test(f, time.perf_counter() - start)

    patched._ccc = True
    unittest.TestCase.run = patched


# threads and pool workers carry the fixture of whoever started them
def _patch_threads():
    start = threading.Thread.start
    if getattr(start, "_ccc", False):
        return

    def patched_start(self):
        ctx = contextvars.copy_context()
        run = self.run
        self.run = lambda: ctx.run(run)
        return start(self)

    patched_start._ccc = True
    threading.Thread.start = patched_start
    import concurrent.futures

    submit = concurrent.futures.ThreadPoolExecutor.submit

    def patched_submit(self, fn, /, *args, **kwargs):
        return submit(self, contextvars.copy_context().run, fn, *args, **kwargs)

    concurrent.futures.ThreadPoolExecutor.submit = patched_submit


# every request http.client sends - urllib, requests and urllib3 among them - carries the fixture it is sent for
def _patch_http_client(mod):
    putrequest = mod.HTTPConnection.putrequest
    if getattr(putrequest, "_ccc", False):
        return

    def patched(self, *args, **kwargs):
        putrequest(self, *args, **kwargs)
        f = current()
        if f is not None:
            self.putheader("baggage", baggage(f))

    patched._ccc = True
    mod.HTTPConnection.putrequest = patched


def _patch_httpx(mod):
    send = mod.Client.send
    asend = mod.AsyncClient.send

    def patched(self, request, *args, **kwargs):
        f = current()
        if f is not None:
            request.headers["baggage"] = _merge(request.headers.get("baggage"), baggage(f))
        return send(self, request, *args, **kwargs)

    async def apatched(self, request, *args, **kwargs):
        f = current()
        if f is not None:
            request.headers["baggage"] = _merge(request.headers.get("baggage"), baggage(f))
        return await asend(self, request, *args, **kwargs)

    mod.Client.send = patched
    mod.AsyncClient.send = apatched


def _patch_aiohttp(mod):
    request = mod.ClientSession._request

    async def patched(self, method, url, **kwargs):
        f = current()
        if f is not None:
            headers = dict(kwargs.get("headers") or {})
            headers["baggage"] = _merge(headers.get("baggage"), baggage(f))
            kwargs["headers"] = headers
        return await request(self, method, url, **kwargs)

    mod.ClientSession._request = patched


# a wsgi app that serves each request for the fixture its baggage names
def _wsgi(app):
    def served(environ, start_response):
        return bound(from_baggage(environ.get("HTTP_BAGGAGE")), app, environ, start_response)

    return served


# an asgi call that serves each request for the fixture its baggage names
def _asgi(call):
    async def served(self, scope, receive, send):
        f = None
        if scope.get("type") in ("http", "websocket"):
            for k, v in scope.get("headers") or ():
                if k.lower() == b"baggage":
                    f = from_baggage(v)
        if f is None:
            return await call(self, scope, receive, send)
        token = _bound.set(f)
        try:
            return await call(self, scope, receive, send)
        finally:
            _bound.reset(token)

    return served


def _patch_method(cls, name, wrap):
    method = getattr(cls, name)
    if getattr(method, "_ccc", False):
        return
    patched = wrap(method)
    patched._ccc = True
    setattr(cls, name, patched)


def _patch_flask(mod):
    _patch_method(mod.Flask, "wsgi_app", lambda m: lambda self, environ, start_response: _wsgi(lambda e, s: m(self, e, s))(environ, start_response))


def _patch_django_wsgi(mod):
    _patch_method(mod.WSGIHandler, "__call__", lambda m: lambda self, environ, start_response: _wsgi(lambda e, s: m(self, e, s))(environ, start_response))


def _patch_wsgiref(mod):
    _patch_method(mod.BaseHandler, "run", lambda m: lambda self, application: m(self, _wsgi(application)))


def _patch_starlette(mod):
    _patch_method(mod.Starlette, "__call__", _asgi)


def _patch_django_asgi(mod):
    _patch_method(mod.ASGIHandler, "__call__", _asgi)


# http.server - each request a handler parses is served for the fixture its baggage names
def _patch_http_server(mod):
    handler = mod.BaseHTTPRequestHandler
    parse = handler.parse_request
    one = handler.handle_one_request
    if getattr(one, "_ccc", False):
        return

    def parse_request(self):
        ok = parse(self)
        if ok:
            f = from_baggage(self.headers.get("baggage"))
            if f is not None:
                self._ccc_token = _bound.set(f)
        return ok

    def handle_one_request(self):
        try:
            return one(self)
        finally:
            token = self.__dict__.pop("_ccc_token", None)
            if token is not None:
                _bound.reset(token)

    handle_one_request._ccc = True
    handler.parse_request = parse_request
    handler.handle_one_request = handle_one_request


# the caller's interceptors with one more on the end - however the caller passed them
def _with_interceptor(fn, at, interceptor):
    def patched(*args, **kwargs):
        if len(args) > at:
            args = list(args)
            args[at] = list(args[at] or ()) + [interceptor]
        else:
            kwargs["interceptors"] = list(kwargs.get("interceptors") or ()) + [interceptor]
        return fn(*args, **kwargs)

    return patched


def _metadata_fixture(metadata):
    for k, v in metadata or ():
        if k == "baggage":
            return from_baggage(v)
    return None


# grpc - every call a server takes runs for the fixture its metadata names, every call a client makes carries its own
def _patch_grpc(grpc):
    if getattr(grpc, "_ccc", False):
        return
    grpc._ccc = True

    def unary(behaviour, f):
        return lambda request, context: bound(f, behaviour, request, context)

    def streaming(behaviour, f):
        def served(request, context):
            token = _bound.set(f)
            try:
                yield from behaviour(request, context)
            finally:
                _bound.reset(token)

        return served

    class ServerInterceptor(grpc.ServerInterceptor):
        def intercept_service(self, continuation, details):
            handler = continuation(details)
            f = _metadata_fixture(details.invocation_metadata)
            if handler is None or f is None:
                return handler
            changes = {}
            for field, wrap in (("unary_unary", unary), ("stream_unary", unary), ("unary_stream", streaming), ("stream_stream", streaming)):
                behaviour = getattr(handler, field, None)
                if behaviour is not None:
                    changes[field] = wrap(behaviour, f)
            return handler._replace(**changes)

    Details = collections.namedtuple("Details", ("method", "timeout", "metadata", "credentials", "wait_for_ready", "compression"))

    class CallDetails(Details, grpc.ClientCallDetails):
        pass

    def outgoing(details):
        f = current()
        if f is None:
            return details
        metadata = [(k, v) for k, v in (details.metadata or ()) if k != "baggage"] + [("baggage", baggage(f))]
        return CallDetails(details.method, details.timeout, metadata, details.credentials, getattr(details, "wait_for_ready", None), getattr(details, "compression", None))

    class ClientInterceptor(grpc.UnaryUnaryClientInterceptor, grpc.UnaryStreamClientInterceptor, grpc.StreamUnaryClientInterceptor, grpc.StreamStreamClientInterceptor):
        def intercept_unary_unary(self, continuation, details, request):
            return continuation(outgoing(details), request)

        def intercept_unary_stream(self, continuation, details, request):
            return continuation(outgoing(details), request)

        def intercept_stream_unary(self, continuation, details, requests):
            return continuation(outgoing(details), requests)

        def intercept_stream_stream(self, continuation, details, requests):
            return continuation(outgoing(details), requests)

    def intercepted(fn):
        return lambda *args, **kwargs: grpc.intercept_channel(fn(*args, **kwargs), ClientInterceptor())

    grpc.server = _with_interceptor(grpc.server, 2, ServerInterceptor())
    grpc.insecure_channel = intercepted(grpc.insecure_channel)
    grpc.secure_channel = intercepted(grpc.secure_channel)
    aio = sys.modules.get("grpc.aio")
    if aio is not None:
        _patch_grpc_aio(aio)


# grpc's asyncio api - the same, through its own interceptor classes and metadata type
def _patch_grpc_aio(aio):
    if getattr(aio, "_ccc", False):
        return
    aio._ccc = True

    def unary(behaviour, f):
        async def served(request, context):
            token = _bound.set(f)
            try:
                return await behaviour(request, context)
            finally:
                _bound.reset(token)

        return served

    def streaming(behaviour, f):
        async def served(request, context):
            token = _bound.set(f)
            try:
                async for item in behaviour(request, context):
                    yield item
            finally:
                _bound.reset(token)

        return served

    class ServerInterceptor(aio.ServerInterceptor):
        async def intercept_service(self, continuation, details):
            handler = await continuation(details)
            f = _metadata_fixture(details.invocation_metadata)
            if handler is None or f is None:
                return handler
            changes = {}
            for field, wrap in (("unary_unary", unary), ("stream_unary", unary), ("unary_stream", streaming), ("stream_stream", streaming)):
                behaviour = getattr(handler, field, None)
                if behaviour is not None:
                    changes[field] = wrap(behaviour, f)
            return handler._replace(**changes)

    def outgoing(details):
        f = current()
        if f is None:
            return details
        kept = [(k, v) for k, v in (details.metadata or ()) if k != "baggage"]
        return details._replace(metadata=aio.Metadata(*kept, ("baggage", baggage(f))))

    class ClientInterceptor(aio.UnaryUnaryClientInterceptor, aio.UnaryStreamClientInterceptor, aio.StreamUnaryClientInterceptor, aio.StreamStreamClientInterceptor):
        async def intercept_unary_unary(self, continuation, details, request):
            return await continuation(outgoing(details), request)

        async def intercept_unary_stream(self, continuation, details, request):
            return await continuation(outgoing(details), request)

        async def intercept_stream_unary(self, continuation, details, requests):
            return await continuation(outgoing(details), requests)

        async def intercept_stream_stream(self, continuation, details, requests):
            return await continuation(outgoing(details), requests)

    aio.server = _with_interceptor(aio.server, 2, ServerInterceptor())
    aio.insecure_channel = _with_interceptor(aio.insecure_channel, 3, ClientInterceptor())
    aio.secure_channel = _with_interceptor(aio.secure_channel, 4, ClientInterceptor())


# modules patched as they are imported - or at once, when one already is
_PATCHES = {
    "http.server": _patch_http_server,
    "wsgiref.handlers": _patch_wsgiref,
    "httpx": _patch_httpx,
    "aiohttp": _patch_aiohttp,
    "flask": _patch_flask,
    "django.core.handlers.wsgi": _patch_django_wsgi,
    "django.core.handlers.asgi": _patch_django_asgi,
    "starlette.applications": _patch_starlette,
    "grpc": _patch_grpc,
    "grpc.aio": _patch_grpc_aio,
}


class _Loader(importlib.abc.Loader):
    def __init__(self, loader, patch):
        self._loader = loader
        self._patch = patch

    def create_module(self, spec):
        return self._loader.create_module(spec)

    def exec_module(self, module):
        self._loader.exec_module(module)
        _apply(self._patch, module)

    def __getattr__(self, name):
        return getattr(self._loader, name)


class _Finder(importlib.abc.MetaPathFinder):
    def __init__(self):
        self._finding = set()

    def find_spec(self, name, path, target=None):
        patch = _PATCHES.get(name)
        if patch is None or name in self._finding:
            return None
        self._finding.add(name)
        try:
            spec = importlib.util.find_spec(name)
        finally:
            self._finding.discard(name)
        if spec is None or spec.loader is None or not hasattr(spec.loader, "exec_module"):
            return None
        spec.loader = _Loader(spec.loader, patch)
        return spec


def _apply(patch, module):
    try:
        patch(module)
    except Exception:
        # a library shaped differently than expected keeps working, unprobed
        pass


def _tick():
    while True:
        time.sleep(FLUSH_SECONDS)
        flush()


def _install():
    global _started
    if _started or not DIR:
        return
    _started = True
    import http.client

    _apply(_patch_http_client, http.client)
    _apply(lambda _: _patch_threads(), None)
    _apply(lambda _: _patch_unittest(), None)
    for name, patch in _PATCHES.items():
        if name in sys.modules:
            _apply(patch, sys.modules[name])
    sys.meta_path.insert(0, _Finder())
    atexit.register(flush)
    threading.Thread(target=_tick, name="ccc-probe-flush", daemon=True).start()
    # a terminating signal nobody else handles still writes the records before it ends the process
    try:
        if threading.current_thread() is threading.main_thread() and signal.getsignal(signal.SIGTERM) == signal.SIG_DFL:

            def terminated(signum, frame):
                flush()
                signal.signal(signum, signal.SIG_DFL)
                os.kill(os.getpid(), signum)

            signal.signal(signal.SIGTERM, terminated)
    except (ValueError, OSError, AttributeError):
        pass


_install()

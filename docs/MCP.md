# MCP

`ccc serve` parses the project into an **in-memory copy of the map** and
serves it over local HTTP, so AI agents query the code map directly instead
of reading `.ccc` files from disk. A file watcher rescans automatically when
source changes (default: every 2s; `--no-watch` to disable):

```sh
ccc serve      # http://127.0.0.1:6767  (MCP endpoint at /mcp -- insights at /insights)
```

```sh
curl -s localhost:6767/find?q=charge            # symbol search (file:line + docs)
curl -s localhost:6767/references?symbol=charge # definitions + every call site
curl -s localhost:6767/dependencies?file=src/render.rs   # file-level impact
curl -s -X POST localhost:6767/refresh          # force an immediate rescan
```


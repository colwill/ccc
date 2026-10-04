# MCP

`ccc run` parses the project into an **in-memory copy of the map** and
serves it over local HTTP, so AI agents query the code map directly instead
of reading `.ccc` files from disk. A file watcher rescans automatically when
source changes (default: every 2s; `--no-watch` to disable):

```sh
ccc run        # http://127.0.0.1:6767  (MCP endpoint at /mcp -- insights at /insights)
```

```sh
curl -s localhost:6767/find?q=charge            # symbol search (file:line + docs)
curl -s localhost:6767/references?symbol=charge # definitions + every call site
curl -s localhost:6767/dependencies?file=src/render.rs   # file-level impact
curl -s -X POST localhost:6767/refresh          # force an immediate rescan
```

## Editing

ccc is the only writer an agent uses on a project it serves - the server's instructions rule out the agent's own
Edit/Write tools, `sed -i` and shell redirection. A `find` or `references` answer ends with a handle, and the edit
tools take it. Every edit is staged into a changeset and shown as a diff before anything is written:

```text
find {"query": "hello"}                    -> 2 result(s) ... handle: `r1` (2 site(s))
edit_rename {"result": "r1", "to": "hi"}   -> `hello_all` -> `hi_all`, `hello_world` -> `hi_world` + diff, changeset `c2-...`
edit_apply {"changeset": "c2-..."}         -> applied changeset `c2-...`, verified, map rescanned
```

| Tool | What it does |
|---|---|
| `edit_rename` | renames each identifier a handle points at. A name with a single definition is followed through every file the map ties to it, including lines the map skips. Same-named sites it left alone are listed with the reason, under a handle that pulls them in when passed back. Comments, strings and module path segments never change |
| `edit_replace` | replaces the token, the line, or the whole definition at each site |
| `edit_delete` | removes a definition with its doc comments and attributes, a call that is a statement on its own, a single-name import, or a note. Refuses while anything still names what it removes |
| `edit_insert` | adds code before or after the one site a handle names, at that site's indentation |
| `edit_text` | every other write: an exact-text replacement, a whole file, a removed file |
| `edit_apply` | writes a changeset - every file or none - and rescans the map |
| `edit_discard` | drops a pending changeset |
| `edit_revert` | puts back an applied changeset, provided none of its files changed since |

What every write is held to:

- **Confined.** The path stays inside the project root: no `..`, no symlinked directory or file on the way, never
  `.git`, and under `.ccc` only `map.json` and `surface.json`.
- **Checked.** Every file must still hold what it held when the change was staged. If one moved, nothing is
  written and the changeset stays pending.
- **Atomic.** Each file is written to a temp file beside it and renamed into place. If any rename fails, the files
  already swapped are put back.
- **Agents only.** The edit tools refuse any request that carries an `Origin` header. A browser page (even a
  `file://` report) can read the map but never write through it.
- **Attributed.** Each applied changeset is appended to `.ccc/edits.jsonl`, which is kept out of git. `ccc prompts`
  reads it to credit a ccc edit to the request behind it, as strongly as it credits an `Edit` or `Write`.
- **Shared.** Every step - staged, applied, reverted, and each inspect an agent made - is appended to
  `.ccc/timeline.jsonl`, also kept out of git, so the visualiser of any ccc server on the project shows it.
  An inspect is a read tool's answer, or a file the agent read with its own tools, which the server
  finds in Claude Code's transcripts (subagents included) every couple of seconds.


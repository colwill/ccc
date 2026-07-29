---
ContextCodeCache - agent guide

what:  a GENERATED map of this project's source. Each source file has a
        `<module>-<file>.<ext>.md` entry listing its constants, functions
        (L<line>:<col>@name:return), intra-file call graph (refs), and
        marker notes (TODO/FIXME/...). See the `# files` list below.
why:   lets agents orient in the codebase cheaply - skim `.ccc` first to
        find where things live, then open the real source for detail.
        `tokens.bin`/`tokens.json`, if present, hold this content pre-encoded
        as APPROXIMATE tiktoken (o200k) ids - for a downstream model that
        shares that vocabulary, NOT for Claude (different tokenizer; its API
        takes text, not token ids). Feed Claude the markdown above as text.
query: `ccc serve` exposes this map over local HTTP - REST endpoints
        (/find /references /dependencies /file /notes) plus an MCP
        endpoint at /mcp - so agents can query instead of reading files.
keep-fresh: whenever you change tracked source, regenerate with
        `ccc scan` (add `--tokens` to refresh the token stream). CI runs
        `ccc check`, which fails when `.ccc` is out of date.
do-not-edit: never hand-edit files under `.ccc` - they are overwritten on
        the next scan. To change the cache, change the source, then rescan.
---

# ContextCodeCache (20260729-22-00-57) UTC
### project: codecache
### totals: 12 files, 229 funcs, 44 consts, 390 refs, 6 notes
### regenerate: `ccc scan`
### files
    - [src/extract.rs](src-extract.rs.md) [rust] 59f/5c/162r/1n
    - [src/html.rs](src-html.rs.md) [rust] 5f/1c/4r/0n
    - [src/languages.rs](src-languages.rs.md) [rust] 11f/7c/0r/0n
    - [src/lib.rs](src-lib.rs.md) [rust] 0f/0c/0r/0n
    - [src/main.rs](src-main.rs.md) [rust] 15f/8c/21r/0n
    - [src/model.rs](src-model.rs.md) [rust] 2f/0c/0r/1n
    - [src/naming.rs](src-naming.rs.md) [rust] 8f/0c/7r/0n
    - [src/render.rs](src-render.rs.md) [rust] 6f/0c/0r/0n
    - [src/scan.rs](src-scan.rs.md) [rust] 9f/5c/9r/1n
    - [src/serve.rs](src-serve.rs.md) [rust] 66f/9c/133r/0n
    - [src/surf.rs](src-surf.rs.md) [rust] 36f/3c/51r/3n
    - [src/tokenize.rs](src-tokenize.rs.md) [rust] 12f/6c/3r/0n

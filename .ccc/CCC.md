---
ContextCodeCache - agent guide

what:  a GENERATED map of this project's source. Each source file has a
        `<module>-<file>.<ext>.md` entry listing the submodules it declares,
        its imports (`pub` marks a re-export), its constants, functions
        (L<line>:<col>@name:return), intra-file call graph (refs), and
        marker notes (TODO/FIXME/...). See the `# files` list below -
        counts read Nf/Nc/Nr/Nn, plus Nm modules and Nx re-exports when a
        file has them. A module root defines nothing and is not empty.
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

# ContextCodeCache (20260820-07-57-23) UTC
### project: ccc
### totals: 37 files, 785 funcs, 137 consts, 1304 refs, 5 notes, 13 mods, 19 re-exports
### regenerate: `ccc scan`
### files
    - [build.rs](build.rs.md) [rust] 8f/2c/10r/0n
    - [extensions/vscode/esbuild.mjs](extensions-vscode-esbuild.mjs.md) [javascript] 0f/4c/0r/0n
    - [extensions/vscode/src/binary.ts](extensions-vscode-src-binary.ts.md) [typescript] 3f/1c/3r/0n
    - [extensions/vscode/src/client.ts](extensions-vscode-src-client.ts.md) [typescript] 14f/2c/8r/0n
    - [extensions/vscode/src/codelens.ts](extensions-vscode-src-codelens.ts.md) [typescript] 10f/0c/6r/0n
    - [extensions/vscode/src/commands.ts](extensions-vscode-src-commands.ts.md) [typescript] 9f/0c/9r/0n
    - [extensions/vscode/src/complexitypanel.ts](extensions-vscode-src-complexitypanel.ts.md) [typescript] 22f/2c/26r/0n
    - [extensions/vscode/src/config.ts](extensions-vscode-src-config.ts.md) [typescript] 5f/0c/7r/0n
    - [extensions/vscode/src/decorations.ts](extensions-vscode-src-decorations.ts.md) [typescript] 10f/5c/8r/0n
    - [extensions/vscode/src/enclosing.ts](extensions-vscode-src-enclosing.ts.md) [typescript] 4f/0c/0r/0n
    - [extensions/vscode/src/extension.ts](extensions-vscode-src-extension.ts.md) [typescript] 21f/2c/26r/0n
    - [extensions/vscode/src/hover.ts](extensions-vscode-src-hover.ts.md) [typescript] 19f/3c/54r/0n
    - [extensions/vscode/src/log.ts](extensions-vscode-src-log.ts.md) [typescript] 13f/1c/9r/0n
    - [extensions/vscode/src/mcpconfig.ts](extensions-vscode-src-mcpconfig.ts.md) [typescript] 4f/3c/5r/0n
    - [extensions/vscode/src/model.ts](extensions-vscode-src-model.ts.md) [typescript] 33f/4c/44r/0n
    - [extensions/vscode/src/pathkeys.ts](extensions-vscode-src-pathkeys.ts.md) [typescript] 2f/2c/0r/0n
    - [extensions/vscode/src/paths.ts](extensions-vscode-src-paths.ts.md) [typescript] 4f/0c/0r/0n
    - [extensions/vscode/src/server.ts](extensions-vscode-src-server.ts.md) [typescript] 16f/6c/24r/0n
    - [extensions/vscode/src/session.ts](extensions-vscode-src-session.ts.md) [typescript] 23f/0c/18r/0n
    - [extensions/vscode/src/status.ts](extensions-vscode-src-status.ts.md) [typescript] 5f/0c/1r/0n
    - [extensions/vscode/src/testpanel.ts](extensions-vscode-src-testpanel.ts.md) [typescript] 13f/0c/13r/0n
    - [extensions/vscode/src/types.ts](extensions-vscode-src-types.ts.md) [typescript] 7f/1c/3r/0n
    - [src/changes.rs](src-changes.rs.md) [rust] 73f/12c/107r/4n
    - [src/coverage.rs](src-coverage.rs.md) [rust] 27f/11c/30r/1n
    - [src/externals.rs](src-externals.rs.md) [rust] 12f/4c/7r/0n
    - [src/extract.rs](src-extract.rs.md) [rust] 121f/11c/333r/0n
    - [src/html.rs](src-html.rs.md) [rust] 9f/2c/8r/0n
    - [src/insights.rs](src-insights.rs.md) [rust] 63f/10c/89r/0n
    - [src/languages.rs](src-languages.rs.md) [rust] 27f/12c/0r/0n
    - [src/lib.rs](src-lib.rs.md) [rust] 0f/0c/0r/0n/13m/19x
    - [src/main.rs](src-main.rs.md) [rust] 15f/10c/24r/0n
    - [src/model.rs](src-model.rs.md) [rust] 8f/2c/2r/0n
    - [src/naming.rs](src-naming.rs.md) [rust] 8f/0c/7r/0n
    - [src/render.rs](src-render.rs.md) [rust] 6f/0c/0r/0n
    - [src/scan.rs](src-scan.rs.md) [rust] 9f/5c/9r/0n
    - [src/serve.rs](src-serve.rs.md) [rust] 150f/14c/411r/0n
    - [src/tokenize.rs](src-tokenize.rs.md) [rust] 12f/6c/3r/0n

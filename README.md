# ContextCodeCache (`ccc`)

Tool that scans a project and generates a **ContextCodeCache** - a `.ccc`
directory holding a compact, machine-readable map of every source file: its
constants, functions (with return types and doc summaries), intra-file call
graph, and marker notes (TODO/FIXME/...). It is designed to give agents a
cheap, always-fresh index of a project.

Please ⭐ if you find this useful 💚

## Install / build

```sh
cargo build --release          # binary @ target/release/ccc
./target/release/ccc install   # copy it onto your PATH (Linux)
```

`ccc install` copies the running binary into `~/.local/bin` (the user-local bin
dir on Linux — no `sudo` needed) and marks it executable. Pass `--dir <DIR>` to
choose a different directory, or `--force` to overwrite an existing `ccc`. If the
target directory isn't on your `$PATH`, it prints the line to add to your shell
profile.

## Usage

```sh
ccc scan [PATH]              # regen PATH/.ccc  (PATH defaults to ".")
ccc scan [PATH] --tokens     # also pre-encode the cache into a token stream
ccc check [PATH]             # exit non-zero if .ccc is stale - for CI
ccc check [PATH] --format json   # same, but print changed cache files as JSON
ccc tokenize [PATH]          # pre-encode an existing .ccc into tokens.bin + tokens.json
ccc install [--dir DIR]      # install the ccc binary onto your PATH (Linux)
```

`ccc check --format json` prints one line — `{ root, up_to_date, files[], changes[] }` —
where `files` is the repo-relative paths of the out-of-date cache entries. It's
meant to be consumed by other tooling; the bundled GitHub Action feeds that array
to downstream jobs via `fromJSON(...)`:

```jsonc
{"root":"example","up_to_date":false,
 "files":["example/.ccc/CCC.md","example/.ccc/src-math.rs.md"],
 "changes":[{"status":"modified","file":"CCC.md","path":"example/.ccc/CCC.md"}, ...]}
```

`scan` rewrites every per-file entry plus the `CCC.md` index, so committed diffs
always come from re-running the generator. `check` regenerates in memory and
compares against the committed `.ccc`, ignoring generation timestamps, so a
freshness gate never fails purely because time passed.

## Specification

```
.ccc/
├── CCC.md                # index: totals + one line per file
├── src-main.rs.md        # <module>-<file>.<ext>.md, one per source file
└── src-math.rs.md
```

Each per-file entry follows this format:

```md
# math.rs.md (yyyymmdd-hh-mm-ss) UTC
# source: src/math.rs [rust]
# const
    - L4@PI:f64
# funcs
    - L7:8@square:f64 // Square a number.
    - L12:8@circle_area:f64 // Area of a circle with the given radius.
# refs
    - circle_area@L14 calls L7:8@square:f64
# note
    - @L13 NOTE: uses the truncated PI above, so results are approximate.
```

- **const** - module-level constants/statics: `L<line>@<name>:<type>`
- **funcs** - definitions: `L<line>:<col>@<name>:<return_type> // doc summary`
- **refs** - calls resolved to a function defined in the same file:
  `<caller>@L<line> calls L<line>:<col>@<func>:<return_type>`
- **note** - marker comments (TODO, FIXME, XXX, HACK, BUG, NOTE, SAFETY)

A worked example lives in [example/](example/) with its generated
[example/.ccc/](example/.ccc/).

## Token stream (pre-encoded cache)

> **Not compatible with Anthropic models.** These are **approximate** [tiktoken](https://github.com/openai/tiktoken)
> IDs (an OpenAI vocabulary). Which can be used with DeepSeek V4-Pro etc.
> Use it for a downstream model that shares the OpenAI vocab, or for rough size estimates. 
> If using Claude, use the `.ccc` markdown as context. 
> For exact Claude token counts, use Anthropic's `count_tokens` endpoint.
> `tokens.json` carries this caveat inline (`approximate: true` + a `note`).

`ccc tokenize` (or `ccc scan --tokens`) encodes the whole `.ccc` corpus with a
pretrained tiktoken vocabulary (`o200k_base` by default, `--encoding cl100k_base`
also supported) and writes:

```
.ccc/
├── tokens.bin    # little-endian u32 token IDs for every cache file, concatenated
└── tokens.json   # index: encoding, layout, and per-file {offset, len} in tokens
```

Consumers load raw tokens with **no re-tokenization** - read `tokens.bin` as a
`u32` slice and index into it via `tokens.json`. The [`TokenCache`](src/tokenize.rs)
loader does exactly this and every `tokenize` run verifies the persisted stream
decodes back to the byte-identical corpus:

```rust
let cache = codecache::TokenCache::load(project_root)?;
let ids: &[u32] = cache.file("src-main.rs.md").unwrap();    // raw tokens, ready to use
let text = cache.decode(ids)?;                              // optional: back to markdown
```

Token artifacts are derived, so a plain `ccc scan` clears them; re-run with
`--tokens` (or `ccc tokenize`) to refresh.

## Supported languages

Rust, Python, JavaScript, TypeScript (+ TSX), and Go, via
[tree-sitter](https://tree-sitter.github.io/). Unsupported files are skipped;
hidden dirs and common build/vendor dirs (`target`, `node_modules`, …) and
`.gitignore` rules are honored.

Adding a language is a matter of extending `src/languages.rs` (extension map,
grammar, and node-kind sets) - the extractor in `src/extract.rs` is
grammar-agnostic.

## Keeping `.ccc` fresh

Because agents rely on the cache, regenerate it whenever tracked source changes.
A CI step of `ccc check .` fails the build if the cache is out of date.

The bundled workflow [.github/workflows/ccc-update.yaml](.github/workflows/ccc-update.yaml)
automates this: on pushes to `main` (and weekly) it checks each root with
`ccc check --format json`, and if the cache drifted it regenerates and opens a
pull request authored by `CCC-bot`. The check step exposes `stale`,
`changed_files` (JSON array), and `changed_count` as job outputs for downstream
jobs. Edit the `CCC_ROOTS` env var to match your project's cache directories.

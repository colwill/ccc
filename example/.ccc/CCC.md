# ContextCodeCache - agent guide
#
# what:  a GENERATED map of this project's source. Each source file has a
#        `<module>-<file>.<ext>.md` entry listing its constants, functions
#        (L<line>:<col>@name:return), intra-file call graph (refs), and
#        marker notes (TODO/FIXME/...). See the `# files` list below.
# why:   lets agents orient in the codebase cheaply - skim `.ccc` first to
#        find where things live, then open the real source for detail.
#        `tokens.bin`/`tokens.json`, if present, hold this content pre-encoded
#        as APPROXIMATE tiktoken (o200k) ids - for a downstream model that
#        shares that vocabulary, NOT for Claude (different tokenizer; its API
#        takes text, not token ids). Feed Claude the markdown above as text.
# keep-fresh: whenever you change tracked source, regenerate with
#        `ccc scan` (add `--tokens` to refresh the token stream). CI runs
#        `ccc check`, which fails when `.ccc` is out of date.
# do-not-edit: never hand-edit files under `.ccc` - they are overwritten on
#        the next scan. To change the cache, change the source, then rescan.
#
# ContextCodeCache (20260701-13-08-47) UTC
# project: example
# totals: 2 files, 4 funcs, 2 consts, 2 refs, 1 notes
# regenerate: `ccc scan`
# files
    - [src/main.rs](src-main.rs.md) [rust] 2f/1c/1r/0n
    - [src/math.rs](src-math.rs.md) [rust] 2f/1c/1r/1n

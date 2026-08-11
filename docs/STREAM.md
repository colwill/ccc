
# Token Stream (stuffing pre-encoded cache into an LLM)

> **Token stream is not compatible with Anthropic models.** These are **approximate** [tiktoken](https://github.com/openai/tiktoken)
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

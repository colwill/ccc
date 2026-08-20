# Cross-repository calls

A project's calls do not stop at its own checkout. A gateway calls a billing service that lives in
another repository, written in another language, behind an HTTP or gRPC hop. No parser can follow
that call: there is no symbol to resolve, and the code on the other side is not even present.

`ccc` closes the gap with two pieces that fit together:

- **`externals` in `.ccc/map.json`** names the peer repositories.
- **`ccc:serves` / `ccc:calls` comments** name the key both ends agree on.

Matching keys become real edges of the service graph, with a file and line at each end.

## The hints

One spelling, in whatever comment syntax the language already uses. Nothing to install, nothing to
import, and no effect on the build:

```rust
// ccc:serves grpc billing.v1.Charge
pub fn charge(account: &str, amount: u64) -> Result<u64> { … }
```

```go
func Checkout(cart []Item) error {
	// ccc:calls grpc billing.v1.Charge
	return client.Charge(ctx, req)
}
```

```python
# ccc:serves rest POST /v1/refund
@app.route("/v1/refund", methods=["POST"])
def refund():
    ...
```

The grammar is `ccc:<directive> [transport] <key>`:

| part | values |
|---|---|
| directive | `serves` (or `provides`, `handles`) · `calls` (or `consumes`, `uses`) |
| transport | `grpc` `rest` `http` `https` `graphql` `queue` `event` `webhook` `ffi` `cli` `soap` `websocket` `ws` `sql` `rpc` — optional |
| key | everything after that, verbatim: `billing.v1.Charge`, `POST /v1/refund`, `orders.created` |

The **key is the whole mechanism**. Both ends must write the same string; matching ignores case and
surrounding whitespace and nothing else. A `ccc:calls` whose key nothing answers is reported rather
than dropped — that is a typo at one end, or a repository nobody configured.

Placement follows the convention each directive already implies. A directive **above a definition**
belongs to it, which is where `ccc:serves` naturally sits in a handler's doc block; decorators and
attributes in between are stepped over. A directive **inside a body** belongs to the enclosing
function, which is where `ccc:calls` naturally sits, next to the call it describes. A directive more
than ten lines above any definition is treated as file-level rather than being dragged onto whatever
function happens to appear later.

## Naming the peers

```json
{
  "services": {
    "gateway": ["gateway/**"],
    "shared":  ["shared/**"]
  },
  "deps": { "gateway": ["billing"] },
  "externals": {
    "billing": {
      "repo": "acme/billing",
      "lang": "go",
      "path": "../billing"
    },
    "ledger": {
      "repo": "acme/ledger",
      "surface": "https://artifacts.internal/ledger/ccc-surface.json",
      "auth": "env:CCC_TOKEN"
    }
  }
}
```

An external is a service like any other: `deps` may name it, and edges end at it. A name cannot be
both a service and an external — it is either code in this repo or code in another one.

| field | meaning |
|---|---|
| `path` | A directory to parse: a sibling checkout, or another corner of a monorepo. Relative paths resolve against the repo root. |
| `surface` | A file, a directory containing `ccc-surface.json`, or an `http(s)` URL, holding a surface published with `ccc export`. |
| `auth` | `env:VARIABLE` — the variable holding a bearer token for a private URL. Only this form is accepted; a literal token in a file that belongs in git is a mistake, not a feature. |
| `repo` | `owner/repo`, for display. |
| `lang` | The peer's language, for display when no surface is reachable. |

`path` wins when the directory is really there — a checkout is the freshest view of a peer, and in a
monorepo it is the only one. `surface` is the fallback, and the only option for a repository you
cannot or should not clone.

**A peer that cannot be reached is reported, never fatal.** A wrong token, an unreachable host or a
missing directory shows up as `resolved: false` with the reason attached, and the rest of the
analysis runs exactly as before.

## Publishing a surface

```sh
ccc export --name billing --repo acme/billing      # -> .ccc/ccc-surface.json
ccc export --name billing -o -                     # stdout, for a CI artifact
```

A surface is only what a repository publishes and consumes — no bodies, no call graph, no private
symbols:

```json
{
  "schema": "ccc-surface/1",
  "name": "billing",
  "generated": "20260812-18-47-02",
  "repo": "acme/billing",
  "languages": ["go"],
  "provides": [
    { "key": "billing.v1.Charge", "transport": "grpc",
      "function": "Charge", "file": "svc/charge.go", "line": 4 }
  ],
  "consumes": [
    { "key": "ledger.v1.Write", "transport": "grpc",
      "function": "Refund", "file": "svc/charge.go", "line": 11 }
  ]
}
```

It is typically a few hundred bytes. Consuming one costs no clone, no toolchain for that repo's
language, and no access to its source — which is what makes a private peer workable in the first
place. Publish it from CI on merge:

```yaml
- run: ccc export --name billing --repo ${{ github.repository }}
- uses: actions/upload-artifact@v4
  with: { name: ccc-surface, path: .ccc/ccc-surface.json }
```

`consumes` is what lets a repository learn that something out there calls **in** to it — the one
direction no amount of local analysis could ever discover.

## What you get

`ccc changes` and `/insights.json` gain:

- `externals[]` — each peer, how it was reached, and whether it resolved
- `crossings[]` — every matched pair, with the call site here and the handler there
- `edges[]` — crossings folded into the ordinary service graph, `via: "annotation"`

```
gateway -> billing [declared, detected] 2 call site(s)
  gateway/checkout.rs:9 cancel -> POST /v1/refund (svc/charge.go:9) [rest, other repo]
  gateway/checkout.rs:4 checkout -> billing.v1.Charge (svc/charge.go:4) [grpc, other repo]

## external repos (1)
billing (acme/billing) via path ../peer-go - 2 provided, 1 consumed

## unanswered keys (1)
gateway/checkout.rs:13 dangling calls 'nobody.v1.Answers' [grpc] - nothing serves this key
```

Because a crossing is an ordinary edge, everything downstream follows for free: a change in a peer
pulls the calling service into `services_to_test`, and `contract-test` targets rank the boundary
others depend on.

The VS Code extension shows a crossing as a CodeLens above the call — `↑ calls billing in
acme/billing` — and opens the handler when that peer is checked out locally, or says where it lives
when it is not.

## What this does and does not establish

`ccc:serves` and `ccc:calls` are **statements by an author**, not inferences. That is exactly why
they can cross a process boundary when nothing else can — and exactly why they are only as accurate
as the comments. ccc reports them faithfully; it cannot check them.

A crossing carries `via: "annotation"`, ranked above every inferred evidence kind, because a human
stated it. A `declared` dep with no detected calls remains the right shape for a link nobody has
annotated yet.

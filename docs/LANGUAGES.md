# Language support

What grammars/languages the `ccc` static analyser supports

## Status

Every language ccc can analyse.

| | grammar | detect | funcs | consts | calls | imports | types | modules | metrics | x-file | tests |
|---|---|---|---|---|---|---|---|---|---|---|---|
| C          | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | ✅ | ✅ |
| C++        | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| C#         | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Go         | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| JavaScript | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | ✅ | ✅ |
| Odin       | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Protobuf   | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | ✅ | ✅ | n/a | ✅ | ✅ |
| Python     | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | ✅ | ✅ |
| Rust       | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| TypeScript | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| TSX        | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Zig        | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a | ✅ | ✅ | ✅ |

✅ - done 

n/a - the language has no such concept


### Extension mapping

| language | extensions |
|---|---|
| C | `.c` |
| C++ | `.cpp`, `.cc`, `.cxx`, `.c++`, `.hpp`, `.hh`, `.hxx`, `.h++`, `.h` |
| C# | `.cs`, `.csx` |
| Go | `.go` |
| JavaScript | `.js`, `.jsx`, `.mjs`, `.cjs` |
| Odin | `.odin` |
| Protobuf | `.proto` |
| Python | `.py`, `.pyi` |
| Rust | `.rs` |
| TypeScript | `.ts`, `.mts`, `.cts` |
| TSX | `.tsx` |
| Zig | `.zig` |

Anything else is skipped rather than guessed at. `.h` is the one contested extension due to
me not wanting to handle the cases where we're walking a C++ vs C header so it defaults to C++.

Please create a PR to change this if you want to solve that specific case.

## Approximations

Protobuf is a schema, so it is mapped onto the same shapes: a
`message` is a struct, a `service` an interface, each `rpc` a method of its
service taking its request message and returning its response (`stream` is
dropped), and enum values are constants. It has no calls or bodies, so the
calls and metrics columns do not apply. Code in other languages is linked to
each rpc through its generated stubs - see [gRPC without comments](EXTERNALS.md#grpc-without-comments).

One deliberate approximation is worth naming: Zig's `errdefer` counts as a
guard even though it only runs on the error path. Reading a correctly written
cleanup as a leak is the worse of the two errors, and the heuristic is
name-matched with no data flow regardless.

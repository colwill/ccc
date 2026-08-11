# Performance

Same model, repo, prompt (*"generate an architecture diagram of this project"*), 5 runs each.

## Human Acceptability

Visual verification of each output to determine how human readable the results are. If they aren't readable then they aren't useful as architecture diagrams for humans.

| Variant | Accepted rounds | **Rate** |
|---|---|---:|
| A (ccc MCP) | 2, 3, 4, 5 | **80%** (4/5) |
| B | 4 only | 20% (1/5) |
| C (frugal) | 5 only | 20% (1/5) |

## Cost and Human Acceptability Conclusion

Human accepted outputs measured by tokens used

| Variant | Total spent (5 runs) | Accepted | **Tokens per valid output** | vs A |
|---|---:|---:|---:|---:|
| A (ccc MCP) | 5,415,904 | 4 | **1,353,976** | — |
| B | 7,726,770 | 1 | 7,726,770 | **5.71x** |
| C (frugal) | 4,481,014 | 1 | 4,481,014 | **3.31x** |

Requiring both objective correctness *and* human acceptance:

| Variant | Passes both | Tokens per pass | vs A |
|---|---:|---:|---:|
| A | 2 | 2,707,952 | — |
| B | 1 | 7,726,770 | 2.85x |
| C | 1 | 4,481,014 | 1.65x |
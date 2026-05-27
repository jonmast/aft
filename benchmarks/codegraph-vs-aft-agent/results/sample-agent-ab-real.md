# AFT vs CodeGraph agent benchmark

- Model: `opencode-go/deepseek-v4-flash-free`
- Fallback model: `opencode-go/deepseek-v4-pro`
- Corpus: `agent-fixture`
- Timestamp: 2026-05-27T06:56:04.694Z
- Tasks: 1
- Dry run: false

## Summary

| arm | runs | successes | success rate | tokens total | median tokens | median wall ms | p95 wall ms | median tool calls |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| aft | 1 | 1 | 1.000 | 13512 | 13512 | 11469 | 11469 | 1 |
| codegraph | 1 | 1 | 1.000 | 10504 | 10504 | 7119 | 7119 | 2 |

## Per task

| task | arm | kind | status | model | wall ms | tokens | tool calls |
| --- | --- | --- | --- | --- | ---: | ---: | ---: |
| answer-loyalty-discount | aft | answer | PASS | opencode-go/deepseek-v4-flash-free | 11469 | 13512 | 1 |
| answer-loyalty-discount | codegraph | answer | PASS | opencode-go/deepseek-v4-flash-free | 7119 | 10504 | 2 |

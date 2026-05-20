# Go Ecosystem

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Modules

| File                  | Command(s)                                  | Strategy                                                                 |
| --------------------- | ------------------------------------------- | ------------------------------------------------------------------------ |
| `go_cmd.rs`           | `go test` / `go build` / `go vet` / `go <other>` | `GoCommands` sub-enum dispatch; `go test` injects `-json` (NDJSON stream) |
| `go_mod_cmd.rs`       | `go mod tidy` / `download` / `graph`        | Line filter; `tidy` keeps added/removed/upgraded; `graph` dedupes edges, `--deep` opt-out |
| `go_list_cmd.rs`      | `go list [-m] ./...`                        | Inject `-json`, stream-parse JSON object stream, summarize package/module counts; `--deep` opt-out |
| `golangci_cmd.rs`     | `golangci-lint run`                         | Force JSON output (`--out-format=json` v1 / `--output.json.path` v2), cluster by rule |
| `staticcheck_cmd.rs`  | `staticcheck`                               | Inject `-f json` (NDJSON), cluster by rule code, top-5 rules with top-3 samples each |
| `govulncheck_cmd.rs`  | `govulncheck`                               | Inject `-json` (NDJSON tagged union: config/progress/osv/finding), dedupe by OSV id; `--full` opt-out |
| `buf_cmd.rs`          | `buf lint` / `build` / `generate` / `<other>` | `lint` injects `--error-format=json` and clusters; `build` keeps errors; `generate` summarizes plugins + retains failures; other subcommands passthrough |
| `goreleaser_cmd.rs`   | `goreleaser release` / `build` / `check`    | ANSI-strip + line filter; drop `•` progress, keep `▶` section headers, `⨯`/`error:` failures, summary lines |
| `gotestsum_cmd.rs`    | `gotestsum`                                 | Inject `--jsonfile=<tmpfile>`, stream gotestsum's own pretty stdout through, then post-read NDJSON and reuse `go_cmd::filter_go_test_json` for a compressed summary. `--watch` and user-supplied `--jsonfile` paths handled as special cases (passthrough / reuse user file) |

## Sub-enum vs top-level command

- `go <sub>` form → branch on `GoCommands` sub-enum in `src/main.rs` (Test/Build/Vet/Mod/List, `Other(external_subcommand)` fallback)
- Independent binaries (`golangci-lint`, `gotestsum`, `govulncheck`, `staticcheck`, `buf`, `goreleaser`) → top-level `Commands` branches

## TOML filters (line-oriented, no Rust module needed)

- `src/filters/gofmt.toml` — `gofmt -l` → count + first 10 paths
- `src/filters/goimports.toml` — `goimports -l` → count + first 10 paths

## Shared behaviour

- All filter functions are `pub(crate) fn(&str) -> String` for unit testability
- NDJSON parsers tolerate malformed lines (skip on `serde_json::from_str` error)
- Exit codes propagate via `exit_code_from_output`; lint-style tools that signal "issues found" with exit 1 (e.g. `golangci-lint`, `staticcheck`) normalise to 0 so LLM hooks do not abort
- Output truncated to top-N entries with `force_tee_tail_hint` recovery pointer when the full payload was teed

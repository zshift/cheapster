# CLAUDE.md

cheapster is a Rust CLI and TUI that finds the cheapest way to get the materials for FFXIV crafts (market, vendors, gathering with teleport fees, crafting), and generates crafting macros with Raphael. See README.md for user-facing behavior.

## Commands

```sh
cargo build --release
cargo test --release          # always --release: the Raphael solver is very slow in debug
cargo clippy --release --all-targets
cargo fmt                     # rustfmt.toml: max_width = 120
```

Before calling work done: `cargo fmt`, then zero clippy warnings, then all tests passing.

## Layout

- `src/pipeline.rs`: loads the data and runs the planner. Shared by the CLI (`main.rs`) and the TUI.
- `src/planner.rs`: the cost model, plus a local search over whole plans (single-item moves, adding or dropping zones).
- `src/crafting.rs`: Raphael integration (feasibility, macros, HQ-ingredient search) and the per-job solve cache.
- `src/travel.rs`: teleport fee formula and route ordering.
- `src/xivapi.rs`, `src/universalis.rs`, `src/teamcraft.rs`: data sources.
- `src/cache.rs`: all HTTP. Per-host rate limiting, retries with exponential backoff, and the disk cache.
- `src/config.rs`: `config.toml`, edited with `toml_edit` so the user's comments survive.
- `src/view.rs`: builds the display model. `report.rs` (text output) and `tui.rs` (ratatui) only render it.

## Rules

- **HTTP:** always go through `Http` in `cache.rs`; never call reqwest directly. Use `get_json_cached` or `cached` for game data. Use plain `get_json` only for live data: market prices and Teamcraft lists. Never cache those two. Teamcraft lists hold the user's progress counts.
- **Progress messages:** use `http.log(...)`, never `println!` or `eprintln!` outside `main.rs` and `report.rs`. Direct printing corrupts the TUI.
- **Tests must not touch the network or the user's files.** In the TUI, `App::persist` is false in tests, so nothing is written to config. The solve cache writes to a temp directory under `cfg!(test)`. Set `app.clipboard = None` in TUI tests. Don't write tests that start a plan with a market set: that spawns a network thread.
- **TUI input on Windows:** pastes arrive as bursts of key presses, not paste events. Bracketed paste must stay off on Windows, and Esc must not quit the main screen. Route test input through `handle_batch` so it goes the same way as real events.
- **Precedence:** command-line flag > environment variable > config file > default. Resolve it in `PlanArgs` in `main.rs`.
- **Style:** match the existing code, with short doc comments explaining *why*, and no comments for obvious code.

## Dependencies

- New or updated crates follow a 7-day cooldown and vetting policy. Use the `rust-dep-vetting` skill: check versions, pin `Cargo.lock` back for anything inside the cooldown, run `cargo audit`, and review build scripts and proc macros.
- Raphael (`raphael-solver`, `raphael-sim`, `raphael-data`) is a git dependency pinned to an exact commit. If you bump it, review the new code and update `SOLVE_CACHE_VERSION` in `crafting.rs`.

## Git

- Only commit when asked. Check the current branch first. If it's `main`, create a branch before committing.
- End commit messages with the `Co-Authored-By` trailer.

## Environment

The development machine runs Windows. Shell commands run in Git Bash or PowerShell. The CLI prints Unicode (`×`, `→`, `·`).

# cheapster

Works out the cheapest way to get everything you need to craft a set of **Final Fantasy XIV** items.

For every item in the recipe tree, cheapster compares:

- buying it on the **market board** (using live [Universalis](https://universalis.app) prices),
- buying it from an **NPC vendor**,
- **gathering** it yourself, counting the teleport fees to get there,
- **crafting** it from its ingredients.

That covers every tier, from raw materials to the finished item. It then prints a plan: where to teleport and what to gather or buy there, what to buy on the market board (grouped by world), and what to craft, in order. If you add your crafter stats, it also writes **ready-to-paste crafting macros** using the [Raphael](https://github.com/KonaeAkira/raphael-rs) solver.

It can read items from a [Teamcraft](https://ffxivteamcraft.com) list, and it skips anything you've already marked as obtained there.

```
$ cheapster -m Aether "20x Cobalt Ingot"
Cheapest plan for: 20× Cobalt Ingot
Market: Aether · Start: Limsa Lominsa Lower Decks · Teleport fees without discounts

Total: 605 gil
  (market 58 · teleports 547)

Buying the finished items on the market instead:
  • 20× Cobalt Ingot: 19,067 gil

Route
  1. Teleport to Summerford Farms: 145 gil
    • Gather 100× Fire Shard [BTN lv15] at Middle La Noscea (Summerford) (22.6, 19.1)
  2. Teleport to Ceruleum Processing Plant: 402 gil
    • Gather 40× Cobalt Ore [MIN lv50] at Northern Thanalan (Bluefog) (23.4, 24.4)

Market board
  Faerie: 58 gil
    • 56× Iron Ore @ 1 = 58 gil
  Stacks can't be split, so you'll have extra: Iron Ore (20 needed, 56 bought)

Craft (in this order)
  [Smithing] 20× Cobalt Ingot (20 crafts)
```

(The example above was run with `--include-crystals`. By default, shards, crystals and clusters are left out.)

## Install

You need a recent stable Rust toolchain (built and tested with Rust 1.98).

```sh
git clone <this repo>
cd cheapster
cargo build --release
# the binary is target/release/cheapster (cheapster.exe on Windows)
```

Or install it onto your `PATH`:

```sh
cargo install --path .
```

The first run downloads about 30 MB of gathering, vendor and teleport data and caches it (see [Caching](#caching)).

## Quick start

```sh
# Create a config file and set your market (world, data center or region)
cheapster config init
cheapster config path          # open this file and set market = "Crystal", etc.

# Plan from item names...
cheapster "3x Iron Ingot" "Cobalt Ingot:5"

# ...from a file (one item per line, # for comments)...
cheapster -f shopping.txt

# ...or from a Teamcraft list (public list URL, list id, or an import link)
cheapster -t https://ffxivteamcraft.com/list/<id>

# Or use the interactive view
cheapster tui
```

Items can be written as `Iron Ingot`, `3x Iron Ingot`, `3 Iron Ingot`, `Iron Ingot x3`, `Iron Ingot:3`, or as an item id such as `5057:3`. Inexact names are fuzzy-matched, and cheapster tells you which item it picked.

## The interactive view (`cheapster tui`)

Paste a Teamcraft list link, then work through the plan. Clicking an item on the market board copies its name, ready to paste into the in-game market board search (Ctrl+V works in game), and ticks it off. Clicking a crafting macro copies it.

| Key | What it does |
| --- | --- |
| `Ctrl+V` / `p` | Import the Teamcraft link on your clipboard (`p` reads the clipboard directly) |
| `i` | Type the items to make instead (comma-separated, or one per line) |
| `m` | Choose the market from a list of regions, data centers and worlds (type to filter) |
| `c` | Edit crafter and gatherer stats |
| click / `Enter` | Copy the selected item name or macro and tick it off |
| `↑↓` / `j` `k`, mouse wheel | Move |
| `Tab` / `1`–`4`, or click a tab | Switch between Market, Route, Crafts and Summary |
| `r` | Re-plan, e.g. after updating your Teamcraft list |
| `q` / `Ctrl+C` | Quit |

The Market tab groups purchases by world, alphabetically, with your home world last. It also shows how much gil you still have to spend. Mouse capture turns off normal text selection in the terminal; hold Shift while dragging to select text.

`cheapster tui --watch-clipboard` imports any Teamcraft link as soon as you copy it, without pasting.

## Configuration

Settings live in `config.toml`:

- Windows: `%APPDATA%\cheapster\config.toml`
- Elsewhere: `$XDG_CONFIG_HOME/cheapster/config.toml` (or `~/.config/cheapster/config.toml`)
- Override the location with `CHEAPSTER_CONFIG`.

`cheapster config init` writes a commented starter file, and `cheapster config` shows what's currently set. The TUI's market picker and stats form save here too, and they keep your comments intact.

```toml
market = "Crystal"            # world, data center or region
home_world = "Coeurl"         # listed last on the market board
start = "Limsa Lominsa Lower Decks"

craft_infeasible = false      # true: plan crafts your stats can't finish
guarantee_hq = true           # HQ macros hold even if every condition rolls Poor

# Base stats (gear and materia, no food or potions). Jobs: CRP BSM ARM GSM LTW WVR ALC CUL
[crafters.CRP]
level = 100
craftsmanship = 5400
control = 5100
cp = 700
specialist = false            # Heart and Soul / Quick Innovation
manipulation = true           # defaults to true from level 65

# Some items need a minimum perception. Jobs: MIN BTN FSH
[gatherers.BTN]
level = 100
perception = 5100
```

Precedence is: **command-line flag > environment variable > config file > default**. The environment variables are `CHEAPSTER_MARKET`, `CHEAPSTER_HOME_WORLD` and `CHEAPSTER_START`.

**Jobs you leave out:** a crafter you don't list is assumed able to craft its recipes, but gets no macros. A gatherer you don't list is assumed able to gather anything.

## How the plan is chosen

- **Teleport fees** use the game client's own formula with no discounts: no favorite destinations, no Free Company or Grand Company reductions, no tickets. The formula was reverse-engineered by the [FFXIVClientStructs](https://github.com/aers/FFXIVClientStructs) project and the CheapTeleport plugin. Walking between city areas over the aethernet is free, and that includes the housing districts reachable from your start city. For example, the Mist material supplier costs nothing to reach from Limsa. Run `cheapster --fees` to list the fee from your start to every aetheryte.
- **Routes** visit zones in the cheapest order. The fees for each zone are shared by everything you do there, so the planner searches whole plans rather than deciding item by item. That includes adding or dropping entire zones, and buying a finished item instead of its parts.
- **Market purchases** use whole listings, since stacks can't be split, and include the 5% tax. When the market doesn't have enough of something, cheapster buys what's there and tells you how many are still missing.
- **Gathering is free by default.** Pass `--gather-cost <gil>` to charge yourself per gathered item and value your time. `--no-timed` skips timed and legendary nodes, and `--no-gather` turns gathering off.
- **Crafting** is checked against your configured stats: recipe level, stat minimums, and whether Raphael can finish it at all. Crafts you can't do are bought instead. `--craft-infeasible` (or `craft_infeasible = true`) plans them anyway.
- **Why it's bought:** the summary lists each item that's bought because you can't craft or gather it, with the reason in raw stats, for example `your ARM needs +2740 craftsmanship (2000 → 4740)` or `your MIN needs +1090 perception (4000 → 5090)`.

### Crafting macros

With crafter stats configured, each craft in the plan gets macros, split to fit the game's 15-line limit:

- **Final items** aim for HQ. With `guarantee_hq`, the macro reaches full quality even if every condition rolls Poor. cheapster picks how many HQ intermediates to use by minimizing total in-game crafting time: the final macro plus the extra time those HQ intermediates cost. If HQ can't be guaranteed, the summary says how much quality the macro reaches and the HQ chance.
- **Intermediates** get HQ macros only for the copies the final item needs, and quick finish-only macros for the rest.

## Caching

Cached data lives in `%LOCALAPPDATA%\cheapster` on Windows, or `$XDG_CACHE_HOME/cheapster` (or `~/.cache/cheapster`) elsewhere. Override the location with `CHEAPSTER_CACHE_DIR`.

| Data | Cached? |
| --- | --- |
| Market prices | Never: always live |
| Your Teamcraft list | Never: it holds your progress |
| Recipes, item names, teleport tables, gathering and vendor data, the list of worlds | 7 days (`--refresh` re-downloads) |
| Crafting solves | Per job, for that job's current stats only. Changing a job's stats discards its cache. |

Every request is rate limited per host. Requests that fail with rate limiting (429), a server error or a network error are retried with exponential backoff, and the server's `Retry-After` header is used when present.

## Limitations

- **HQ only comes from intermediates you craft.** HQ materials bought on the market aren't considered, so some gear can't reach guaranteed HQ even when buying a few HQ ingredients would get it there.
- **HQ intermediates are made from NQ materials.** That makes the HQ planning one level deep.
- **Fishing spots aren't included** as gathering sources.
- **Vendor quest unlocks are ignored**, and the route doesn't include a trip back to your start.
- **Food and potions** aren't applied to crafter stats.
- **Teleport fees** come from a reverse-engineered formula. They've matched the in-game values checked so far, but they aren't official.
- **Private Teamcraft lists can't be read.** Make the list public, or use an import link.

## Development

```sh
cargo test --release     # the solver is slow in debug builds
cargo clippy --all-targets
cargo fmt                # rustfmt.toml: 120-column lines
```

The code is laid out as follows:

- `src/pipeline.rs`: loads the data and runs the planner. Shared by the command line and the TUI.
- `src/planner.rs`: the cost model and the search over whole plans.
- `src/crafting.rs`: Raphael integration, crafting feasibility, macros, and the solve cache.
- `src/travel.rs`: teleport fees and route ordering.
- `src/xivapi.rs`, `src/universalis.rs`, `src/teamcraft.rs`: data sources.
- `src/cache.rs`: HTTP, rate limiting, retries and the disk cache.
- `src/config.rs`: `config.toml`.
- `src/view.rs`: builds the display data. `src/report.rs` prints it on the command line and `src/tui.rs` draws it in the TUI.

Dependencies are pinned in `Cargo.lock`. Raphael isn't published on crates.io, so its crates are a git dependency pinned to an exact commit.

## Credits

- [Universalis](https://universalis.app): market board prices and the list of worlds.
- [XIVAPI](https://v2.xivapi.com): items, recipes and teleport data.
- [FFXIV Teamcraft](https://ffxivteamcraft.com): lists, plus gathering node, vendor and aetheryte data.
- [Raphael](https://github.com/KonaeAkira/raphael-rs) (Apache-2.0): the crafting simulator and macro solver.
- [FFXIVClientStructs](https://github.com/aers/FFXIVClientStructs) and the CheapTeleport plugin: the reverse-engineered teleport fee formula.

FINAL FANTASY XIV is a registered trademark of Square Enix. cheapster is a fan project and isn't affiliated with Square Enix.

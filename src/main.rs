mod cache;
mod input;
mod planner;
mod report;
mod teamcraft;
mod travel;
mod universalis;
mod xivapi;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cache::Http;
use crate::input::Wanted;
use crate::planner::{HOME, Item, Problem, Settings};
use crate::travel::Travel;
use crate::xivapi::Recipe;

/// Finds the cheapest way to get everything needed to craft FFXIV items: buying from the
/// market board or vendors, gathering it yourself, or crafting intermediates, including the
/// teleport fees for flying around.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Items to make, e.g. "Iron Ingot", "3x Iron Ingot", "Iron Ingot:3" or an item id.
    items: Vec<String>,

    /// Read items from a file, one per line (same formats as above; # starts a comment).
    #[arg(short, long)]
    file: Option<PathBuf>,

    /// A Teamcraft list URL or id, or a Teamcraft import link.
    #[arg(short, long)]
    teamcraft: Option<String>,

    /// World, data center or region to buy from on the market board.
    #[arg(short, long, env = "CHEAPSTER_MARKET", required_unless_present = "fees")]
    market: Option<String>,

    /// Your home world; its market purchases are listed last.
    #[arg(long, env = "CHEAPSTER_HOME_WORLD", default_value = "Coeurl")]
    home_world: Option<String>,

    /// Aetheryte you start at.
    #[arg(long, env = "CHEAPSTER_START", default_value = "Limsa Lominsa Lower Decks")]
    start: String,

    /// Require HQ when buying the finished items themselves.
    #[arg(long)]
    hq: bool,

    /// Never gather; only buy or craft.
    #[arg(long)]
    no_gather: bool,

    /// Skip timed, unspoiled and legendary gathering nodes.
    #[arg(long)]
    no_timed: bool,

    /// Gil to charge yourself per gathered item, to value your time (default: free).
    #[arg(long, default_value_t = 0)]
    gather_cost: u64,

    /// Re-download cached game data (gathering nodes, vendors, teleport tables).
    #[arg(long)]
    refresh: bool,

    /// Print the teleport fee from the start to every aetheryte, then exit.
    #[arg(long)]
    fees: bool,

    /// Plan for shards, crystals and clusters too (by default you're assumed to have enough).
    #[arg(long)]
    include_crystals: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let http = Http::new(args.refresh)?;

    if args.fees {
        let travel = Travel::new(xivapi::teleport_sheets(&http)?);
        let (start, start_name) = find_start(&travel, &args.start)?;
        let mut fees: Vec<(u32, &str)> =
            travel.aetherytes().filter_map(|(t, name)| Some((travel.cost(start, t)?, name))).collect();
        fees.sort();
        println!("Teleport fees from {start_name} (no discounts):");
        for (fee, name) in fees {
            println!("  {:>6}  {name}", report::gil(fee as u64));
        }
        return Ok(());
    }
    let market = args.market.as_deref().unwrap_or_default();

    let mut stock = HashMap::new();
    let mut wanted: Vec<Wanted> = args.items.iter().filter_map(|s| input::parse_entry(s)).collect();
    if let Some(path) = &args.file {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        wanted.extend(text.lines().filter_map(input::parse_entry));
    }
    if let Some(list) = &args.teamcraft {
        let list = teamcraft::load_list(&http, list)?;
        eprintln!(
            "Loaded Teamcraft list \"{}\" ({} items to make, {} ingredients already on hand)",
            list.name,
            list.wanted.len(),
            list.stock.len()
        );
        wanted.extend(list.wanted);
        stock = list.stock;
    }
    if wanted.is_empty() {
        bail!("no items given; pass item names, --file or --teamcraft");
    }

    let mut targets: Vec<(u32, u32)> = Vec::new();
    for w in wanted {
        let (id, qty) = match w {
            Wanted::Id(id, qty) => (id, qty),
            Wanted::Name(name, qty) => (xivapi::find_item(&http, &name)?, qty),
        };
        match targets.iter_mut().find(|(t, _)| *t == id) {
            Some((_, q)) => *q += qty,
            None => targets.push((id, qty)),
        }
    }

    eprintln!("Fetching recipes...");
    let mut recipes = expand_recipes(&http, &targets)?;
    if !args.include_crystals {
        for r in recipes.values_mut().flatten() {
            r.ingredients.retain(|(id, _)| !is_crystal(*id));
        }
    }
    let mut ids: Vec<u32> = targets.iter().map(|(id, _)| *id).collect();
    for list in recipes.values() {
        for r in list {
            ids.extend(r.ingredients.iter().map(|(id, _)| *id));
        }
    }
    ids.sort_unstable();
    ids.dedup();
    let infos = xivapi::items(&http, &ids)?;

    let travel = Travel::new(xivapi::teleport_sheets(&http)?);
    let (start, start_name) = find_start(&travel, &args.start)?;
    let sources = teamcraft::source_index(&http)?;

    eprintln!("Fetching market prices on {market}...");
    let tradable: Vec<u32> = ids.iter().copied().filter(|id| infos.get(id).is_some_and(|i| !i.untradable)).collect();
    let mut listings = universalis::listings(&http, market, &tradable)?;

    let stop_for = |territory: u32| -> Option<u32> {
        let key = travel.stop_key(territory)?;
        let fee = travel.cost(start, key)?;
        Some(if fee == 0 { HOME } else { key })
    };

    let mut items = HashMap::new();
    for id in &ids {
        let mut vendors: Vec<(u32, teamcraft::VendorOffer)> = Vec::new();
        for offer in sources.vendors.get(id).into_iter().flatten() {
            let Some(stop) = stop_for(offer.territory) else { continue };
            match vendors.iter_mut().find(|(s, _)| *s == stop) {
                Some(existing) if existing.1.price <= offer.price => {}
                Some(existing) => *existing = (stop, offer.clone()),
                None => vendors.push((stop, offer.clone())),
            }
        }
        let mut gathers: Vec<(u32, teamcraft::GatherSpot)> = Vec::new();
        if !args.no_gather {
            for spot in sources.gather.get(id).into_iter().flatten() {
                if args.no_timed && (spot.timed || spot.legendary) {
                    continue;
                }
                let Some(stop) = stop_for(spot.territory) else { continue };
                let rank = |s: &teamcraft::GatherSpot| (s.legendary, s.timed, s.level);
                match gathers.iter_mut().find(|(s, _)| *s == stop) {
                    Some(existing) if rank(&existing.1) <= rank(spot) => {}
                    Some(existing) => *existing = (stop, spot.clone()),
                    None => gathers.push((stop, spot.clone())),
                }
            }
        }
        // Keep the search small: only the closest few zones per item matter in practice.
        keep_closest(&mut gathers, &travel, start, 6);
        keep_closest(&mut vendors, &travel, start, 6);
        items.insert(
            *id,
            Item {
                name: infos.get(id).map(|i| i.name.clone()).unwrap_or_else(|| format!("Item #{id}")),
                recipes: dedupe_recipes(recipes.get(id).cloned().unwrap_or_default()),
                listings: listings.remove(id).unwrap_or_default(),
                vendors,
                gathers,
            },
        );
    }
    break_cycles(&mut items, &targets);

    eprintln!("Optimising...");
    let settings = Settings { hq_targets: args.hq, gather_cost: args.gather_cost };
    let problem = Problem::new(items, targets, stock, &travel, start, settings);
    let (plan, eval) = problem.solve();
    report::print(&problem, &plan, &eval, &sources, market, args.home_world.as_deref(), &start_name);
    if !args.include_crystals {
        println!("Shards, crystals and clusters are left out (pass --include-crystals to plan for them).");
    }
    Ok(())
}

/// Fire Shard (2) through Water Cluster (19).
fn is_crystal(id: u32) -> bool {
    (2..=19).contains(&id)
}

fn find_start(travel: &Travel, name: &str) -> Result<(u32, String)> {
    travel.find_aetheryte(name).with_context(|| format!("no aetheryte named \"{name}\""))
}

/// Fetches recipes for the targets and, recursively, for every ingredient.
fn expand_recipes(http: &Http, targets: &[(u32, u32)]) -> Result<HashMap<u32, Vec<Recipe>>> {
    let mut all: HashMap<u32, Vec<Recipe>> = HashMap::new();
    let mut seen: HashSet<u32> = targets.iter().map(|(id, _)| *id).collect();
    let mut frontier: Vec<u32> = seen.iter().copied().collect();
    while !frontier.is_empty() {
        let found = xivapi::recipes(http, &frontier)?;
        frontier.clear();
        for (id, list) in found {
            for r in &list {
                for (ing, _) in &r.ingredients {
                    if seen.insert(*ing) {
                        frontier.push(*ing);
                    }
                }
            }
            all.insert(id, list);
        }
    }
    Ok(all)
}

/// Several jobs often share an identical recipe (e.g. Iron Ingot for BSM and ARM).
fn dedupe_recipes(recipes: Vec<Recipe>) -> Vec<Recipe> {
    let mut out: Vec<Recipe> = Vec::new();
    for r in recipes {
        let mut key = r.ingredients.clone();
        key.sort_unstable();
        let duplicate = out.iter_mut().find(|o| {
            let mut k = o.ingredients.clone();
            k.sort_unstable();
            k == key && o.yields == r.yields
        });
        match duplicate {
            Some(o) if !o.job.contains(&r.job) => o.job = format!("{}/{}", o.job, r.job),
            Some(_) => {}
            None => out.push(r),
        }
    }
    out
}

/// Drops any recipe that would make an item an ingredient of itself.
fn break_cycles(items: &mut HashMap<u32, Item>, targets: &[(u32, u32)]) {
    fn visit(id: u32, items: &mut HashMap<u32, Item>, on_stack: &mut HashSet<u32>, done: &mut HashSet<u32>) {
        if done.contains(&id) || !on_stack.insert(id) {
            return;
        }
        let recipes = items.get(&id).map(|i| i.recipes.clone()).unwrap_or_default();
        let mut keep = Vec::new();
        for r in recipes {
            if r.ingredients.iter().any(|(ing, _)| on_stack.contains(ing)) {
                continue;
            }
            for (ing, _) in &r.ingredients {
                visit(*ing, items, on_stack, done);
            }
            keep.push(r);
        }
        if let Some(item) = items.get_mut(&id) {
            item.recipes = keep;
        }
        on_stack.remove(&id);
        done.insert(id);
    }
    let mut on_stack = HashSet::new();
    let mut done = HashSet::new();
    for (id, _) in targets {
        visit(*id, items, &mut on_stack, &mut done);
    }
}

fn keep_closest<T>(options: &mut Vec<(u32, T)>, travel: &Travel, start: u32, n: usize) {
    options.sort_by_key(|(stop, _)| if *stop == HOME { 0 } else { travel.cost(start, *stop).unwrap_or(u32::MAX) });
    options.truncate(n);
}

//! Loads everything a plan needs and solves it. Shared by the CLI and the TUI.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::cache::Http;
use crate::config::{self, Config};
use crate::crafting::{self, CraftOrder, Feasibility};
use crate::input::{self, Wanted};
use crate::planner::{Choice, Evaluation, HOME, Item, Plan, Problem, Settings};
use crate::teamcraft;
use crate::travel::Travel;
use crate::universalis;
use crate::view::{self, View};
use crate::xivapi::{self, Recipe};

/// How to plan, independent of what to plan for.
#[derive(Clone, Debug)]
pub struct Options {
    pub market: String,
    pub home_world: Option<String>,
    pub start: String,
    pub hq: bool,
    pub no_gather: bool,
    pub no_timed: bool,
    pub gather_cost: u64,
    pub include_crystals: bool,
    /// Crafter stats and crafting preferences from config.toml.
    pub config: Config,
}

/// What to plan for.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub items: Vec<String>,
    pub file: Option<PathBuf>,
    pub teamcraft: Option<String>,
}

pub fn run(http: &Http, opts: &Options, inputs: &Inputs) -> Result<View> {
    let mut stock = HashMap::new();
    let mut wanted: Vec<Wanted> = inputs.items.iter().filter_map(|s| input::parse_entry(s)).collect();
    if let Some(path) = &inputs.file {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        wanted.extend(text.lines().filter_map(input::parse_entry));
    }
    if let Some(list) = &inputs.teamcraft {
        let list = teamcraft::load_list(http, list)?;
        http.log(&format!(
            "Loaded Teamcraft list \"{}\" ({} items to make, {} ingredients already on hand)",
            list.name,
            list.wanted.len(),
            list.stock.len()
        ));
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
            Wanted::Name(name, qty) => (xivapi::find_item(http, &name)?, qty),
        };
        match targets.iter_mut().find(|(t, _)| *t == id) {
            Some((_, q)) => *q += qty,
            None => targets.push((id, qty)),
        }
    }

    http.log("Fetching recipes...");
    let mut recipes = expand_recipes(http, &targets)?;
    if !opts.include_crystals {
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
    let infos = xivapi::items(http, &ids)?;

    let travel = Travel::new(xivapi::teleport_sheets(http)?);
    let (start, start_name) = find_start(&travel, &opts.start)?;
    let sources = teamcraft::source_index(http)?;

    http.log(&format!("Fetching market prices on {}...", opts.market));
    let tradable: Vec<u32> = ids.iter().copied().filter(|id| infos.get(id).is_some_and(|i| !i.untradable)).collect();
    let mut listings = universalis::listings(http, &opts.market, &tradable)?;

    let stop_for = |territory: u32| -> Option<u32> {
        let key = travel.stop_key(territory)?;
        let fee = travel.cost(start, key)?;
        Some(if fee == 0 { HOME } else { key })
    };

    let mut items = HashMap::new();
    // Why items with gathering nodes can't be gathered by you.
    let mut gather_blocked: HashMap<u32, String> = HashMap::new();
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
        let spots = sources.gather.get(id).map(Vec::as_slice).unwrap_or_default();
        let mut blocks: Vec<GatherBlock> = Vec::new();
        if opts.no_gather && !spots.is_empty() {
            blocks.push(GatherBlock::Off);
        }
        for spot in spots.iter().filter(|_| !opts.no_gather) {
            if let Some(block) = gather_block(spot, &opts.config) {
                blocks.push(block);
                continue;
            }
            if opts.no_timed && (spot.timed || spot.legendary) {
                blocks.push(GatherBlock::TimedOnly);
                continue;
            }
            let Some(stop) = stop_for(spot.territory) else {
                blocks.push(GatherBlock::Unreachable);
                continue;
            };
            let rank = |s: &teamcraft::GatherSpot| (s.legendary, s.timed, s.level);
            match gathers.iter_mut().find(|(s, _)| *s == stop) {
                Some(existing) if rank(&existing.1) <= rank(spot) => {}
                Some(existing) => *existing = (stop, spot.clone()),
                None => gathers.push((stop, spot.clone())),
            }
        }
        if gathers.is_empty()
            && let Some(block) = blocks.into_iter().min()
        {
            gather_blocked.insert(*id, block.explain());
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
    let infeasible = drop_infeasible_recipes(http, &mut items, &opts.config);

    http.log("Optimising...");
    let settings = Settings { hq_targets: opts.hq, gather_cost: opts.gather_cost };
    let problem = Problem::new(items, targets, stock, &travel, start, settings);
    let (plan, eval) = problem.solve();

    let crafts = if opts.config.any_crafters() {
        crafting::plan(&craft_orders(&problem, &plan, &eval), &opts.config, &|msg| http.log(msg))
    } else {
        Vec::new()
    };
    let mut view = view::build(&problem, &plan, &eval, &sources, opts, &start_name, &crafts);

    // Explain items you have to buy (or can't get) because you can't craft or gather them.
    for id in &problem.order {
        if eval.demand.get(id).copied().unwrap_or(0) == 0 {
            continue;
        }
        let choice = plan[id];
        if matches!(choice, Choice::Craft(_) | Choice::Gather(_) | Choice::Vendor(_)) {
            continue;
        }
        let mut why = Vec::new();
        if let Some((rid, feasibility)) = infeasible.get(id) {
            http.log(&format!("Working out what {} needs...", problem.items[id].name));
            why.push(format!("can't craft: {}", crafting::explain(&opts.config, *rid, feasibility)));
        }
        if let Some(reason) = gather_blocked.get(id) {
            why.push(format!("can't gather: {reason}"));
        }
        let has_recipe = recipes.contains_key(id);
        let has_node = sources.gather.contains_key(id);
        if why.is_empty() && !has_recipe && !has_node {
            why.push("no recipe or gathering node (a monster drop or other source)".into());
        }
        if !why.is_empty() {
            view.blocked.push(format!("{}: {}", problem.items[id].name, why.join("; ")));
        }
    }
    if !infeasible.is_empty() && !opts.config.craft_infeasible && view.blocked.iter().any(|b| b.contains("can't craft"))
    {
        view.notes
            .push("Set craft_infeasible = true (or press c in the TUI) to plan crafts your stats can't finish.".into());
    }
    if !opts.config.any_crafters() {
        view.notes.push(format!(
            "Add your crafter stats to {} to get crafting macros (run `cheapster config init`).",
            config::path().display()
        ));
    }
    if !opts.include_crystals {
        view.notes
            .push("Shards, crystals and clusters are left out (pass --include-crystals to plan for them).".into());
    }
    if let Err(e) = crafting::save_solve_cache() {
        http.log(&format!("Couldn't save the crafting solve cache: {e:#}"));
    }
    Ok(view)
}

/// Why a gathering node can't be used, most actionable first (the smallest one is reported).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GatherBlock {
    Level { job: &'static str, need: u32, have: u8 },
    Perception { job: &'static str, need: u32, have: u16 },
    TimedOnly,
    Unreachable,
    Off,
}

impl GatherBlock {
    fn explain(&self) -> String {
        match self {
            GatherBlock::Level { job, need, have } => format!("your {job} is level {have} and the node needs {need}"),
            GatherBlock::Perception { job, need, have } => {
                format!("your {job} needs +{} perception ({have} → {need})", need - *have as u32)
            }
            GatherBlock::TimedOnly => "only on timed or legendary nodes, and --no-timed is on".into(),
            GatherBlock::Unreachable => "only in zones you can't teleport to".into(),
            GatherBlock::Off => "gathering is turned off (--no-gather)".into(),
        }
    }
}

/// Whether your configured gatherer meets a node's level and the item's perception minimum.
fn gather_block(spot: &teamcraft::GatherSpot, config: &Config) -> Option<GatherBlock> {
    let job = spot.job();
    let gatherer = config::gatherer_id(job).and_then(|j| config.gatherers[j])?;
    if (gatherer.level as u32) < spot.level {
        return Some(GatherBlock::Level { job, need: spot.level, have: gatherer.level });
    }
    if spot.perception > gatherer.perception as u32 {
        return Some(GatherBlock::Perception { job, need: spot.perception, have: gatherer.perception });
    }
    None
}

/// Removes recipes your configured crafters can't finish (unless `craft_infeasible` is set),
/// and puts the recipe variants you can do first. Returns, per item that can't be crafted at
/// all, one recipe and why.
fn drop_infeasible_recipes(
    http: &Http,
    items: &mut HashMap<u32, Item>,
    config: &Config,
) -> HashMap<u32, (u32, Feasibility)> {
    let mut reasons = HashMap::new();
    if !config.any_crafters() {
        return reasons;
    }
    http.log("Checking which crafts your stats can finish...");
    let mut memo: HashMap<u32, Feasibility> = HashMap::new();
    for (id, item) in items.iter_mut() {
        for recipe in &mut item.recipes {
            for rid in &recipe.ids {
                memo.entry(*rid).or_insert_with(|| crafting::feasibility(config, *rid));
            }
            recipe.ids.sort_by_key(|rid| !memo[rid].possible());
        }
        // After sorting, a recipe's first variant is possible if any of them is.
        let possible = |r: &Recipe| memo[&r.ids[0]].possible();
        if let Some(first) = item.recipes.first()
            && !item.recipes.iter().any(possible)
        {
            reasons.insert(*id, (first.ids[0], memo[&first.ids[0]].clone()));
        }
        if !config.craft_infeasible {
            item.recipes.retain(possible);
        }
    }
    reasons
}

/// The crafts in the solved plan, ingredients first.
fn craft_orders(problem: &Problem, plan: &Plan, eval: &Evaluation) -> Vec<CraftOrder> {
    problem
        .order
        .iter()
        .rev()
        .filter_map(|id| {
            let Choice::Craft(r) = plan[id] else { return None };
            let demand = eval.demand.get(id).copied().unwrap_or(0);
            if demand == 0 {
                return None;
            }
            let item = &problem.items[id];
            let recipe = &item.recipes[r];
            Some(CraftOrder {
                item_id: *id,
                name: item.name.clone(),
                recipe_ids: recipe.ids.clone(),
                crafts: demand.div_ceil(recipe.yields),
                yields: recipe.yields,
                final_item: problem.targets.iter().any(|(t, _)| t == id),
            })
        })
        .collect()
}

/// Fire Shard (2) through Water Cluster (19).
fn is_crystal(id: u32) -> bool {
    (2..=19).contains(&id)
}

pub fn find_start(travel: &Travel, name: &str) -> Result<(u32, String)> {
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
            Some(o) => {
                if !o.job.contains(&r.job) {
                    o.job = format!("{}/{}", o.job, r.job);
                }
                o.ids.extend(r.ids);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Gatherer;

    fn spot(level: u32, perception: u32) -> teamcraft::GatherSpot {
        teamcraft::GatherSpot {
            territory: 1,
            zone: "Somewhere".into(),
            x: 0.0,
            y: 0.0,
            level,
            kind: 2, // logging: BTN
            timed: false,
            legendary: false,
            spawns: Vec::new(),
            duration: 0,
            perception,
        }
    }

    #[test]
    fn gathering_blocks_explain_level_and_perception() {
        let mut config = Config::default();
        // Unconfigured gatherers are assumed to manage anything.
        assert_eq!(gather_block(&spot(100, 5090), &config), None);

        config.gatherers[1] = Some(Gatherer { level: 90, perception: 4800 });
        let level = gather_block(&spot(100, 0), &config).unwrap();
        assert_eq!(level.explain(), "your BTN is level 90 and the node needs 100");

        config.gatherers[1] = Some(Gatherer { level: 100, perception: 4800 });
        let perception = gather_block(&spot(100, 5090), &config).unwrap();
        assert_eq!(perception.explain(), "your BTN needs +290 perception (4800 → 5090)");
        assert_eq!(gather_block(&spot(100, 4800), &config), None);

        // The most actionable reason wins when several nodes are blocked differently.
        assert_eq!(
            [GatherBlock::Unreachable, perception, GatherBlock::Off].into_iter().min().unwrap().explain(),
            "your BTN needs +290 perception (4800 → 5090)"
        );
    }
}

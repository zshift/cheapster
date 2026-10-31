//! Crafting macros via Raphael's solver (https://github.com/KonaeAkira/raphael-rs): checks
//! whether your crafters can finish a recipe, and builds the rotations to paste in game.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use raphael_data::{CrafterStats, Locale, RECIPES, RLVLS, get_game_settings, get_initial_quality, macro_name};
use raphael_sim::{Action, SimulationState};
use raphael_solver::{AtomicFlag, MacroSolver, SolverSettings};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Crafter, JOBS};

/// Game macros hold 15 lines; we use one for a completion notice.
const ACTIONS_PER_MACRO: usize = 14;

/// HQ ingredients to use per craft: (item id, count).
type HqInputs = Vec<(u32, u32)>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Solution {
    pub actions: Vec<Action>,
    /// Final quality, including what HQ ingredients contributed.
    pub quality: u16,
    pub max_quality: u16,
}

impl Solution {
    pub fn is_hq(&self) -> bool {
        self.quality >= self.max_quality
    }

    /// In-game seconds.
    pub fn duration(&self) -> u32 {
        self.actions.iter().map(|a| a.time_cost() as u32).sum()
    }

    /// The rotation as game macros, split to fit the 15-line limit.
    pub fn macros(&self, label: &str) -> Vec<String> {
        let chunks: Vec<&[Action]> = self.actions.chunks(ACTIONS_PER_MACRO).collect();
        let total = chunks.len();
        chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                let mut lines: Vec<String> = chunk
                    .iter()
                    .map(|a| format!("/ac \"{}\" <wait.{}>", macro_name(*a, Locale::EN), a.time_cost()))
                    .collect();
                lines.push(if i + 1 == total {
                    format!("/echo {label} done <se.1>")
                } else {
                    format!("/echo {label} macro {} of {total} done <se.1>", i + 1)
                });
                lines.join("\n")
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Feasibility {
    Ok,
    /// No crafter configured for the job, or the recipe isn't in Raphael's data.
    Unknown,
    TooLowLevel {
        job: &'static str,
        need: u8,
        have: u8,
    },
    /// Below the recipe's minimum craftsmanship or control.
    StatsTooLow {
        job: &'static str,
        need: (u16, u16),
        have: (u16, u16),
    },
    CantFinish {
        job: &'static str,
    },
}

impl Feasibility {
    pub fn possible(&self) -> bool {
        matches!(self, Feasibility::Ok | Feasibility::Unknown)
    }
}

/// Why a recipe can't be crafted, in raw stats: what to raise, and by how much.
/// For "can't finish" this searches for the craftsmanship (or CP) that would, so it takes a
/// few solves.
pub fn explain(config: &Config, recipe_id: u32, why: &Feasibility) -> String {
    match why {
        Feasibility::TooLowLevel { job, need, have } => {
            format!("your {job} is level {have} and the recipe needs {need}")
        }
        Feasibility::StatsTooLow { job, need, have } => {
            let mut gaps = Vec::new();
            for (stat, need, have) in [("craftsmanship", need.0, have.0), ("control", need.1, have.1)] {
                if have < need {
                    gaps.push(format!("+{} {stat} ({have} → {need})", need - have));
                }
            }
            format!("the recipe has stat minimums: your {job} needs {}", gaps.join(" and "))
        }
        Feasibility::CantFinish { job } => {
            let Some(crafter) = crafter_for(config, recipe_id) else { return format!("your {job} can't finish it") };
            let finishes = |c: Crafter| solve(recipe_id, c, None, false).is_some();
            let craftsmanship =
                lowest_passing(crafter.craftsmanship, |v| finishes(Crafter { craftsmanship: v, ..crafter }));
            let cp = lowest_passing(crafter.cp, |v| finishes(Crafter { cp: v, ..crafter }));
            let mut options = Vec::new();
            if let Some(v) = craftsmanship {
                options.push(format!("+{} craftsmanship ({} → {v})", v - crafter.craftsmanship, crafter.craftsmanship));
            }
            if let Some(v) = cp {
                options.push(format!("+{} CP ({} → {v})", v - crafter.cp, crafter.cp));
            }
            match options.len() {
                0 => format!("your {job} can't finish it, even with double the craftsmanship or CP"),
                _ => format!("your {job} can't finish it: it needs about {}", options.join(" or ")),
            }
        }
        Feasibility::Ok | Feasibility::Unknown => "it can be crafted".into(),
    }
}

/// Roughly the smallest value above `current` (searched up to double) for which `passes` holds,
/// within 10 above the true threshold.
fn lowest_passing(current: u16, passes: impl Fn(u16) -> bool) -> Option<u16> {
    let mut low = current;
    let mut high = current.saturating_mul(2).max(current.saturating_add(100));
    if !passes(high) {
        return None;
    }
    while high - low > 10 {
        let mid = low + (high - low) / 2;
        if passes(mid) { high = mid } else { low = mid }
    }
    Some(high)
}

pub fn job_of(recipe_id: u32) -> Option<&'static str> {
    RECIPES.get(recipe_id).map(|r| JOBS[r.job_id as usize])
}

pub fn crafter_for(config: &Config, recipe_id: u32) -> Option<Crafter> {
    RECIPES.get(recipe_id).and_then(|r| config.crafters[r.job_id as usize])
}

/// Whether the configured crafter for this recipe's job can finish it at all.
pub fn feasibility(config: &Config, recipe_id: u32) -> Feasibility {
    let Some(recipe) = RECIPES.get(recipe_id) else { return Feasibility::Unknown };
    let job = JOBS[recipe.job_id as usize];
    let Some(crafter) = config.crafters[recipe.job_id as usize] else { return Feasibility::Unknown };
    // Level-scaling recipes (e.g. Cosmic Exploration) adapt to the crafter instead.
    if recipe.max_level_scaling == 0 {
        let need = RLVLS[recipe.recipe_level as usize].job_level;
        if crafter.level < need {
            return Feasibility::TooLowLevel { job, need, have: crafter.level };
        }
    }
    if crafter.craftsmanship < recipe.req_craftsmanship || crafter.control < recipe.req_control {
        return Feasibility::StatsTooLow {
            job,
            need: (recipe.req_craftsmanship, recipe.req_control),
            have: (crafter.craftsmanship, crafter.control),
        };
    }
    match solve(recipe_id, crafter, None, false) {
        Some(_) => Feasibility::Ok,
        None => Feasibility::CantFinish { job },
    }
}

fn stats(c: Crafter) -> CrafterStats {
    CrafterStats {
        craftsmanship: c.craftsmanship,
        control: c.control,
        cp: c.cp,
        level: c.level,
        manipulation: c.manipulation,
        heart_and_soul: c.specialist,
        quick_innovation: c.specialist,
    }
}

/// Solves a rotation. With `initial_quality: None` it only needs to finish the craft;
/// otherwise it aims for full quality starting from that much (from HQ ingredients).
/// `guarantee` makes the quality hold even if every condition is Poor.
///
/// Results are cached on disk per job, for that job's current stats only (see `SolveCache`).
pub fn solve(recipe_id: u32, crafter: Crafter, initial_quality: Option<u16>, guarantee: bool) -> Option<Solution> {
    let job = RECIPES.get(recipe_id)?.job_id;
    // Guarantee only changes quality-seeking solves.
    let guarantee = guarantee && initial_quality.is_some();
    let key = format!("{recipe_id}:{initial_quality:?}:{guarantee}");
    cached_solve(job, crafter, &key, || solve_uncached(recipe_id, crafter, initial_quality, guarantee))
}

/// Bump to throw away every cached solve (e.g. when moving to a new Raphael version).
const SOLVE_CACHE_VERSION: &str = "raphael-723b17e/1";

/// One job's cached solves, valid only for the stats they were solved with.
#[derive(Serialize, Deserialize)]
struct JobSolves {
    version: String,
    crafter: Crafter,
    /// `None` records that the recipe couldn't be finished with these stats.
    solves: HashMap<String, Option<Solution>>,
}

impl JobSolves {
    fn new(crafter: Crafter) -> Self {
        Self { version: SOLVE_CACHE_VERSION.into(), crafter, solves: HashMap::new() }
    }

    fn valid_for(&self, crafter: Crafter) -> bool {
        self.version == SOLVE_CACHE_VERSION && self.crafter == crafter
    }
}

/// Solves are deterministic, so they're reused until a job's stats change; then that job's
/// cache is replaced (other jobs keep theirs). Kept in memory and written by `save_solve_cache`.
#[derive(Default)]
struct SolveCache {
    jobs: HashMap<u8, JobSolves>,
    dirty: HashSet<u8>,
}

static SOLVES: Mutex<Option<SolveCache>> = Mutex::new(None);

fn solves_dir() -> PathBuf {
    if cfg!(test) {
        // Never read or overwrite the real cache from tests.
        std::env::temp_dir().join(format!("cheapster-test-solves-{}", std::process::id()))
    } else {
        crate::cache::cache_dir().join("solves")
    }
}

fn solves_file(job: u8) -> PathBuf {
    solves_dir().join(format!("{}.json", JOBS[job as usize]))
}

fn cached_solve(job: u8, crafter: Crafter, key: &str, compute: impl FnOnce() -> Option<Solution>) -> Option<Solution> {
    {
        let mut guard = SOLVES.lock().unwrap_or_else(|e| e.into_inner());
        let cache = guard.get_or_insert_with(SolveCache::default);
        let entry = cache.jobs.entry(job).or_insert_with(|| {
            fs::read_to_string(solves_file(job))
                .ok()
                .and_then(|text| serde_json::from_str::<JobSolves>(&text).ok())
                .unwrap_or_else(|| JobSolves::new(crafter))
        });
        if !entry.valid_for(crafter) {
            // Stats changed: drop this job's old solves (the file is replaced on save).
            *entry = JobSolves::new(crafter);
            cache.dirty.insert(job);
        }
        if let Some(hit) = entry.solves.get(key) {
            return hit.clone();
        }
    }
    // Solve without holding the lock: it can take seconds.
    let result = compute();
    let mut guard = SOLVES.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(SolveCache::default);
    if let Some(entry) = cache.jobs.get_mut(&job)
        && entry.valid_for(crafter)
    {
        entry.solves.insert(key.to_string(), result.clone());
        cache.dirty.insert(job);
    }
    result
}

/// Writes any new or invalidated solves to disk.
pub fn save_solve_cache() -> Result<()> {
    let mut guard = SOLVES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(cache) = guard.as_mut() else { return Ok(()) };
    let dir = solves_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for job in cache.dirty.drain() {
        if let Some(entry) = cache.jobs.get(&job) {
            let path = solves_file(job);
            fs::write(&path, serde_json::to_string(entry)?).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

fn solve_uncached(recipe_id: u32, crafter: Crafter, initial_quality: Option<u16>, guarantee: bool) -> Option<Solution> {
    let recipe = *RECIPES.get(recipe_id)?;
    let mut settings = get_game_settings(recipe, None, stats(crafter), None, None);
    let max_quality = settings.max_quality;
    match initial_quality {
        None => settings.max_quality = 0,
        Some(initial) => {
            settings.max_quality = max_quality.saturating_sub(initial);
            settings.adversarial = guarantee;
        }
    }
    let solver_settings = SolverSettings { simulator_settings: settings, allow_non_max_quality_solutions: true };
    let mut solver = MacroSolver::new(solver_settings, Box::new(|_| {}), Box::new(|_| {}), AtomicFlag::new());
    let actions = solver.solve().ok()?;
    let state = SimulationState::from_macro(&settings, &actions).ok()?;
    if u32::from(state.progress) < u32::from(settings.max_progress) {
        return None;
    }
    let quality = match initial_quality {
        None => 0,
        Some(initial) => (initial as u32 + state.quality as u32).min(max_quality as u32) as u16,
    };
    Some(Solution { actions, quality, max_quality })
}

/// Starting quality a craft gets from `hq` HQ copies of each ingredient (by item id).
pub fn initial_quality(recipe_id: u32, crafter: Crafter, hq: &[(u32, u32)]) -> u16 {
    let Some(recipe) = RECIPES.get(recipe_id) else { return 0 };
    let mut slots = [0u8; 6];
    for (i, ingredient) in recipe.ingredients.iter().enumerate().take(6) {
        if let Some((_, n)) = hq.iter().find(|(id, _)| *id == ingredient.item_id) {
            slots[i] = (*n).min(ingredient.amount) as u8;
        }
    }
    get_initial_quality(stats(crafter), *recipe, slots)
}

/// One crafted item in the plan, as the planner decided it.
#[derive(Clone, Debug)]
pub struct CraftOrder {
    pub item_id: u32,
    pub name: String,
    /// Candidate game recipes (same ingredients, different jobs).
    pub recipe_ids: Vec<u32>,
    pub crafts: u32,
    pub yields: u32,
    /// Final items are made HQ; intermediates only as needed for that.
    pub final_item: bool,
}

/// Macros and HQ requirements for one crafted item.
#[derive(Clone, Debug)]
pub struct CraftPlan {
    pub item_id: u32,
    pub job: Option<&'static str>,
    /// How many of the crafts must come out HQ.
    pub hq_crafts: u32,
    pub nq: Option<Solution>,
    pub hq: Option<Solution>,
    /// HQ ingredients (item id, count) to put into each HQ craft.
    pub hq_inputs: Vec<(u32, u32)>,
    pub notes: Vec<String>,
}

/// Works out macros for every crafted item. Final items aim for guaranteed HQ, using the
/// combination of HQ intermediates that minimises total in-game crafting time; everything else
/// gets a quick finish-only macro. Intermediates are made HQ from NQ materials (one level deep).
///
/// `orders` must list ingredients before the items that use them.
pub fn plan(orders: &[CraftOrder], config: &Config, log: &dyn Fn(&str)) -> Vec<CraftPlan> {
    let guarantee = config.guarantee_hq;
    let chosen: Vec<Option<(u32, Crafter)>> =
        orders.iter().map(|o| o.recipe_ids.iter().find_map(|id| crafter_for(config, *id).map(|c| (*id, c)))).collect();
    let mut plans: Vec<CraftPlan> = orders
        .iter()
        .zip(&chosen)
        .map(|(o, c)| CraftPlan {
            item_id: o.item_id,
            job: c.map(|(id, _)| id).or_else(|| o.recipe_ids.first().copied()).and_then(job_of),
            hq_crafts: 0,
            nq: None,
            hq: None,
            hq_inputs: Vec::new(),
            notes: Vec::new(),
        })
        .collect();
    let index_of = |item: u32| orders.iter().position(|o| o.item_id == item);

    // Finish-only macros for everything we have a crafter for.
    for (i, o) in orders.iter().enumerate() {
        match chosen[i] {
            Some((rid, crafter)) => {
                log(&format!("Solving macros: {}...", o.name));
                plans[i].nq = solve(rid, crafter, None, false);
                if plans[i].nq.is_none() {
                    plans[i].notes.push("your stats can't finish this craft".into());
                }
            }
            None => {
                let job = plans[i].job.unwrap_or("this job");
                plans[i].notes.push(format!("no {job} stats in config.toml, so no macro"));
            }
        }
    }

    // HQ macros for intermediates, from NQ materials. Solved lazily, only when a final item
    // could use them.
    let mut hq_intermediate: Vec<Option<Option<Solution>>> = vec![None; orders.len()];

    for t in 0..orders.len() {
        if !orders[t].final_item {
            continue;
        }
        let Some((rid, crafter)) = chosen[t] else { continue };
        let Some(recipe) = RECIPES.get(rid) else { continue };
        log(&format!("Solving HQ macro: {}...", orders[t].name));
        let crafts = orders[t].crafts;

        // Ingredients we craft ourselves and can make HQ: (item, max per craft, extra secs per HQ item).
        let mut slots: Vec<(u32, u32, f64)> = Vec::new();
        for ing in recipe.ingredients.iter().filter(|g| g.item_id != 0) {
            let Some(j) = index_of(ing.item_id) else { continue };
            let (Some((jrid, jcrafter)), Some(nq)) = (chosen[j], plans[j].nq.as_ref()) else { continue };
            let hq = hq_intermediate[j].get_or_insert_with(|| {
                log(&format!("Solving HQ macro: {}...", orders[j].name));
                solve(jrid, jcrafter, Some(0), guarantee).filter(Solution::is_hq)
            });
            if let Some(hq) = hq {
                let extra_per_craft = hq.duration() as f64 - nq.duration() as f64;
                slots.push((ing.item_id, ing.amount, extra_per_craft.max(0.0) / orders[j].yields as f64));
            }
        }

        let Some(base) = solve(rid, crafter, Some(0), guarantee) else {
            plans[t].notes.push("couldn't solve an HQ rotation".into());
            continue;
        };
        // The most quality the rotation itself can add; mixes starting lower can't reach HQ.
        let gain = base.quality;
        let needed = base.max_quality.saturating_sub(gain);

        let mut combos: Vec<(HqInputs, u16, f64)> = Vec::new();
        for counts in combinations(&slots.iter().map(|s| s.1).collect::<Vec<_>>()) {
            let hq: Vec<(u32, u32)> = slots.iter().zip(&counts).map(|(s, n)| (s.0, *n)).collect();
            let q = initial_quality(rid, crafter, &hq);
            if q < needed {
                continue;
            }
            let extra: f64 = slots.iter().zip(&counts).map(|(s, n)| s.2 * (*n * crafts) as f64).sum();
            combos.push((hq, q, extra));
        }
        combos.sort_by(|a, b| a.2.total_cmp(&b.2).then(b.1.cmp(&a.1)));

        // (hq inputs, solution, total seconds)
        let mut best: Option<(HqInputs, Solution, f64)> = None;
        if base.is_hq() {
            best = Some((Vec::new(), base.clone(), (base.duration() * crafts) as f64));
        }
        let mut solved: Vec<(u16, Option<Solution>)> = Vec::new();
        for (hq, q, extra) in combos {
            if best.as_ref().is_some_and(|b| b.2 <= extra) {
                break;
            }
            let solution = match solved.iter().find(|(sq, _)| *sq == q) {
                Some((_, s)) => s.clone(),
                None => {
                    let s = solve(rid, crafter, Some(q), guarantee);
                    solved.push((q, s.clone()));
                    s
                }
            };
            let Some(solution) = solution.filter(Solution::is_hq) else { continue };
            let total = extra + (solution.duration() * crafts) as f64;
            if best.as_ref().is_none_or(|b| total < b.2) {
                best = Some((hq, solution, total));
            }
        }

        let (hq_inputs, solution) = match best {
            Some((hq, s, _)) => (hq, s),
            None => {
                // Can't reach HQ: use every HQ intermediate available for the best quality.
                let all: Vec<(u32, u32)> = slots.iter().map(|s| (s.0, s.1)).collect();
                let q = initial_quality(rid, crafter, &all);
                let s = solve(rid, crafter, Some(q), guarantee).unwrap_or(base);
                let pct = raphael_data::hq_percentage(s.quality, s.max_quality).unwrap_or(0);
                plans[t].notes.push(format!(
                    "can't guarantee HQ with your stats: reaches {}/{} quality ({pct}% HQ chance)",
                    s.quality, s.max_quality
                ));
                (all.into_iter().filter(|(_, n)| *n > 0).collect(), s)
            }
        };
        for (item, per_craft) in hq_inputs.iter().filter(|(_, n)| *n > 0) {
            if let Some(j) = index_of(*item) {
                let hq_crafts = (per_craft * crafts).div_ceil(orders[j].yields).min(orders[j].crafts);
                plans[j].hq_crafts = (plans[j].hq_crafts + hq_crafts).min(orders[j].crafts);
                if plans[j].hq.is_none() {
                    plans[j].hq = hq_intermediate[j].clone().flatten();
                }
            }
        }
        plans[t].hq_inputs = hq_inputs.into_iter().filter(|(_, n)| *n > 0).collect();
        plans[t].hq_crafts = crafts;
        plans[t].hq = Some(solution);
    }
    plans
}

/// Every vector `v` with `0 <= v[i] <= max[i]`, capped so huge recipes stay tractable.
fn combinations(max: &[u32]) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new()];
    for &m in max {
        let step = if out.len() * (m as usize + 1) > 20_000 { (m / 4).max(1) } else { 1 };
        let values: Vec<u32> = (0..=m).step_by(step as usize).chain(std::iter::once(m)).collect();
        out = out
            .into_iter()
            .flat_map(|prefix| {
                let mut values = values.clone();
                values.dedup();
                values.into_iter().map(move |v| {
                    let mut p = prefix.clone();
                    p.push(v);
                    p
                })
            })
            .collect();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const BIS: Crafter =
        Crafter { level: 100, craftsmanship: 5400, control: 5100, cp: 700, specialist: false, manipulation: true };

    fn recipe_for(item_id: u32) -> u32 {
        RECIPES.entries().find(|(_, r)| r.item_id == item_id).map(|(id, _)| id).expect("recipe")
    }

    #[test]
    fn solves_low_level_recipe_and_formats_macros() {
        // Iron Ingot (SMN lv 16).
        let recipe = recipe_for(5057);
        let finish = solve(recipe, BIS, None, false).unwrap();
        assert!(!finish.actions.is_empty());
        let hq = solve(recipe, BIS, Some(0), true).unwrap();
        assert!(hq.is_hq());
        let macros = hq.macros("Iron Ingot");
        assert!(macros[0].starts_with("/ac \""));
        assert!(macros.iter().all(|m| m.lines().count() <= 15));
    }

    #[test]
    fn solves_are_cached_per_job_for_current_stats_only() {
        // A low-level CUL recipe: no other test uses CUL, so nothing else touches its cache.
        let recipe = RECIPES
            .entries()
            .find(|(_, r)| r.job_id == 7 && r.max_level_scaling == 0 && RLVLS[r.recipe_level as usize].job_level <= 10)
            .map(|(id, _)| id)
            .unwrap();
        let first =
            Crafter { level: 50, craftsmanship: 500, control: 500, cp: 300, specialist: false, manipulation: false };
        let a = solve(recipe, first, None, false).expect("solvable");
        let again = solve(recipe, first, None, false).unwrap();
        assert_eq!(a.actions, again.actions);
        let cached_keys = |crafter: Crafter| {
            let guard = SOLVES.lock().unwrap();
            let entry = &guard.as_ref().unwrap().jobs[&7];
            (entry.crafter == crafter).then_some(entry.solves.len())
        };
        assert_eq!(cached_keys(first), Some(1));

        // New stats for the job replace its cache instead of adding to it.
        let second = Crafter { cp: 350, ..first };
        solve(recipe, second, None, false).unwrap();
        assert_eq!(cached_keys(second), Some(1));
        assert_eq!(cached_keys(first), None);

        save_solve_cache().unwrap();
        let saved: JobSolves = serde_json::from_str(&fs::read_to_string(solves_file(7)).unwrap()).unwrap();
        assert!(saved.valid_for(second) && saved.solves.len() == 1);
    }

    #[test]
    fn lowest_passing_finds_the_threshold() {
        let found = lowest_passing(100, |v| v >= 157).unwrap();
        assert!((157..167).contains(&found), "{found}");
        assert_eq!(lowest_passing(100, |v| v >= 1000), None);
    }

    #[test]
    fn explains_stat_minimums_in_raw_numbers() {
        let why = Feasibility::StatsTooLow { job: "ARM", need: (4500, 4200), have: (4100, 4300) };
        assert_eq!(
            explain(&Config::default(), 0, &why),
            "the recipe has stat minimums: your ARM needs +400 craftsmanship (4100 → 4500)"
        );
    }

    #[test]
    fn explains_how_much_more_it_takes_to_finish() {
        let recipe = recipe_for(42893); // Archeo Kingdom Cuirass of Fending, level 100
        let job = RECIPES.get(recipe).unwrap().job_id as usize;
        let mut config = Config::default();
        config.crafters[job] = Some(Crafter { craftsmanship: 2000, cp: 300, ..BIS });
        let why = feasibility(&config, recipe);
        assert!(matches!(why, Feasibility::CantFinish { .. } | Feasibility::StatsTooLow { .. }), "{why:?}");
        let text = explain(&config, recipe, &why);
        assert!(text.contains('+') && text.contains("craftsmanship"), "{text}");
    }

    #[test]
    fn level_too_low_is_infeasible() {
        let mut config = Config::default();
        let recipe = recipe_for(5057);
        let job = RECIPES.get(recipe).unwrap().job_id as usize;
        config.crafters[job] = Some(Crafter { level: 5, ..BIS });
        assert!(matches!(feasibility(&config, recipe), Feasibility::TooLowLevel { .. }));
        config.crafters[job] = Some(BIS);
        assert_eq!(feasibility(&config, recipe), Feasibility::Ok);
        assert_eq!(feasibility(&Config::default(), recipe), Feasibility::Unknown);
    }

    /// Not a correctness test: prints solve times for a level 100 recipe.
    #[test]
    #[ignore]
    fn timing() {
        for item in [44001u32, 42893] {
            let recipe = recipe_for(item); // Ra'Kaznar Ingot, Archeo Kingdom Cuirass of Fending
            println!("item {item}");
            for (label, iq, g) in [("finish", None, false), ("hq", Some(0), false), ("hq guaranteed", Some(0), true)] {
                let t = Instant::now();
                let s = solve(recipe, BIS, iq, g);
                println!(
                    "{label}: {:?} -> {:?}",
                    t.elapsed(),
                    s.map(|s| (s.actions.len(), s.duration(), s.quality, s.max_quality))
                );
            }
        }
    }
}

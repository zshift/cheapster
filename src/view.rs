//! Turns a solved plan into display-ready pieces shared by the text report and the TUI.

use std::collections::BTreeMap;

use crate::planner::{Choice, Evaluation, HOME, Plan, Problem};
use crate::teamcraft::{GatherSpot, SourceIndex};
use crate::universalis;

pub struct View {
    /// "Cheapest plan for: ..."
    pub title: String,
    /// Market, start and fee assumptions.
    pub context: String,
    pub total: String,
    pub breakdown: String,
    pub finished: Vec<String>,
    pub finished_total: Option<String>,
    pub on_hand: Vec<String>,
    pub route: Vec<Stop>,
    /// Alphabetical by world, home world last.
    pub market: Vec<WorldGroup>,
    pub overbuys: Vec<String>,
    pub crafts: Vec<String>,
    pub shortfalls: Vec<Shortfall>,
    pub notes: Vec<String>,
}

pub struct Stop {
    pub heading: String,
    pub errands: Vec<String>,
}

pub struct WorldGroup {
    pub world: String,
    pub home: bool,
    pub lines: Vec<MarketLine>,
}

impl WorldGroup {
    pub fn subtotal(&self) -> u64 {
        self.lines.iter().map(|l| l.cost).sum()
    }

    pub fn heading(&self) -> String {
        format!("{}{}", self.world, if self.home { " (home)" } else { "" })
    }
}

pub struct MarketLine {
    pub item: String,
    pub quantity: u32,
    pub hq: bool,
    pub price_per_unit: u64,
    /// Including tax.
    pub cost: u64,
}

impl MarketLine {
    pub fn describe(&self) -> String {
        format!(
            "{}{}× {} @ {} = {} gil",
            self.quantity,
            if self.hq { " HQ" } else { "" },
            self.item,
            gil(self.price_per_unit),
            gil(self.cost)
        )
    }
}

/// An item the market can't fully supply and nothing else provides.
pub struct Shortfall {
    pub item: String,
    pub summary: String,
    pub listings: Option<String>,
}

pub fn build(
    problem: &Problem,
    plan: &Plan,
    eval: &Evaluation,
    sources: &SourceIndex,
    market: &str,
    home_world: Option<&str>,
    start: &str,
) -> View {
    let demand = |id: &u32| eval.demand.get(id).copied().unwrap_or(0);
    let needed: Vec<u32> = problem.order.iter().copied().filter(|id| demand(id) > 0).collect();
    let name = |id: u32| problem.items[&id].name.clone();

    let wanted: Vec<String> = problem.targets.iter().map(|(id, q)| format!("{q}× {}", name(*id))).collect();
    let title = format!("Cheapest plan for: {}", wanted.join(", "));
    let context = format!("Market: {market} · Start: {start} · Teleport fees without discounts");

    let total = if eval.missing > 0 {
        format!("Total: {} gil, but {} unit(s) aren't available anywhere", gil(eval.total), eval.missing)
    } else {
        format!("Total: {} gil", gil(eval.total))
    };
    let mut parts = vec![format!("market {}", gil(eval.market))];
    if eval.vendor > 0 {
        parts.push(format!("vendors {}", gil(eval.vendor)));
    }
    if eval.travel > 0 {
        parts.push(format!("teleports {}", gil(eval.travel)));
    }
    if eval.gather_time > 0 {
        parts.push(format!("gathering time {}", gil(eval.gather_time)));
    }
    let breakdown = format!("({})", parts.join(" · "));

    let mut finished = Vec::new();
    let mut finished_sum = Some(0);
    for (id, q) in &problem.targets {
        let cost = problem.market_cost(*id, *q);
        finished.push(match cost {
            Some(c) => format!("{q}× {}: {} gil", name(*id), gil(c)),
            None => format!("{q}× {}: not enough on the market", name(*id)),
        });
        finished_sum = finished_sum.zip(cost).map(|(a, b)| a + b);
    }
    let finished_total = finished_sum.filter(|_| problem.targets.len() > 1).map(|t| format!("Total: {} gil", gil(t)));

    let on_hand = problem
        .order
        .iter()
        .filter_map(|id| eval.from_stock.get(id).map(|qty| format!("{qty}× {}", name(*id))))
        .collect();

    // Errands at each stop, in route order.
    let mut errands: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut first_spot: BTreeMap<u32, (f64, f64)> = BTreeMap::new();
    for id in &needed {
        let item = &problem.items[id];
        let d = demand(id);
        match plan[id] {
            Choice::Vendor(i) => {
                let (stop, offer) = &item.vendors[i];
                first_spot.entry(*stop).or_insert((offer.x, offer.y));
                errands.entry(*stop).or_default().push(format!(
                    "Buy {d}× {} from {} ({} {:.1}, {:.1}) @ {} = {} gil",
                    item.name,
                    offer.npc,
                    offer.zone,
                    offer.x,
                    offer.y,
                    gil(offer.price),
                    gil(offer.price * d as u64)
                ));
            }
            Choice::Gather(i) => {
                let (stop, spot) = &item.gathers[i];
                first_spot.entry(*stop).or_insert((spot.x, spot.y));
                errands.entry(*stop).or_default().push(format!(
                    "Gather {d}× {} [{} lv{}] at {} ({:.1}, {:.1}){}",
                    item.name,
                    spot.job(),
                    spot.level,
                    spot.zone,
                    spot.x,
                    spot.y,
                    node_notes(spot)
                ));
            }
            _ => {}
        }
    }
    let mut route = Vec::new();
    if let Some(list) = errands.remove(&HOME) {
        route.push(Stop { heading: format!("At {start} (no teleport needed)"), errands: list });
    }
    let (_, legs) = problem.travel.route(problem.start, &eval.stops);
    for (n, (stop, fee)) in legs.iter().enumerate() {
        let aetheryte = nearest_aetheryte(sources, problem, *stop, first_spot.get(stop).copied());
        route.push(Stop {
            heading: format!("{}. Teleport to {aetheryte}: {} gil", n + 1, gil(*fee as u64)),
            errands: errands.remove(stop).unwrap_or_default(),
        });
    }

    // Items the market can't fully supply are listed as shortfalls instead.
    let mut by_world: BTreeMap<String, Vec<MarketLine>> = BTreeMap::new();
    let mut overbuys = Vec::new();
    for id in &needed {
        if plan[id] != Choice::Market {
            continue;
        }
        let item = &problem.items[id];
        let d = demand(id);
        let Some(purchase) = universalis::buy(&item.listings, d, problem.hq_only(*id)) else { continue };
        for l in &purchase.picks {
            by_world.entry(l.world_name.clone().unwrap_or_else(|| market.to_string())).or_default().push(MarketLine {
                item: item.name.clone(),
                quantity: l.quantity,
                hq: l.hq,
                price_per_unit: l.price_per_unit,
                cost: l.cost(),
            });
        }
        if purchase.units > d {
            overbuys.push(format!("{} ({} needed, {} bought)", item.name, d, purchase.units));
        }
    }
    let is_home = |w: &str| home_world.is_some_and(|h| h.eq_ignore_ascii_case(w));
    let mut market_groups: Vec<WorldGroup> =
        by_world.into_iter().map(|(world, lines)| WorldGroup { home: is_home(&world), world, lines }).collect();
    market_groups.sort_by_key(|g| g.home);

    let crafts = needed
        .iter()
        .rev()
        .filter_map(|id| {
            let Choice::Craft(r) = plan[id] else { return None };
            let item = &problem.items[id];
            let recipe = &item.recipes[r];
            let d = demand(id);
            let times = d.div_ceil(recipe.yields);
            let makes = if recipe.yields > 1 { format!(" → {} made", times * recipe.yields) } else { String::new() };
            Some(format!(
                "[{}] {}× {} ({times} craft{}){makes}",
                recipe.job,
                d,
                item.name,
                if times == 1 { "" } else { "s" }
            ))
        })
        .collect();

    let shortfalls = needed
        .iter()
        .filter(|id| match plan[*id] {
            Choice::Unsourced => true,
            Choice::Market => problem.market_cost(**id, demand(id)).is_none(),
            _ => false,
        })
        .map(|id| {
            let item = &problem.items[id];
            let d = demand(id);
            let hq = problem.hq_only(*id);
            let mut available: Vec<_> = item.listings.iter().filter(|l| !hq || l.hq).collect();
            available.sort_by_key(|l| l.price_per_unit);
            let units: u32 = available.iter().map(|l| l.quantity).sum();
            if units == 0 {
                return Shortfall {
                    item: item.name.clone(),
                    summary: format!("{d}× {}: none listed", item.name),
                    listings: None,
                };
            }
            let cost: u64 = available.iter().map(|l| l.cost()).sum();
            let picks: Vec<String> = available
                .iter()
                .map(|l| {
                    let world = l.world_name.as_deref().unwrap_or(market);
                    format!("{world} {}{} @ {}", l.quantity, if l.hq { " HQ" } else { "" }, gil(l.price_per_unit))
                })
                .collect();
            Shortfall {
                item: item.name.clone(),
                summary: format!(
                    "{d}× {}: only {units} listed, buy them all for {} gil and find {} more",
                    item.name,
                    gil(cost),
                    d - units
                ),
                listings: Some(picks.join(", ")),
            }
        })
        .collect();

    View {
        title,
        context,
        total,
        breakdown,
        finished,
        finished_total,
        on_hand,
        route,
        market: market_groups,
        overbuys,
        crafts,
        shortfalls,
        notes: Vec::new(),
    }
}

fn node_notes(spot: &GatherSpot) -> String {
    let mut notes = Vec::new();
    if spot.timed {
        if spot.spawns.is_empty() {
            notes.push("timed".to_string());
        } else {
            let times: Vec<String> = spot.spawns.iter().map(|h| format!("{h:02}:00")).collect();
            notes.push(format!("timed: ET {}", times.join("/")));
        }
    }
    if spot.legendary {
        notes.push("legendary: needs folklore tome".to_string());
    }
    if notes.is_empty() { String::new() } else { format!(" [{}]", notes.join(", ")) }
}

fn nearest_aetheryte(sources: &SourceIndex, problem: &Problem, stop: u32, near: Option<(f64, f64)>) -> String {
    let in_zone = sources.aetherytes.iter().filter(|a| a.territory == stop && !a.name.is_empty());
    let best = match near {
        Some((x, y)) => in_zone.min_by(|a, b| {
            let da = (a.x - x).powi(2) + (a.y - y).powi(2);
            let db = (b.x - x).powi(2) + (b.y - y).powi(2);
            da.total_cmp(&db)
        }),
        None => in_zone.into_iter().next(),
    };
    best.map(|a| a.name.clone())
        .or_else(|| problem.travel.aetheryte_name(stop).map(str::to_string))
        .unwrap_or_else(|| format!("territory #{stop}"))
}

pub fn gil(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

//! Prints a plan as a shopping list, travel route and crafting order.

use std::collections::BTreeMap;

use crate::planner::{Choice, Evaluation, HOME, Plan, Problem};
use crate::teamcraft::{GatherSpot, SourceIndex};
use crate::universalis;

pub fn print(
    problem: &Problem,
    plan: &Plan,
    eval: &Evaluation,
    sources: &SourceIndex,
    market: &str,
    home_world: Option<&str>,
    start: &str,
) {
    let demand = |id: &u32| eval.demand.get(id).copied().unwrap_or(0);
    let needed: Vec<u32> = problem.order.iter().copied().filter(|id| demand(id) > 0).collect();
    let name = |id: u32| problem.items[&id].name.as_str();

    let wanted: Vec<String> = problem.targets.iter().map(|(id, q)| format!("{q}× {}", name(*id))).collect();
    println!("Cheapest plan for: {}", wanted.join(", "));
    println!("Market: {market} · Start: {start} · Teleport fees without discounts\n");

    if eval.missing > 0 {
        println!(
            "Total: {} gil, but {} unit(s) aren't available anywhere (see the end)",
            gil(eval.total),
            eval.missing
        );
    } else {
        println!("Total: {} gil", gil(eval.total));
    }
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
    println!("  ({})", parts.join(" · "));

    println!("\nBuying the finished items on the market instead:");
    let mut finished_total = Some(0);
    for (id, q) in &problem.targets {
        let cost = problem.market_cost(*id, *q);
        match cost {
            Some(c) => println!("  • {q}× {}: {} gil", name(*id), gil(c)),
            None => println!("  • {q}× {}: not enough on the market", name(*id)),
        }
        finished_total = finished_total.zip(cost).map(|(a, b)| a + b);
    }
    if let (Some(total), true) = (finished_total, problem.targets.len() > 1) {
        println!("  Total: {} gil", gil(total));
    }
    println!();

    let from_stock: Vec<(&u32, &u32)> =
        problem.order.iter().filter_map(|id| eval.from_stock.get_key_value(id)).collect();
    if !from_stock.is_empty() {
        println!("Already on hand (marked obtained in Teamcraft), not bought:");
        for (id, qty) in from_stock {
            println!("  • {qty}× {}", name(*id));
        }
        println!();
    }

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

    println!("Route");
    if let Some(list) = errands.get(&HOME) {
        println!("  At {start} (no teleport needed)");
        for e in list {
            println!("    • {e}");
        }
    }
    let (_, legs) = problem.travel.route(problem.start, &eval.stops);
    if legs.is_empty() && !errands.contains_key(&HOME) {
        println!("  No travel needed.");
    }
    for (n, (stop, fee)) in legs.iter().enumerate() {
        let aetheryte = nearest_aetheryte(sources, problem, *stop, first_spot.get(stop).copied());
        println!("  {}. Teleport to {aetheryte}: {} gil", n + 1, gil(*fee as u64));
        for e in errands.get(stop).into_iter().flatten() {
            println!("    • {e}");
        }
    }
    println!();

    // Items the market can't fully supply are listed in their own section below.
    let buys: Vec<&u32> = needed
        .iter()
        .filter(|id| plan[*id] == Choice::Market && problem.market_cost(**id, demand(id)).is_some())
        .collect();
    if !buys.is_empty() {
        println!("Market board");
        // world -> (subtotal, lines)
        let mut by_world: BTreeMap<String, (u64, Vec<String>)> = BTreeMap::new();
        let mut overbuys = Vec::new();
        for id in buys {
            let item = &problem.items[id];
            let d = demand(id);
            let Some(purchase) = universalis::buy(&item.listings, d, problem.hq_only(*id)) else { continue };
            for l in &purchase.picks {
                let world = l.world_name.clone().unwrap_or_else(|| market.to_string());
                let hq = if l.hq { " HQ" } else { "" };
                let entry = by_world.entry(world).or_default();
                entry.0 += l.cost();
                entry.1.push(format!(
                    "{}{hq}× {} @ {} = {} gil",
                    l.quantity,
                    item.name,
                    gil(l.price_per_unit),
                    gil(l.cost())
                ));
            }
            if purchase.units > d {
                overbuys.push(format!("{} ({} needed, {} bought)", item.name, d, purchase.units));
            }
        }
        // Alphabetical, with the home world last so cross-world shopping comes first.
        let is_home = |w: &str| home_world.is_some_and(|h| h.eq_ignore_ascii_case(w));
        let mut worlds: Vec<_> = by_world.into_iter().collect();
        worlds.sort_by_key(|(w, _)| is_home(w));
        for (world, (subtotal, lines)) in worlds {
            let home = if is_home(&world) { " (home)" } else { "" };
            println!("  {world}{home}: {} gil", gil(subtotal));
            for line in lines {
                println!("    • {line}");
            }
        }
        if !overbuys.is_empty() {
            println!("  Stacks can't be split, so you'll have extra: {}", overbuys.join(", "));
        }
        println!();
    }

    let crafts: Vec<&u32> = needed.iter().rev().filter(|id| matches!(plan[*id], Choice::Craft(_))).collect();
    if !crafts.is_empty() {
        println!("Craft (in this order)");
        for id in crafts {
            let item = &problem.items[id];
            let Choice::Craft(r) = plan[id] else { unreachable!() };
            let recipe = &item.recipes[r];
            let d = demand(id);
            let times = d.div_ceil(recipe.yields);
            let makes = if recipe.yields > 1 { format!(" → {} made", times * recipe.yields) } else { String::new() };
            println!(
                "  [{}] {}× {} ({times} craft{}){makes}",
                recipe.job,
                d,
                item.name,
                if times == 1 { "" } else { "s" }
            );
        }
        println!();
    }

    let unsourced: Vec<&u32> = needed
        .iter()
        .filter(|id| match plan[*id] {
            Choice::Unsourced => true,
            Choice::Market => problem.market_cost(**id, demand(id)).is_none(),
            _ => false,
        })
        .collect();
    if !unsourced.is_empty() {
        println!("Not enough on the market (and no vendor or gathering node), e.g. monster drops:");
        for id in unsourced {
            let item = &problem.items[id];
            let d = demand(id);
            let hq = problem.hq_only(*id);
            let mut available: Vec<_> = item.listings.iter().filter(|l| !hq || l.hq).collect();
            available.sort_by_key(|l| l.price_per_unit);
            let units: u32 = available.iter().map(|l| l.quantity).sum();
            if units == 0 {
                println!("  {d}× {}: none listed", item.name);
                continue;
            }
            let cost: u64 = available.iter().map(|l| l.cost()).sum();
            println!(
                "  {d}× {}: only {units} listed, buy them all for {} gil and find {} more",
                item.name,
                gil(cost),
                d - units
            );
            let picks: Vec<String> = available
                .iter()
                .map(|l| {
                    let world = l.world_name.as_deref().unwrap_or(market);
                    format!("{world} {}{} @ {}", l.quantity, if l.hq { " HQ" } else { "" }, gil(l.price_per_unit))
                })
                .collect();
            println!("      {}", picks.join(", "));
        }
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

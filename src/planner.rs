//! Picks, for every item in the recipe tree, whether to buy it on the market, buy it from a
//! vendor, gather it, or craft it, minimising total gil including teleport fees.
//!
//! Teleport fees are shared by everything done in the same zone, so the choices aren't
//! independent. We run a local search over whole plans: single-item changes, plus moves that
//! add or drop an entire zone from the route.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::teamcraft::{GatherSpot, VendorOffer};
use crate::travel::Travel;
use crate::universalis::{self, Listing};
use crate::xivapi::Recipe;

/// Stop id used for anything reachable without teleporting from the start.
pub const HOME: u32 = 0;

pub struct Item {
    pub name: String,
    pub recipes: Vec<Recipe>,
    pub listings: Vec<Listing>,
    pub vendors: Vec<(u32, VendorOffer)>,
    pub gathers: Vec<(u32, GatherSpot)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Market,
    Vendor(usize),
    Gather(usize),
    Craft(usize),
    Unsourced,
}

pub struct Settings {
    pub hq_targets: bool,
    /// Extra gil charged per gathered unit, to account for time spent.
    pub gather_cost: u64,
}

pub struct Problem<'a> {
    pub items: HashMap<u32, Item>,
    /// Topological order: every item appears before its ingredients.
    pub order: Vec<u32>,
    pub targets: Vec<(u32, u32)>,
    pub travel: &'a Travel,
    pub start: u32,
    pub settings: Settings,
    /// Items already on hand (e.g. marked obtained in Teamcraft), used before anything else.
    pub stock: HashMap<u32, u32>,
    market_memo: RefCell<HashMap<(u32, u32), (u64, u32)>>,
    route_memo: RefCell<HashMap<Vec<u32>, u64>>,
}

pub type Plan = HashMap<u32, Choice>;

#[derive(Clone, Debug)]
pub struct Evaluation {
    /// Units we found no way to get; minimised before cost.
    pub missing: u64,
    pub total: u64,
    pub market: u64,
    pub vendor: u64,
    pub gather_time: u64,
    pub travel: u64,
    pub demand: HashMap<u32, u32>,
    pub stops: Vec<u32>,
    /// How much of each item comes out of `stock`.
    pub from_stock: HashMap<u32, u32>,
}

impl Evaluation {
    fn key(&self) -> (u64, u64) {
        (self.missing, self.total)
    }
}

impl<'a> Problem<'a> {
    pub fn new(
        items: HashMap<u32, Item>,
        targets: Vec<(u32, u32)>,
        stock: HashMap<u32, u32>,
        travel: &'a Travel,
        start: u32,
        settings: Settings,
    ) -> Self {
        let order = topo_order(&items, &targets);
        Self {
            items,
            order,
            targets,
            travel,
            start,
            settings,
            stock,
            market_memo: RefCell::default(),
            route_memo: RefCell::default(),
        }
    }

    fn is_target(&self, id: u32) -> bool {
        self.targets.iter().any(|(t, _)| *t == id)
    }

    pub fn hq_only(&self, id: u32) -> bool {
        self.settings.hq_targets && self.is_target(id)
    }

    /// Cost of buying `qty` on the market, if there's enough supply.
    pub fn market_cost(&self, id: u32, qty: u32) -> Option<u64> {
        match self.market_partial(id, qty) {
            (cost, 0) => Some(cost),
            _ => None,
        }
    }

    /// Cost of buying as much of `qty` as the market has, and how many units are still missing.
    pub fn market_partial(&self, id: u32, qty: u32) -> (u64, u32) {
        if let Some(c) = self.market_memo.borrow().get(&(id, qty)) {
            return *c;
        }
        let item = &self.items[&id];
        let hq = self.hq_only(id);
        let result = match universalis::buy(&item.listings, qty, hq) {
            Some(p) => (p.cost, 0),
            None => {
                let available = item.listings.iter().filter(|l| !hq || l.hq);
                let (cost, units) = available.fold((0, 0), |(c, u), l| (c + l.cost(), u + l.quantity));
                (cost, qty - units)
            }
        };
        self.market_memo.borrow_mut().insert((id, qty), result);
        result
    }

    pub fn route_cost(&self, stops: &[u32]) -> u64 {
        if let Some(c) = self.route_memo.borrow().get(stops) {
            return *c;
        }
        let (cost, _) = self.travel.route(self.start, stops);
        self.route_memo.borrow_mut().insert(stops.to_vec(), cost);
        cost
    }

    pub fn stop_of(&self, id: u32, choice: Choice) -> Option<u32> {
        let item = &self.items[&id];
        match choice {
            Choice::Vendor(i) => Some(item.vendors[i].0),
            Choice::Gather(i) => Some(item.gathers[i].0),
            _ => None,
        }
    }

    pub fn evaluate(&self, plan: &Plan) -> Evaluation {
        let mut demand: HashMap<u32, u32> = HashMap::new();
        for (id, qty) in &self.targets {
            *demand.entry(*id).or_default() += qty;
        }
        let mut eval = Evaluation {
            missing: 0,
            total: 0,
            market: 0,
            vendor: 0,
            gather_time: 0,
            travel: 0,
            demand: HashMap::new(),
            stops: Vec::new(),
            from_stock: HashMap::new(),
        };
        let mut stops = HashSet::new();
        for id in &self.order {
            let mut d = demand.get(id).copied().unwrap_or(0);
            let have = self.stock.get(id).copied().unwrap_or(0).min(d);
            if have > 0 {
                eval.from_stock.insert(*id, have);
                d -= have;
                demand.insert(*id, d);
            }
            if d == 0 {
                continue;
            }
            let item = &self.items[id];
            let choice = plan[id];
            match choice {
                Choice::Craft(r) => {
                    let recipe = &item.recipes[r];
                    let crafts = d.div_ceil(recipe.yields);
                    for (ing, amount) in &recipe.ingredients {
                        *demand.entry(*ing).or_default() += crafts * amount;
                    }
                }
                Choice::Market => {
                    let (cost, missing) = self.market_partial(*id, d);
                    eval.market += cost;
                    eval.missing += missing as u64;
                }
                Choice::Vendor(i) => eval.vendor += item.vendors[i].1.price * d as u64,
                Choice::Gather(_) => eval.gather_time += self.settings.gather_cost * d as u64,
                Choice::Unsourced => eval.missing += d as u64,
            }
            if let Some(stop) = self.stop_of(*id, choice)
                && stop != HOME
            {
                stops.insert(stop);
            }
        }
        let mut stops: Vec<u32> = stops.into_iter().collect();
        stops.sort_unstable();
        eval.travel = self.route_cost(&stops);
        eval.total = eval.market + eval.vendor + eval.gather_time + eval.travel;
        eval.demand = demand;
        eval.stops = stops;
        eval
    }

    fn options(&self, id: u32) -> Vec<Choice> {
        let item = &self.items[&id];
        let mut out = Vec::new();
        if !item.listings.is_empty() {
            out.push(Choice::Market);
        }
        out.extend((0..item.vendors.len()).map(Choice::Vendor));
        out.extend((0..item.gathers.len()).map(Choice::Gather));
        out.extend((0..item.recipes.len()).map(Choice::Craft));
        if out.is_empty() {
            out.push(Choice::Unsourced);
        }
        out
    }

    /// Bottom-up per-unit estimate, used to seed the search. With `free_travel`, zones cost
    /// nothing to reach, which biases towards gathering everything.
    fn seed(&self, free_travel: bool) -> Plan {
        let mut unit: HashMap<u32, f64> = HashMap::new();
        let mut plan = Plan::new();
        for id in self.order.iter().rev() {
            let item = &self.items[id];
            let mut best = (f64::INFINITY, Choice::Unsourced);
            for choice in self.options(*id) {
                let reachable = |stop: u32| free_travel || stop == HOME;
                let cost = match choice {
                    Choice::Market => {
                        let hq = self.hq_only(*id);
                        item.listings
                            .iter()
                            .filter(|l| !hq || l.hq)
                            .map(|l| l.cost() as f64 / l.quantity as f64)
                            .fold(f64::INFINITY, f64::min)
                    }
                    Choice::Vendor(i) if reachable(item.vendors[i].0) => item.vendors[i].1.price as f64,
                    Choice::Gather(i) if reachable(item.gathers[i].0) => self.settings.gather_cost as f64,
                    Choice::Craft(r) => {
                        let recipe = &item.recipes[r];
                        let sum: f64 = recipe
                            .ingredients
                            .iter()
                            .map(|(ing, amt)| unit.get(ing).copied().unwrap_or(f64::INFINITY) * *amt as f64)
                            .sum();
                        sum / recipe.yields as f64
                    }
                    _ => f64::INFINITY,
                };
                if cost < best.0 {
                    best = (cost, choice);
                }
            }
            if best.1 == Choice::Unsourced {
                // Nothing reachable without travel: fall back to the first real option.
                best.1 = self.options(*id)[0];
            }
            unit.insert(*id, best.0);
            plan.insert(*id, best.1);
        }
        plan
    }

    pub fn solve(&self) -> (Plan, Evaluation) {
        // Also start from "buy the finished items", which the other seeds can miss when
        // only a whole-subtree change beats them.
        let mut buy_finished = self.seed(false);
        for (id, _) in &self.targets {
            if !self.items[id].listings.is_empty() {
                buy_finished.insert(*id, Choice::Market);
            }
        }
        let mut best: Option<(Plan, Evaluation)> = None;
        for seed in [self.seed(false), self.seed(true), buy_finished] {
            let (plan, eval) = self.local_search(seed);
            if best.as_ref().is_none_or(|(_, b)| eval.key() < b.key()) {
                best = Some((plan, eval));
            }
        }
        best.unwrap()
    }

    fn local_search(&self, mut plan: Plan) -> (Plan, Evaluation) {
        let mut eval = self.evaluate(&plan);
        let all_stops: Vec<u32> = {
            let mut s: HashSet<u32> = HashSet::new();
            for item in self.items.values() {
                s.extend(item.vendors.iter().map(|v| v.0));
                s.extend(item.gathers.iter().map(|g| g.0));
            }
            s.remove(&HOME);
            s.into_iter().collect()
        };

        for _round in 0..50 {
            let mut improved = self.single_moves(&mut plan, &mut eval, None);

            // Drop a zone: move everything sourced there to its best alternative.
            for stop in eval.stops.clone() {
                let mut candidate = plan.clone();
                let users: Vec<u32> = candidate
                    .iter()
                    .filter(|(id, c)| self.stop_of(**id, **c) == Some(stop))
                    .map(|(id, _)| *id)
                    .collect();
                for id in users {
                    let mut best: Option<(Choice, (u64, u64))> = None;
                    for opt in self.options(id) {
                        if self.stop_of(id, opt) == Some(stop) {
                            continue;
                        }
                        candidate.insert(id, opt);
                        let k = self.evaluate(&candidate).key();
                        if best.is_none_or(|(_, bk)| k < bk) {
                            best = Some((opt, k));
                        }
                    }
                    match best {
                        Some((opt, _)) => candidate.insert(id, opt),
                        None => candidate.insert(id, plan[&id]),
                    };
                }
                // Dropping a zone often only pays off once parents change too (e.g. buying a
                // finished item instead of the raw materials we no longer gather).
                let mut ce = self.evaluate(&candidate);
                self.single_moves(&mut candidate, &mut ce, Some(stop));
                if ce.key() < eval.key() {
                    plan = candidate;
                    eval = ce;
                    improved = true;
                }
            }

            // Add a zone: source everything possible there, then undo the parts that don't pay.
            for stop in &all_stops {
                if eval.stops.contains(stop) {
                    continue;
                }
                let mut candidate = plan.clone();
                let mut switched = Vec::new();
                for (id, item) in &self.items {
                    // Items no longer reachable from the targets (e.g. ingredients of a recipe
                    // dropped as uncraftable) aren't part of any plan.
                    if !candidate.contains_key(id) {
                        continue;
                    }
                    let option = item
                        .gathers
                        .iter()
                        .position(|g| g.0 == *stop)
                        .map(Choice::Gather)
                        .or_else(|| item.vendors.iter().position(|v| v.0 == *stop).map(Choice::Vendor));
                    if let Some(opt) = option {
                        switched.push((*id, candidate.insert(*id, opt).unwrap()));
                    }
                }
                let mut ce = self.evaluate(&candidate);
                for (id, previous) in switched {
                    let current = candidate.insert(id, previous).unwrap();
                    let reverted = self.evaluate(&candidate);
                    if reverted.key() <= ce.key() {
                        ce = reverted;
                    } else {
                        candidate.insert(id, current);
                    }
                }
                self.single_moves(&mut candidate, &mut ce, None);
                if ce.key() < eval.key() {
                    plan = candidate;
                    eval = ce;
                    improved = true;
                }
            }

            if !improved {
                break;
            }
        }
        (plan, eval)
    }

    /// Repeatedly changes one item's source at a time while that lowers the total.
    /// Options at `banned` are never picked.
    fn single_moves(&self, plan: &mut Plan, eval: &mut Evaluation, banned: Option<u32>) -> bool {
        let mut improved = false;
        loop {
            let mut changed = false;
            for id in &self.order {
                if eval.demand.get(id).copied().unwrap_or(0) == 0 {
                    continue;
                }
                let current = plan[id];
                for opt in self.options(*id) {
                    if opt == current || (banned.is_some() && self.stop_of(*id, opt) == banned) {
                        continue;
                    }
                    plan.insert(*id, opt);
                    let e = self.evaluate(plan);
                    if e.key() < eval.key() {
                        *eval = e;
                        changed = true;
                        break;
                    }
                    plan.insert(*id, current);
                }
            }
            if !changed {
                return improved;
            }
            improved = true;
        }
    }
}

/// Orders items so each appears before all of its ingredients. Recipes that would form a
/// cycle are dropped by the caller beforehand, but we guard against them anyway.
fn topo_order(items: &HashMap<u32, Item>, targets: &[(u32, u32)]) -> Vec<u32> {
    fn visit(id: u32, items: &HashMap<u32, Item>, state: &mut HashMap<u32, u8>, out: &mut Vec<u32>) {
        match state.get(&id) {
            Some(_) => return,
            None => state.insert(id, 1),
        };
        if let Some(item) = items.get(&id) {
            for recipe in &item.recipes {
                for (ing, _) in &recipe.ingredients {
                    if state.get(ing) != Some(&1) {
                        visit(*ing, items, state, out);
                    }
                }
            }
        }
        state.insert(id, 2);
        out.push(id);
    }
    let mut state = HashMap::new();
    let mut out = Vec::new();
    for (id, _) in targets {
        visit(*id, items, &mut state, &mut out);
    }
    out.reverse();
    out
}

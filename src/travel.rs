//! Teleport fees (no discounts) and route planning between zones.
//!
//! The fee formula is the client's, as reverse-engineered in FFXIVClientStructs
//! (`Telepo.GetTeleportCost`) and the CheapTeleport plugin:
//! within one aetherstream plane the fee is `100 + distance * max(1000, expA + expB + 600) / 5000`;
//! crossing planes goes through a relay pair with a fixed surcharge; anything over 1000 gil
//! grows at half rate.

use std::collections::HashMap;

use crate::xivapi::{TeleportSheets, TerritoryTelepo};

/// Housing districts are reachable from their city's aethernet for free, and cost a quarter
/// of the city fee when teleporting from elsewhere.
const RESIDENTIAL: &[(u32, u32)] = &[
    (339, 129), // Mist -> Limsa Lominsa Lower Decks
    (340, 132), // The Lavender Beds -> New Gridania
    (341, 130), // The Goblet -> Ul'dah - Steps of Nald
    (641, 628), // Shirogane -> Kugane
    (979, 418), // Empyreum -> Foundation
];

pub struct Travel {
    sheets: TeleportSheets,
    /// Territory -> aethernet group, for territories that are part of a city network.
    groups: HashMap<u32, u32>,
    /// Territories that have a teleportable aetheryte.
    teleportable: HashMap<u32, String>,
}

impl Travel {
    pub fn new(sheets: TeleportSheets) -> Self {
        let mut groups = HashMap::new();
        let mut teleportable = HashMap::new();
        for a in &sheets.aetherytes {
            if a.group != 0 {
                groups.insert(a.territory, a.group);
            }
            if a.is_aetheryte && !a.name.is_empty() && sheets.telepo.contains_key(&a.territory) {
                teleportable.entry(a.territory).or_insert_with(|| a.name.clone());
            }
        }
        for (district, city) in RESIDENTIAL {
            if let Some(g) = groups.get(city).copied() {
                groups.insert(*district, g);
            }
        }
        Self { sheets, groups, teleportable }
    }

    /// Finds the territory of the aetheryte whose name contains `name` (case-insensitive).
    pub fn find_aetheryte(&self, name: &str) -> Option<(u32, String)> {
        let needle = name.to_lowercase();
        let mut matches: Vec<(&u32, &String)> =
            self.teleportable.iter().filter(|(_, n)| n.to_lowercase().contains(&needle)).collect();
        matches.sort_by_key(|(t, n)| (n.len(), **t));
        matches.first().map(|(t, n)| (**t, (*n).clone()))
    }

    /// Every territory with a teleportable aetheryte, with that aetheryte's name.
    pub fn aetherytes(&self) -> impl Iterator<Item = (u32, &str)> {
        self.teleportable.iter().map(|(t, n)| (*t, n.as_str()))
    }

    pub fn aetheryte_name(&self, territory: u32) -> Option<&str> {
        self.teleportable.get(&territory).map(String::as_str)
    }

    fn same_city(&self, a: u32, b: u32) -> bool {
        a == b || matches!((self.groups.get(&a), self.groups.get(&b)), (Some(x), Some(y)) if x == y)
    }

    /// Whether `territory` can be reached at all (directly or via a city's aethernet).
    pub fn reachable(&self, territory: u32) -> bool {
        self.teleportable.contains_key(&territory) || self.city_aetheryte(territory).is_some()
    }

    /// The teleport destination for a territory: itself, or the main aetheryte of its city.
    pub fn destination(&self, territory: u32) -> Option<u32> {
        if self.teleportable.contains_key(&territory) { Some(territory) } else { self.city_aetheryte(territory) }
    }

    /// Identifies where you have to go to act in `territory`: residential districts are kept
    /// as-is (they have their own fee), anything else maps to its teleport destination.
    pub fn stop_key(&self, territory: u32) -> Option<u32> {
        if RESIDENTIAL.iter().any(|(d, _)| *d == territory) {
            return self.reachable(territory).then_some(territory);
        }
        self.destination(territory)
    }

    fn city_aetheryte(&self, territory: u32) -> Option<u32> {
        let group = self.groups.get(&territory)?;
        self.sheets
            .aetherytes
            .iter()
            .filter(|a| a.is_aetheryte && a.group == *group && self.teleportable.contains_key(&a.territory))
            .map(|a| a.territory)
            .min()
    }

    /// Gil cost to get from territory `from` to territory `to`, with no discounts.
    pub fn cost(&self, from: u32, to: u32) -> Option<u32> {
        if self.same_city(from, to) {
            return Some(0);
        }
        if let Some((_, city)) = RESIDENTIAL.iter().find(|(d, _)| *d == to) {
            return self.cost(from, *city).map(|c| c / 4);
        }
        let to = self.destination(to)?;
        let from = if self.sheets.telepo.contains_key(&from) { from } else { self.destination(from)? };
        self.teleport_fee(from, to)
    }

    fn teleport_fee(&self, from: u32, to: u32) -> Option<u32> {
        let exit = self.sheets.telepo.get(&from)?;
        let entry = self.sheets.telepo.get(&to)?;
        let cost = if exit.relay == entry.relay {
            self.raw_cost(exit, entry)?
        } else {
            let relay = self.sheets.relays.get(&entry.relay)?.relays.get(exit.relay as usize)?;
            let relay_enter = self.sheets.telepo.get(&relay.enter)?;
            let relay_exit = self.sheets.telepo.get(&relay.exit)?;
            self.raw_cost(relay_enter, entry)? + relay.cost as f64 + self.raw_cost(exit, relay_exit)?
        };
        let cost = if cost > 1000.0 { (cost - 1000.0) / 2.0 + 1000.0 } else { cost };
        Some(cost as u32)
    }

    fn raw_cost(&self, exit: &TerritoryTelepo, entry: &TerritoryTelepo) -> Option<f64> {
        let wrap = self.sheets.relays.get(&entry.relay)?.wrap;
        let mut dx = (exit.x - entry.x).abs();
        if dx > wrap && dx < 2 * wrap {
            dx = 2 * wrap - dx;
        }
        let dy = exit.y - entry.y;
        let dist = ((dx as f64).powi(2) + (dy as f64).powi(2)).sqrt();
        let scale = f64::max(1000.0, (entry.expansion + exit.expansion) as f64 + 600.0) / 5000.0;
        Some((100.0 + dist * scale).floor())
    }

    /// Cheapest order to visit every stop starting from `start` (no return trip).
    /// Returns the total fee and the ordered stops with the fee paid to reach each.
    pub fn route(&self, start: u32, stops: &[u32]) -> (u64, Vec<(u32, u32)>) {
        let n = stops.len();
        if n == 0 {
            return (0, Vec::new());
        }
        let fee = |a: u32, b: u32| self.cost(a, b).unwrap_or(u32::MAX / 4) as u64;
        let from_start: Vec<u64> = stops.iter().map(|s| fee(start, *s)).collect();
        let between: Vec<Vec<u64>> = stops.iter().map(|a| stops.iter().map(|b| fee(*a, *b)).collect()).collect();

        let order = if n <= 12 { exact_route(&from_start, &between) } else { greedy_route(&from_start, &between) };

        let mut total = 0;
        let mut prev: Option<usize> = None;
        let mut legs = Vec::with_capacity(n);
        for i in order {
            let c = match prev {
                None => from_start[i],
                Some(p) => between[p][i],
            };
            total += c;
            legs.push((stops[i], c as u32));
            prev = Some(i);
        }
        (total, legs)
    }
}

/// Held-Karp dynamic program over subsets; fine for up to ~12 stops.
fn exact_route(from_start: &[u64], between: &[Vec<u64>]) -> Vec<usize> {
    let n = from_start.len();
    let full = 1usize << n;
    let mut best = vec![vec![u64::MAX; n]; full];
    let mut parent = vec![vec![usize::MAX; n]; full];
    for i in 0..n {
        best[1 << i][i] = from_start[i];
    }
    for mask in 1..full {
        for last in 0..n {
            let cur = best[mask][last];
            if cur == u64::MAX || mask & (1 << last) == 0 {
                continue;
            }
            for next in 0..n {
                if mask & (1 << next) != 0 {
                    continue;
                }
                let m = mask | (1 << next);
                let c = cur + between[last][next];
                if c < best[m][next] {
                    best[m][next] = c;
                    parent[m][next] = last;
                }
            }
        }
    }
    let mut last = (0..n).min_by_key(|i| best[full - 1][*i]).unwrap();
    let mut mask = full - 1;
    let mut order = vec![last];
    while parent[mask][last] != usize::MAX {
        let p = parent[mask][last];
        mask &= !(1 << last);
        last = p;
        order.push(last);
    }
    order.reverse();
    order
}

/// Nearest-neighbour tour improved with 2-opt.
fn greedy_route(from_start: &[u64], between: &[Vec<u64>]) -> Vec<usize> {
    let n = from_start.len();
    let mut used = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut cur: Option<usize> = None;
    for _ in 0..n {
        let next = (0..n).filter(|i| !used[*i]).min_by_key(|i| cur.map_or(from_start[*i], |c| between[c][*i])).unwrap();
        used[next] = true;
        order.push(next);
        cur = Some(next);
    }
    let cost = |o: &[usize]| -> u64 { from_start[o[0]] + o.windows(2).map(|w| between[w[0]][w[1]]).sum::<u64>() };
    let mut best = cost(&order);
    let mut improved = true;
    while improved {
        improved = false;
        for i in 0..n {
            for j in i + 1..n {
                order[i..=j].reverse();
                let c = cost(&order);
                if c < best {
                    best = c;
                    improved = true;
                } else {
                    order[i..=j].reverse();
                }
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_route_finds_optimum() {
        // Going 0 -> 1 -> 2 is cheap; starting at 2 looks cheapest but is a trap.
        let from_start = vec![10, 50, 5];
        let between = vec![vec![0, 1, 100], vec![1, 0, 1], vec![100, 100, 0]];
        assert_eq!(exact_route(&from_start, &between), vec![0, 1, 2]);
        let mut greedy = greedy_route(&from_start, &between);
        greedy.sort_unstable();
        assert_eq!(greedy, vec![0, 1, 2]);
    }
}

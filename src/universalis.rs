//! Market board listings from Universalis (https://universalis.app).

use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;

use crate::cache::Http;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub price_per_unit: u64,
    pub quantity: u32,
    pub hq: bool,
    pub total: u64,
    pub tax: u64,
    pub world_name: Option<String>,
}

impl Listing {
    /// What the buyer actually pays, including the 5% market tax.
    pub fn cost(&self) -> u64 {
        self.total + self.tax
    }
}

#[derive(Deserialize)]
struct MultiResponse {
    #[serde(default)]
    items: HashMap<String, ItemListings>,
}

#[derive(Deserialize)]
struct ItemListings {
    #[serde(default)]
    listings: Vec<Listing>,
}

/// Current listings for each item on `market` (a world, data center or region name).
pub fn listings(http: &Http, market: &str, ids: &[u32]) -> Result<HashMap<u32, Vec<Listing>>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(100) {
        let id_list: Vec<String> = chunk.iter().map(u32::to_string).collect();
        // Universalis returns a bare item object (not a map) for single-item requests,
        // so always ask for at least two ids.
        let mut joined = id_list.join(",");
        if chunk.len() == 1 {
            joined.push_str(",0");
        }
        let url = format!(
            "https://universalis.app/api/v2/{market}/{joined}?listings=500&entries=0&fields=items.listings.pricePerUnit,items.listings.quantity,items.listings.hq,items.listings.total,items.listings.tax,items.listings.worldName"
        );
        let resp: MultiResponse = http.get_json(&url)?;
        for (id, item) in resp.items {
            if let Ok(id) = id.parse::<u32>() {
                out.insert(id, item.listings);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Purchase {
    pub cost: u64,
    pub units: u32,
    pub picks: Vec<Listing>,
}

/// Cheapest way to buy at least `qty` units from `listings`. Listings must be bought whole,
/// so this may buy more than needed. Returns `None` if there isn't enough supply.
pub fn buy(listings: &[Listing], qty: u32, hq_only: bool) -> Option<Purchase> {
    if qty == 0 {
        return Some(Purchase { cost: 0, units: 0, picks: Vec::new() });
    }
    let mut sorted: Vec<&Listing> = listings.iter().filter(|l| !hq_only || l.hq).collect();
    sorted.sort_by_key(|l| l.price_per_unit);

    let mut picks: Vec<&Listing> = Vec::new();
    let mut units = 0;
    let mut cost = 0;
    let mut rest = sorted.iter();
    for l in rest.by_ref() {
        if units >= qty {
            break;
        }
        picks.push(l);
        units += l.quantity;
        cost += l.cost();
    }
    if units < qty {
        return None;
    }

    // Greedy-by-unit-price can overshoot badly on the last stack (e.g. buying 99 to get 1).
    // Try swapping that last pick for the cheapest single listing that still covers the gap.
    if let Some(last) = picks.last().copied() {
        let need = qty - (units - last.quantity);
        let replacement = sorted
            .iter()
            .filter(|l| !picks.iter().any(|p| std::ptr::eq(*p, **l)) && l.quantity >= need)
            .min_by_key(|l| l.cost());
        if let Some(r) = replacement
            && r.cost() < last.cost()
        {
            picks.pop();
            cost = cost - last.cost() + r.cost();
            units = units - last.quantity + r.quantity;
            picks.push(r);
        }
    }
    Some(Purchase { cost, units, picks: picks.into_iter().cloned().collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(ppu: u64, qty: u32) -> Listing {
        let total = ppu * qty as u64;
        Listing { price_per_unit: ppu, quantity: qty, hq: false, total, tax: total * 5 / 100, world_name: None }
    }

    #[test]
    fn buys_cheapest_units_first() {
        let p = buy(&[l(10, 5), l(5, 3), l(20, 10)], 6, false).unwrap();
        assert_eq!(p.units, 8);
        assert_eq!(p.cost, l(5, 3).cost() + l(10, 5).cost());
    }

    #[test]
    fn avoids_overbuying_a_huge_last_stack() {
        let p = buy(&[l(1, 2), l(2, 99), l(5, 1)], 3, false).unwrap();
        assert_eq!(p.units, 3);
        assert_eq!(p.cost, l(1, 2).cost() + l(5, 1).cost());
    }

    #[test]
    fn not_enough_supply() {
        assert!(buy(&[l(1, 2)], 3, false).is_none());
    }
}

//! FFXIV Teamcraft integration: reading lists, and its datamined gathering/vendor data.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache::Http;
use crate::input::Wanted;

const DATA: &str = "https://raw.githubusercontent.com/ffxiv-teamcraft/ffxiv-teamcraft/staging/libs/data/src/lib/json";

pub struct TeamcraftList {
    pub name: String,
    /// Final items still to make.
    pub wanted: Vec<Wanted>,
    /// Ingredients already obtained and not yet used up, by item id.
    pub stock: HashMap<u32, u32>,
}

/// Loads a Teamcraft list. Accepts a list URL, a bare list id, or an import link
/// (`https://ffxivteamcraft.com/import/...`).
pub fn load_list(http: &Http, input: &str) -> Result<TeamcraftList> {
    let input = input.trim().trim_end_matches('/');
    if let Some(idx) = input.find("/import/") {
        return Ok(TeamcraftList {
            name: "Teamcraft import".to_string(),
            wanted: decode_import(&input[idx + "/import/".len()..])?,
            stock: HashMap::new(),
        });
    }
    let id = match input.find("/list/") {
        Some(idx) => input[idx + "/list/".len()..].split(['/', '?', '#']).next().unwrap_or_default(),
        None => input,
    };
    let url =
        format!("https://firestore.googleapis.com/v1/projects/ffxivteamcraft/databases/(default)/documents/lists/{id}");
    let doc: Value = http.get_json(&url).with_context(|| {
        format!("fetching Teamcraft list {id} (private lists can't be read; make it public or use an import link)")
    })?;
    let fields = &doc["fields"];
    let name = fields["name"]["stringValue"].as_str().unwrap_or(id).to_string();
    let empty = Vec::new();
    let items = fields["finalItems"]["arrayValue"]["values"].as_array().unwrap_or(&empty);
    let mut wanted = Vec::new();
    for item in items {
        let f = &item["mapValue"]["fields"];
        let int = |key: &str| f[key]["integerValue"].as_str().and_then(|s| s.parse::<u32>().ok());
        let (Some(id), Some(amount)) = (int("id"), int("amount")) else { continue };
        let done = int("done").unwrap_or(0);
        if amount > done {
            wanted.push(Wanted::Id(id, amount - done));
        }
    }
    if wanted.is_empty() {
        bail!("Teamcraft list \"{name}\" has no unfinished items");
    }

    // Teamcraft tracks every ingredient's progress too: `done` is how many you've obtained,
    // `used` how many already went into crafts you've checked off. The rest is on hand.
    let mut stock: HashMap<u32, u32> = HashMap::new();
    for item in fields["items"]["arrayValue"]["values"].as_array().unwrap_or(&empty) {
        let f = &item["mapValue"]["fields"];
        let int = |key: &str| f[key]["integerValue"].as_str().and_then(|s| s.parse::<u32>().ok());
        let Some(id) = int("id") else { continue };
        let on_hand = int("done").unwrap_or(0).saturating_sub(int("used").unwrap_or(0));
        if on_hand > 0 {
            *stock.entry(id).or_default() += on_hand;
        }
    }
    Ok(TeamcraftList { name, wanted, stock })
}

/// Import links encode `itemId,recipeId,quantity;...` as base64.
fn decode_import(encoded: &str) -> Result<Vec<Wanted>> {
    let encoded = percent_decode(encoded);
    let engines = [
        base64::engine::general_purpose::STANDARD,
        base64::engine::general_purpose::URL_SAFE,
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    let bytes = engines
        .iter()
        .find_map(|e| e.decode(encoded.as_bytes()).ok())
        .context("Teamcraft import link is not valid base64")?;
    let text = String::from_utf8(bytes).context("Teamcraft import link is not valid text")?;
    let mut wanted = Vec::new();
    for entry in text.split(';').filter(|e| !e.trim().is_empty()) {
        let parts: Vec<&str> = entry.split(',').map(str::trim).collect();
        let id = parts.first().and_then(|s| s.parse().ok());
        let qty = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
        match id {
            Some(id) => wanted.push(Wanted::Id(id, qty)),
            None => bail!("unrecognised Teamcraft import entry \"{entry}\""),
        }
    }
    Ok(wanted)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatherSpot {
    pub territory: u32,
    pub zone: String,
    pub x: f64,
    pub y: f64,
    pub level: u32,
    pub kind: i32,
    /// Only spawns at certain Eorzean hours.
    pub timed: bool,
    /// Needs a folklore tome.
    pub legendary: bool,
    pub spawns: Vec<u32>,
    pub duration: u32,
}

impl GatherSpot {
    pub fn job(&self) -> &'static str {
        match self.kind {
            0 | 1 => "MIN",
            2 | 3 => "BTN",
            _ => "FSH",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorOffer {
    pub price: u64,
    pub npc: String,
    pub territory: u32,
    pub zone: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AetheryteSpot {
    pub territory: u32,
    pub name: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceIndex {
    pub gather: HashMap<u32, Vec<GatherSpot>>,
    pub vendors: HashMap<u32, Vec<VendorOffer>>,
    pub aetherytes: Vec<AetheryteSpot>,
}

#[derive(Deserialize)]
struct Node {
    #[serde(default)]
    items: Vec<u32>,
    #[serde(default)]
    level: u32,
    #[serde(rename = "type", default)]
    kind: i32,
    #[serde(default)]
    limited: bool,
    #[serde(default)]
    legendary: bool,
    #[serde(default)]
    ephemeral: bool,
    #[serde(default)]
    spawns: Vec<u32>,
    #[serde(default)]
    duration: u32,
    #[serde(default)]
    zoneid: u32,
    #[serde(default)]
    map: u32,
    x: Option<f64>,
    y: Option<f64>,
}

#[derive(Deserialize)]
struct MapEntry {
    #[serde(default)]
    territory_id: u32,
    #[serde(default)]
    placename_id: u32,
}

#[derive(Deserialize)]
struct Place {
    #[serde(default)]
    en: String,
}

#[derive(Deserialize)]
struct Shop {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    npcs: Vec<u32>,
    #[serde(default)]
    trades: Vec<Trade>,
}

#[derive(Deserialize)]
struct Trade {
    currencies: Vec<Amount>,
    items: Vec<Amount>,
}

#[derive(Deserialize)]
struct Amount {
    id: u32,
    amount: u64,
}

#[derive(Deserialize)]
struct Npc {
    #[serde(default)]
    en: String,
    position: Option<Position>,
}

#[derive(Deserialize)]
struct Position {
    #[serde(default)]
    map: u32,
    #[serde(default)]
    zoneid: u32,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
}

#[derive(Deserialize)]
struct TcAetheryte {
    #[serde(default)]
    territory: u32,
    #[serde(rename = "type", default)]
    kind: i32,
    #[serde(default)]
    nameid: u32,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
}

pub fn source_index(http: &Http) -> Result<SourceIndex> {
    http.cached("teamcraft-sources-v2", || {
        http.log("Downloading gathering and vendor data from Teamcraft (one-time, ~30 MB)...");
        let get = |name: &str| format!("{DATA}/{name}.json");
        let maps: HashMap<String, MapEntry> = http.get_json(&get("maps"))?;
        let places: HashMap<String, Place> = http.get_json(&get("places"))?;
        let nodes: HashMap<String, Node> = http.get_json(&get("nodes"))?;
        let shops: Vec<Shop> = http.get_json(&get("shops"))?;
        let npcs: HashMap<String, Npc> = http.get_json(&get("npcs"))?;
        let tc_aetherytes: Vec<TcAetheryte> = http.get_json(&get("aetherytes"))?;

        let territory = |map: u32| maps.get(&map.to_string()).map(|m| m.territory_id).filter(|t| *t != 0);
        let place = |id: u32| places.get(&id.to_string()).map(|p| p.en.clone()).unwrap_or_default();
        // `zoneid` is the sub-area (e.g. "Archeo Alexandria"); prefix it with the map's zone.
        let zone = |map: u32, sub: u32| {
            let main = maps.get(&map.to_string()).map(|m| place(m.placename_id)).unwrap_or_default();
            let sub = place(sub);
            match (main.is_empty(), sub.is_empty() || sub == main) {
                (true, _) => sub,
                (false, true) => main,
                (false, false) => format!("{main} ({sub})"),
            }
        };

        let mut gather: HashMap<u32, Vec<GatherSpot>> = HashMap::new();
        for node in nodes.values() {
            let (Some(x), Some(y), Some(terr)) = (node.x, node.y, territory(node.map)) else { continue };
            if node.items.is_empty() {
                continue;
            }
            let spot = GatherSpot {
                territory: terr,
                zone: zone(node.map, node.zoneid),
                x,
                y,
                level: node.level,
                kind: node.kind,
                timed: node.limited || node.ephemeral || !node.spawns.is_empty(),
                legendary: node.legendary,
                spawns: node.spawns.clone(),
                duration: node.duration,
            };
            for item in &node.items {
                gather.entry(*item).or_default().push(spot.clone());
            }
        }

        let mut vendors: HashMap<u32, Vec<VendorOffer>> = HashMap::new();
        for shop in shops.iter().filter(|s| s.kind == "GilShop") {
            let locations: Vec<(&Npc, &Position, u32)> = shop
                .npcs
                .iter()
                .filter_map(|id| npcs.get(&id.to_string()))
                .filter_map(|n| n.position.as_ref().map(|p| (n, p)))
                .filter_map(|(n, p)| territory(p.map).map(|t| (n, p, t)))
                .collect();
            for trade in &shop.trades {
                let [currency] = trade.currencies.as_slice() else { continue };
                let [item] = trade.items.as_slice() else { continue };
                if currency.id != 1 || item.amount == 0 {
                    continue;
                }
                let price = currency.amount.div_ceil(item.amount);
                for (npc, pos, terr) in &locations {
                    vendors.entry(item.id).or_default().push(VendorOffer {
                        price,
                        npc: npc.en.clone(),
                        territory: *terr,
                        zone: zone(pos.map, pos.zoneid),
                        x: pos.x,
                        y: pos.y,
                    });
                }
            }
        }

        let aetherytes = tc_aetherytes
            .iter()
            .filter(|a| a.kind == 0 && a.territory != 0)
            .map(|a| AetheryteSpot { territory: a.territory, name: place(a.nameid), x: a.x, y: a.y })
            .collect();

        Ok(SourceIndex { gather, vendors, aetherytes })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_import_links() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("5057,43,3;5111,null,10");
        let wanted = decode_import(&encoded.replace('=', "%3D")).unwrap();
        assert_eq!(wanted, vec![Wanted::Id(5057, 3), Wanted::Id(5111, 10)]);
    }
}

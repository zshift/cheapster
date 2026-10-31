//! Item, recipe and teleport-sheet lookups via XIVAPI v2 (https://v2.xivapi.com).

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cache::Http;

const BASE: &str = "https://v2.xivapi.com/api";

#[derive(Debug, Clone)]
pub struct ItemInfo {
    pub name: String,
    pub untradable: bool,
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub job: String,
    pub yields: u32,
    pub ingredients: Vec<(u32, u32)>,
}

#[derive(Deserialize)]
struct SearchResponse<T> {
    results: Vec<Row<T>>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct SheetResponse<T> {
    rows: Vec<Row<T>>,
}

#[derive(Deserialize)]
struct Row<T> {
    row_id: u32,
    score: Option<f64>,
    fields: T,
}

fn search<T: DeserializeOwned>(http: &Http, sheet: &str, query: &str, fields: &str) -> Result<Vec<Row<T>>> {
    let mut out = Vec::new();
    let mut url = Url::parse_with_params(
        &format!("{BASE}/search"),
        &[("sheets", sheet), ("query", query), ("fields", fields), ("limit", "100")],
    )?;
    loop {
        let page: SearchResponse<T> = http.get_json(url.as_str())?;
        let full = page.results.len() == 100;
        out.extend(page.results);
        match page.next {
            Some(cursor) if full => {
                url = Url::parse_with_params(&format!("{BASE}/search"), &[("cursor", cursor.as_str())])?
            }
            _ => return Ok(out),
        }
    }
}

/// Reads every row of a sheet, following `after` pagination.
fn sheet_all<T: DeserializeOwned>(http: &Http, sheet: &str, fields: &str) -> Result<Vec<(u32, T)>> {
    let mut out = Vec::new();
    let mut after: Option<u32> = None;
    loop {
        let mut params = vec![("fields", fields.to_string()), ("limit", "500".to_string())];
        if let Some(a) = after {
            params.push(("after", a.to_string()));
        }
        let url = Url::parse_with_params(&format!("{BASE}/sheet/{sheet}"), &params)?;
        let page: SheetResponse<T> = http.get_json(url.as_str())?;
        let Some(last) = page.rows.last().map(|r| r.row_id) else {
            return Ok(out);
        };
        out.extend(page.rows.into_iter().map(|r| (r.row_id, r.fields)));
        after = Some(last);
    }
}

#[derive(Deserialize)]
struct NameFields {
    #[serde(rename = "Name")]
    name: String,
}

/// Resolves an item name to its id. Exact (case-insensitive) matches win; otherwise the
/// best fuzzy match is used and the alternatives are reported.
pub fn find_item(http: &Http, name: &str) -> Result<u32> {
    let escaped = name.replace('"', "");
    let exact: Vec<Row<NameFields>> = search(http, "Item", &format!("Name=\"{escaped}\""), "Name")?;
    if let Some(row) = exact.first() {
        return Ok(row.row_id);
    }
    let mut fuzzy: Vec<Row<NameFields>> = search(http, "Item", &format!("Name~\"{escaped}\""), "Name")?;
    fuzzy.retain(|r| !r.fields.name.is_empty());
    fuzzy.sort_by(|a, b| b.score.unwrap_or(0.0).total_cmp(&a.score.unwrap_or(0.0)));
    let Some(best) = fuzzy.first() else {
        bail!("no item found matching \"{name}\"");
    };
    let others: Vec<&str> = fuzzy.iter().skip(1).take(4).map(|r| r.fields.name.as_str()).collect();
    http.log(&format!(
        "note: \"{name}\" is not an exact item name; using \"{}\"{}",
        best.fields.name,
        if others.is_empty() { String::new() } else { format!(" (other matches: {})", others.join(", ")) }
    ));
    Ok(best.row_id)
}

#[derive(Deserialize)]
struct ItemFields {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "IsUntradable")]
    untradable: bool,
}

pub fn items(http: &Http, ids: &[u32]) -> Result<HashMap<u32, ItemInfo>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(100) {
        let rows: Vec<String> = chunk.iter().map(u32::to_string).collect();
        let url = Url::parse_with_params(
            &format!("{BASE}/sheet/Item"),
            &[("rows", rows.join(",")), ("fields", "Name,IsUntradable".to_string())],
        )?;
        let page: SheetResponse<ItemFields> = http.get_json(url.as_str())?;
        for r in page.rows {
            out.insert(r.row_id, ItemInfo { name: r.fields.name, untradable: r.fields.untradable });
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct RecipeFields {
    #[serde(rename = "ItemResult@as(raw)")]
    result: u32,
    #[serde(rename = "AmountResult")]
    yields: u32,
    #[serde(rename = "Ingredient@as(raw)")]
    ingredients: Vec<i64>,
    #[serde(rename = "AmountIngredient")]
    amounts: Vec<u32>,
    #[serde(rename = "CraftType")]
    craft_type: Option<CraftType>,
}

#[derive(Deserialize)]
struct CraftType {
    fields: Option<NameFields>,
}

/// Fetches every recipe that produces one of `ids`.
pub fn recipes(http: &Http, ids: &[u32]) -> Result<HashMap<u32, Vec<Recipe>>> {
    let mut out: HashMap<u32, Vec<Recipe>> = HashMap::new();
    for chunk in ids.chunks(40) {
        let query: Vec<String> = chunk.iter().map(|id| format!("ItemResult={id}")).collect();
        let rows: Vec<Row<RecipeFields>> = search(
            http,
            "Recipe",
            &query.join(" "),
            "ItemResult@as(raw),AmountResult,Ingredient@as(raw),AmountIngredient,CraftType.Name",
        )
        .context("fetching recipes")?;
        for r in rows {
            let f = r.fields;
            let ingredients = f
                .ingredients
                .iter()
                .zip(&f.amounts)
                .filter(|(id, amount)| **id > 0 && **amount > 0)
                .map(|(id, amount)| (*id as u32, *amount))
                .collect();
            let job = f.craft_type.and_then(|c| c.fields).map(|n| n.name).unwrap_or_default();
            out.entry(f.result).or_default().push(Recipe { job, yields: f.yields.max(1), ingredients });
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerritoryTelepo {
    #[serde(rename = "X")]
    pub x: i32,
    #[serde(rename = "Y")]
    pub y: i32,
    #[serde(rename = "Expansion")]
    pub expansion: i32,
    #[serde(rename = "Relay@as(raw)")]
    pub relay: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelepoRelay {
    /// Width of the aetherstream's horizontal wrap-around; unnamed in the sheet schema.
    #[serde(rename = "Unknown_70", default)]
    pub wrap: i32,
    #[serde(rename = "Relays")]
    pub relays: Vec<RelayEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayEntry {
    #[serde(rename = "Cost")]
    pub cost: i32,
    #[serde(rename = "EnterTerritory@as(raw)")]
    pub enter: u32,
    #[serde(rename = "ExitTerritory@as(raw)")]
    pub exit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aetheryte {
    pub id: u32,
    pub territory: u32,
    pub group: u32,
    pub is_aetheryte: bool,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeleportSheets {
    pub telepo: HashMap<u32, TerritoryTelepo>,
    pub relays: HashMap<u32, TelepoRelay>,
    pub aetherytes: Vec<Aetheryte>,
}

#[derive(Deserialize)]
struct AetheryteFields {
    #[serde(rename = "Territory@as(raw)")]
    territory: u32,
    #[serde(rename = "AethernetGroup")]
    group: u32,
    #[serde(rename = "IsAetheryte")]
    is_aetheryte: bool,
    #[serde(rename = "Invisible")]
    invisible: bool,
    #[serde(rename = "PlaceName")]
    place: Option<CraftType>,
}

pub fn teleport_sheets(http: &Http) -> Result<TeleportSheets> {
    http.cached("teleport-sheets-v1", || {
        http.log("Downloading teleport data from XIVAPI...");
        let telepo = sheet_all::<TerritoryTelepo>(http, "TerritoryTypeTelepo", "X,Y,Expansion,Relay@as(raw)")?
            .into_iter()
            .collect();
        let relays = sheet_all::<TelepoRelay>(
            http,
            "TelepoRelay",
            "Unknown_70,Relays[].Cost,Relays[].EnterTerritory@as(raw),Relays[].ExitTerritory@as(raw)",
        )?
        .into_iter()
        .collect();
        let aetherytes = sheet_all::<AetheryteFields>(
            http,
            "Aetheryte",
            "Territory@as(raw),AethernetGroup,IsAetheryte,Invisible,PlaceName.Name",
        )?
        .into_iter()
        .filter(|(_, a)| !a.invisible && a.territory != 0)
        .map(|(id, a)| Aetheryte {
            id,
            territory: a.territory,
            group: a.group,
            is_aetheryte: a.is_aetheryte,
            name: a.place.and_then(|p| p.fields).map(|n| n.name).unwrap_or_default(),
        })
        .collect();
        Ok(TeleportSheets { telepo, relays, aetherytes })
    })
}

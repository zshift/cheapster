//! User config (`config.toml`): defaults for planning options, plus crafter stats used to
//! check which crafts you can do and to generate crafting macros.
//!
//! Precedence everywhere is: command-line flag > environment variable > config file > default.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};

/// Crafting jobs in the game's (and Raphael's) job id order.
pub const JOBS: [&str; 8] = ["CRP", "BSM", "ARM", "GSM", "LTW", "WVR", "ALC", "CUL"];
const JOB_NAMES: [&str; 8] =
    ["carpenter", "blacksmith", "armorer", "goldsmith", "leatherworker", "weaver", "alchemist", "culinarian"];

/// Gathering jobs, matching `GatherSpot::job`.
pub const GATHERERS: [&str; 3] = ["MIN", "BTN", "FSH"];
const GATHERER_NAMES: [&str; 3] = ["miner", "botanist", "fisher"];

/// What limits which items a gatherer can collect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gatherer {
    pub level: u8,
    pub perception: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Crafter {
    pub level: u8,
    pub craftsmanship: u16,
    pub control: u16,
    pub cp: u16,
    /// Specialists get Heart and Soul and Quick Innovation.
    pub specialist: bool,
    pub manipulation: bool,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub market: Option<String>,
    pub home_world: Option<String>,
    pub start: Option<String>,
    /// Plan crafts even when your stats can't finish them.
    pub craft_infeasible: bool,
    /// Solve HQ macros so they reach full quality even under the worst conditions.
    pub guarantee_hq: bool,
    /// Indexed by job id; `None` for jobs that aren't configured.
    pub crafters: [Option<Crafter>; 8],
    /// Indexed like `GATHERERS`; `None` means "assume they can gather anything".
    pub gatherers: [Option<Gatherer>; 3],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            market: None,
            home_world: None,
            start: None,
            craft_infeasible: false,
            guarantee_hq: true,
            crafters: [None; 8],
            gatherers: [None; 3],
        }
    }
}

impl Config {
    pub fn any_crafters(&self) -> bool {
        self.crafters.iter().any(Option::is_some)
    }
}

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("CHEAPSTER_CONFIG") {
        return p.into();
    }
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cheapster").join("config.toml")
}

/// Loads the config, or the defaults if there is no config file.
pub fn load() -> Result<Config> {
    let path = path();
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse(&text).with_context(|| format!("in config file {}", path.display()))
}

fn parse(text: &str) -> Result<Config> {
    let doc: DocumentMut = text.parse().context("not valid TOML")?;
    let string = |key: &str| doc.get(key).and_then(Item::as_str).map(str::to_string).filter(|s| !s.is_empty());
    let mut config = Config {
        market: string("market"),
        home_world: string("home_world"),
        start: string("start"),
        craft_infeasible: doc.get("craft_infeasible").and_then(Item::as_bool).unwrap_or(false),
        guarantee_hq: doc.get("guarantee_hq").and_then(Item::as_bool).unwrap_or(true),
        crafters: [None; 8],
        gatherers: [None; 3],
    };
    if let Some(crafters) = doc.get("crafters").and_then(Item::as_table_like) {
        for (key, item) in crafters.iter() {
            let job = job_id(key).with_context(|| {
                format!("unknown crafter \"{key}\" (use one of {}, or full job names)", JOBS.join(", "))
            })?;
            let table = item.as_table_like().with_context(|| format!("[crafters.{key}] must be a table"))?;
            let int = |field: &str| -> Result<Option<i64>> {
                match table.get(field) {
                    None => Ok(None),
                    Some(v) => {
                        v.as_integer().map(Some).with_context(|| format!("crafters.{key}.{field} must be a number"))
                    }
                }
            };
            let need = |field: &str| -> Result<i64> {
                int(field)?.with_context(|| format!("crafters.{key} is missing \"{field}\""))
            };
            let level = need("level")?.clamp(1, 100) as u8;
            config.crafters[job as usize] = Some(Crafter {
                level,
                craftsmanship: need("craftsmanship")?.clamp(0, u16::MAX as i64) as u16,
                control: need("control")?.clamp(0, u16::MAX as i64) as u16,
                cp: need("cp")?.clamp(0, u16::MAX as i64) as u16,
                specialist: table.get("specialist").and_then(Item::as_bool).unwrap_or(false),
                // Manipulation is learned at 65 via a quest nearly everyone has done.
                manipulation: table.get("manipulation").and_then(Item::as_bool).unwrap_or(level >= 65),
            });
        }
    }
    if let Some(gatherers) = doc.get("gatherers").and_then(Item::as_table_like) {
        for (key, item) in gatherers.iter() {
            let job = gatherer_id(key).with_context(|| {
                format!("unknown gatherer \"{key}\" (use one of {}, or full job names)", GATHERERS.join(", "))
            })?;
            let table = item.as_table_like().with_context(|| format!("[gatherers.{key}] must be a table"))?;
            let need = |field: &str| -> Result<i64> {
                table
                    .get(field)
                    .with_context(|| format!("gatherers.{key} is missing \"{field}\""))?
                    .as_integer()
                    .with_context(|| format!("gatherers.{key}.{field} must be a number"))
            };
            config.gatherers[job] = Some(Gatherer {
                level: need("level")?.clamp(1, 100) as u8,
                perception: need("perception")?.clamp(0, u16::MAX as i64) as u16,
            });
        }
    }
    Ok(config)
}

/// Accepts "MIN", "min" or "miner".
pub fn gatherer_id(name: &str) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    GATHERERS
        .iter()
        .position(|j| j.eq_ignore_ascii_case(&lower))
        .or_else(|| GATHERER_NAMES.iter().position(|j| *j == lower))
}

/// Accepts "CRP", "crp" or "carpenter".
pub fn job_id(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    JOBS.iter()
        .position(|j| j.eq_ignore_ascii_case(&lower))
        .or_else(|| JOB_NAMES.iter().position(|j| *j == lower))
        .map(|i| i as u8)
}

/// Saves the market into the config file, keeping everything else (including comments) as is.
pub fn save_market(market: &str) -> Result<()> {
    edit(|doc| doc["market"] = value(market))
}

/// Saves crafter stats and crafting preferences, keeping everything else as is.
pub fn save_crafting(config: &Config) -> Result<()> {
    edit(|doc| set_crafting(doc, config))
}

/// Applies a change to the config file, starting from the commented template if there is none.
fn edit(change: impl FnOnce(&mut DocumentMut)) -> Result<()> {
    let path = path();
    let text = fs::read_to_string(&path).unwrap_or_else(|_| TEMPLATE.to_string());
    let mut doc: DocumentMut = text.parse().with_context(|| format!("{} is not valid TOML", path.display()))?;
    change(&mut doc);
    write(&path, &doc.to_string())
}

fn set_crafting(doc: &mut DocumentMut, config: &Config) {
    doc["craft_infeasible"] = value(config.craft_infeasible);
    doc["guarantee_hq"] = value(config.guarantee_hq);
    let gatherers: Vec<Option<Vec<(&str, Item)>>> = config
        .gatherers
        .iter()
        .map(|g| g.map(|g| vec![("level", value(g.level as i64)), ("perception", value(g.perception as i64))]))
        .collect();
    set_job_tables(doc, "gatherers", &GATHERERS, &gatherers, gatherer_id);
    let crafters: Vec<Option<Vec<(&str, Item)>>> = config
        .crafters
        .iter()
        .map(|c| {
            c.map(|c| {
                vec![
                    ("level", value(c.level as i64)),
                    ("craftsmanship", value(c.craftsmanship as i64)),
                    ("control", value(c.control as i64)),
                    ("cp", value(c.cp as i64)),
                    ("specialist", value(c.specialist)),
                    ("manipulation", value(c.manipulation)),
                ]
            })
        })
        .collect();
    set_job_tables(doc, "crafters", &JOBS, &crafters, |k| job_id(k).map(|j| j as usize));
}

/// Writes one `[section.JOB]` table per configured job and removes unconfigured ones. Existing
/// sections are updated in place (even if named e.g. `carpenter`), keeping their comments.
fn set_job_tables(
    doc: &mut DocumentMut,
    section: &str,
    jobs: &[&str],
    values: &[Option<Vec<(&str, Item)>>],
    id: impl Fn(&str) -> Option<usize>,
) {
    if values.iter().all(Option::is_none) && !doc.contains_key(section) {
        return;
    }
    if !doc.contains_key(section) {
        let mut table = Table::new();
        // Render as [section.JOB] headers rather than an empty [section] header.
        table.set_implicit(true);
        doc[section] = Item::Table(table);
    }
    let Some(tables) = doc[section].as_table_like_mut() else { return };
    for (index, (job, fields)) in jobs.iter().zip(values).enumerate() {
        let existing: Option<String> = tables.iter().map(|(k, _)| k.to_string()).find(|k| id(k) == Some(index));
        match fields {
            None => {
                if let Some(key) = existing {
                    tables.remove(&key);
                }
            }
            Some(fields) => {
                let key = existing.unwrap_or_else(|| job.to_string());
                if tables.get(&key).and_then(Item::as_table_like).is_none() {
                    tables.insert(&key, Item::Table(Table::new()));
                }
                let table = tables.get_mut(&key).and_then(Item::as_table_like_mut).unwrap();
                for (name, v) in fields {
                    table.insert(name, v.clone());
                }
            }
        }
    }
}

/// Creates a commented starter config if none exists. Returns its path.
pub fn create_template() -> Result<PathBuf> {
    let path = path();
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    write(&path, TEMPLATE)?;
    Ok(path)
}

fn write(path: &PathBuf, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

const TEMPLATE: &str = r#"# cheapster config. Command-line flags and environment variables override these.

# World, data center or region to buy from on the market board.
# market = "Crystal"
# home_world = "Coeurl"
# start = "Limsa Lominsa Lower Decks"

# Crafts your stats can't finish are bought instead. Set to true to plan them anyway.
craft_infeasible = false

# HQ macros reach full quality even if every condition rolls Poor (slower, but HQ is certain).
guarantee_hq = true

# Your crafters' base stats (gear and materia, without food or potions).
# Jobs: CRP BSM ARM GSM LTW WVR ALC CUL (or full names like "carpenter").
# Crafts for jobs not listed here are assumed possible, but get no macro.
#
# [crafters.CRP]
# level = 100
# craftsmanship = 5400
# control = 4900
# cp = 600
# specialist = false     # Heart and Soul / Quick Innovation
# manipulation = true    # defaults to true from level 65

# Your gatherers. Some items need a minimum perception; jobs not listed here are assumed
# able to gather anything. Jobs: MIN BTN FSH (or "miner", "botanist", "fisher").
#
# [gatherers.BTN]
# level = 100
# perception = 5100
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_crafters_and_defaults() {
        let config = parse(
            r#"
            market = "Crystal"
            [crafters.CRP]
            level = 100
            craftsmanship = 5400
            control = 4900
            cp = 600
            specialist = true
            [crafters.culinarian]
            level = 50
            craftsmanship = 500
            control = 400
            cp = 300
            "#,
        )
        .unwrap();
        assert_eq!(config.market.as_deref(), Some("Crystal"));
        assert!(config.guarantee_hq && !config.craft_infeasible);
        let crp = config.crafters[0].unwrap();
        assert!(crp.specialist && crp.manipulation);
        let cul = config.crafters[7].unwrap();
        assert_eq!((cul.level, cul.manipulation), (50, false));
        assert!(config.crafters[1].is_none());
    }

    #[test]
    fn rejects_unknown_jobs_and_missing_stats() {
        assert!(parse("[crafters.MIN]\nlevel = 100").is_err());
        assert!(parse("[crafters.CRP]\nlevel = 100").is_err());
    }

    #[test]
    fn saving_crafting_keeps_comments_and_round_trips() {
        let original = r#"# my notes
market = "Crystal" # where I shop

[crafters.carpenter]
level = 90
craftsmanship = 4000
control = 3900
cp = 600

[crafters.CUL]
level = 100
craftsmanship = 5000
control = 5000
cp = 650
"#;
        let mut config = parse(original).unwrap();
        let crp =
            Crafter { level: 100, craftsmanship: 5400, control: 5100, cp: 700, specialist: true, manipulation: true };
        config.crafters[0] = Some(crp);
        config.crafters[7] = None;
        config.crafters[1] = Some(Crafter { specialist: false, ..crp });
        config.guarantee_hq = false;

        let mut doc: DocumentMut = original.parse().unwrap();
        set_crafting(&mut doc, &config);
        let text = doc.to_string();
        assert!(text.contains("# my notes") && text.contains("# where I shop"), "{text}");
        // The existing [crafters.carpenter] section is updated rather than duplicated.
        assert!(text.contains("[crafters.carpenter]") && !text.contains("[crafters.CRP]"), "{text}");
        assert!(!text.contains("CUL"), "{text}");

        let reloaded = parse(&text).unwrap();
        assert_eq!(reloaded.crafters, config.crafters);
        assert!(!reloaded.guarantee_hq);
        assert_eq!(reloaded.market.as_deref(), Some("Crystal"));
    }

    #[test]
    fn saving_into_the_template_uses_job_sections() {
        let mut config = parse(TEMPLATE).unwrap();
        config.crafters[3] =
            Some(Crafter { level: 100, craftsmanship: 1, control: 2, cp: 3, specialist: false, manipulation: true });
        let mut doc: DocumentMut = TEMPLATE.parse().unwrap();
        set_crafting(&mut doc, &config);
        let text = doc.to_string();
        assert!(text.contains("[crafters.GSM]") && !text.contains("\n[crafters]\n"), "{text}");
        assert_eq!(parse(&text).unwrap().crafters, config.crafters);
    }

    #[test]
    fn gatherers_parse_and_save() {
        let config = parse("[gatherers.botanist]\nlevel = 100\nperception = 5000\n").unwrap();
        assert_eq!(config.gatherers[1], Some(Gatherer { level: 100, perception: 5000 }));
        assert!(parse("[gatherers.CRP]\nlevel = 100\nperception = 1").is_err());

        let mut changed = config.clone();
        changed.gatherers[1] = None;
        changed.gatherers[0] = Some(Gatherer { level: 90, perception: 4000 });
        let mut doc: DocumentMut = "[gatherers.botanist]\nlevel = 100\nperception = 5000\n".parse().unwrap();
        set_crafting(&mut doc, &changed);
        let text = doc.to_string();
        assert!(text.contains("[gatherers.MIN]") && !text.contains("botanist"), "{text}");
        assert_eq!(parse(&text).unwrap().gatherers, changed.gatherers);
    }

    #[test]
    fn template_is_valid() {
        let config = parse(TEMPLATE).unwrap();
        assert!(!config.any_crafters());
    }
}

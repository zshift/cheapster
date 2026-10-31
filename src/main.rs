mod cache;
mod config;
mod crafting;
mod input;
mod pipeline;
mod planner;
mod report;
mod teamcraft;
mod travel;
mod tui;
mod universalis;
mod view;
mod xivapi;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::cache::Http;
use crate::config::Config;
use crate::pipeline::{Inputs, Options};
use crate::travel::Travel;

/// Finds the cheapest way to get everything needed to craft FFXIV items: buying from the
/// market board or vendors, gathering it yourself, or crafting intermediates, including the
/// teleport fees for flying around.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Items to make, e.g. "Iron Ingot", "3x Iron Ingot", "Iron Ingot:3" or an item id.
    items: Vec<String>,

    /// Read items from a file, one per line (same formats as above; # starts a comment).
    #[arg(short, long)]
    file: Option<PathBuf>,

    /// A Teamcraft list URL or id, or a Teamcraft import link.
    #[arg(short, long)]
    teamcraft: Option<String>,

    /// Print the teleport fee from the start to every aetheryte, then exit.
    #[arg(long)]
    fees: bool,

    #[command(flatten)]
    opts: PlanArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive view: paste a Teamcraft link to plan it, click market items to copy their names.
    Tui {
        /// Teamcraft list URL or id to load at startup.
        list: Option<String>,

        /// Import Teamcraft links as soon as you copy them, without pasting.
        #[arg(long)]
        watch_clipboard: bool,

        #[command(flatten)]
        opts: PlanArgs,
    },

    /// Show or create the config file (market, home world, crafter stats).
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Create a commented starter config file.
    Init,
    /// Print the config file's path.
    Path,
}

const DEFAULT_START: &str = "Limsa Lominsa Lower Decks";

#[derive(Args, Clone)]
struct PlanArgs {
    /// World, data center or region to buy from on the market board.
    #[arg(short, long, env = "CHEAPSTER_MARKET")]
    market: Option<String>,

    /// Your home world; its market purchases are listed last.
    #[arg(long, env = "CHEAPSTER_HOME_WORLD")]
    home_world: Option<String>,

    /// Aetheryte you start at [default: Limsa Lominsa Lower Decks].
    #[arg(long, env = "CHEAPSTER_START")]
    start: Option<String>,

    /// Plan crafts even when your configured crafter stats can't finish them.
    #[arg(long)]
    craft_infeasible: bool,

    /// Require HQ when buying the finished items themselves.
    #[arg(long)]
    hq: bool,

    /// Never gather; only buy or craft.
    #[arg(long)]
    no_gather: bool,

    /// Skip timed, unspoiled and legendary gathering nodes.
    #[arg(long)]
    no_timed: bool,

    /// Gil to charge yourself per gathered item, to value your time (default: free).
    #[arg(long, default_value_t = 0)]
    gather_cost: u64,

    /// Plan for shards, crystals and clusters too (by default you're assumed to have enough).
    #[arg(long)]
    include_crystals: bool,

    /// Re-download cached game data (gathering nodes, vendors, teleport tables).
    #[arg(long)]
    refresh: bool,
}

impl PlanArgs {
    fn options(&self, config: &Config) -> Result<Options> {
        let opts = self.options_allowing_no_market(config);
        if opts.market.is_empty() {
            anyhow::bail!(
                "no market: pass --market <world|dc|region>, set CHEAPSTER_MARKET, or set market in {}",
                config::path().display()
            );
        }
        Ok(opts)
    }

    /// Flags and environment variables win over the config file. The TUI can ask for the
    /// market itself, so it may be left empty here.
    fn options_allowing_no_market(&self, config: &Config) -> Options {
        let mut config = config.clone();
        config.craft_infeasible |= self.craft_infeasible;
        Options {
            market: self.market.clone().or_else(|| config.market.clone()).unwrap_or_default(),
            home_world: self.home_world.clone().or_else(|| config.home_world.clone()),
            start: self.start.clone().or_else(|| config.start.clone()).unwrap_or_else(|| DEFAULT_START.into()),
            hq: self.hq,
            no_gather: self.no_gather,
            no_timed: self.no_timed,
            gather_cost: self.gather_cost,
            include_crystals: self.include_crystals,
            config,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::load()?;

    match cli.command {
        Some(Command::Tui { list, watch_clipboard, opts }) => {
            return tui::run(opts.options_allowing_no_market(&config), opts.refresh, list, watch_clipboard);
        }
        Some(Command::Config { action }) => return config_command(action, &config),
        None => {}
    }

    let http = Http::new(cli.opts.refresh, |msg| eprintln!("{msg}"))?;
    if cli.fees {
        let travel = Travel::new(xivapi::teleport_sheets(&http)?);
        let start = cli.opts.options_allowing_no_market(&config).start;
        let (start, start_name) = pipeline::find_start(&travel, &start)?;
        let mut fees: Vec<(u32, &str)> =
            travel.aetherytes().filter_map(|(t, name)| Some((travel.cost(start, t)?, name))).collect();
        fees.sort();
        println!("Teleport fees from {start_name} (no discounts):");
        for (fee, name) in fees {
            println!("  {:>6}  {name}", view::gil(fee as u64));
        }
        return Ok(());
    }

    let inputs = Inputs { items: cli.items, file: cli.file, teamcraft: cli.teamcraft };
    let view = pipeline::run(&http, &cli.opts.options(&config)?, &inputs)?;
    report::print(&view);
    Ok(())
}

fn config_command(action: Option<ConfigAction>, config: &Config) -> Result<()> {
    match action {
        Some(ConfigAction::Init) => println!("Created {}", config::create_template()?.display()),
        Some(ConfigAction::Path) => println!("{}", config::path().display()),
        None => {
            let path = config::path();
            if !path.exists() {
                println!("No config file yet. Run `cheapster config init` to create {}", path.display());
                return Ok(());
            }
            println!("{}", path.display());
            println!("  market: {}", config.market.as_deref().unwrap_or("(not set)"));
            println!("  home_world: {}", config.home_world.as_deref().unwrap_or("(not set)"));
            println!("  start: {}", config.start.as_deref().unwrap_or(DEFAULT_START));
            println!("  craft_infeasible: {} · guarantee_hq: {}", config.craft_infeasible, config.guarantee_hq);
            for (job, crafter) in config::JOBS.iter().zip(&config.crafters) {
                match crafter {
                    Some(c) => println!(
                        "  {job}: lv{} · {} craftsmanship · {} control · {} CP{}",
                        c.level,
                        c.craftsmanship,
                        c.control,
                        c.cp,
                        if c.specialist { " · specialist" } else { "" }
                    ),
                    None => println!("  {job}: (not set)"),
                }
            }
            for (job, gatherer) in config::GATHERERS.iter().zip(&config.gatherers) {
                match gatherer {
                    Some(g) => println!("  {job}: lv{} · {} perception", g.level, g.perception),
                    None => println!("  {job}: (not set, assumed able to gather anything)"),
                }
            }
        }
    }
    Ok(())
}

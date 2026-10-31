mod cache;
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

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::cache::Http;
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
}

#[derive(Args, Clone)]
struct PlanArgs {
    /// World, data center or region to buy from on the market board.
    #[arg(short, long, env = "CHEAPSTER_MARKET")]
    market: Option<String>,

    /// Your home world; its market purchases are listed last.
    #[arg(long, env = "CHEAPSTER_HOME_WORLD", default_value = "Coeurl")]
    home_world: Option<String>,

    /// Aetheryte you start at.
    #[arg(long, env = "CHEAPSTER_START", default_value = "Limsa Lominsa Lower Decks")]
    start: String,

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
    fn options(&self) -> Result<Options> {
        self.market.as_ref().context("pass --market <world|dc|region> or set CHEAPSTER_MARKET")?;
        Ok(self.options_allowing_no_market())
    }

    /// The TUI can ask for the market itself, so it may be left empty here.
    fn options_allowing_no_market(&self) -> Options {
        Options {
            market: self.market.clone().unwrap_or_default(),
            home_world: self.home_world.clone(),
            start: self.start.clone(),
            hq: self.hq,
            no_gather: self.no_gather,
            no_timed: self.no_timed,
            gather_cost: self.gather_cost,
            include_crystals: self.include_crystals,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Tui { list, watch_clipboard, opts }) = cli.command {
        return tui::run(opts.options_allowing_no_market(), opts.refresh, list, watch_clipboard);
    }

    let http = Http::new(cli.opts.refresh, |msg| eprintln!("{msg}"))?;
    if cli.fees {
        let travel = Travel::new(xivapi::teleport_sheets(&http)?);
        let (start, start_name) = pipeline::find_start(&travel, &cli.opts.start)?;
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
    let view = pipeline::run(&http, &cli.opts.options()?, &inputs)?;
    report::print(&view);
    Ok(())
}

//! Prints a plan as a shopping list, travel route and crafting order.

use crate::view::{View, gil};

pub fn print(view: &View) {
    println!("{}", view.title);
    println!("{}\n", view.context);

    println!("{}", view.total);
    println!("  {}", view.breakdown);

    println!("\nBuying the finished items on the market instead:");
    for line in &view.finished {
        println!("  • {line}");
    }
    if let Some(total) = &view.finished_total {
        println!("  {total}");
    }
    println!();

    if !view.on_hand.is_empty() {
        println!("Already on hand (marked obtained in Teamcraft), not bought:");
        for line in &view.on_hand {
            println!("  • {line}");
        }
        println!();
    }

    println!("Route");
    if view.route.is_empty() {
        println!("  No travel needed.");
    }
    for stop in &view.route {
        println!("  {}", stop.heading);
        for e in &stop.errands {
            println!("    • {e}");
        }
    }
    println!();

    if !view.market.is_empty() {
        println!("Market board");
        for group in &view.market {
            println!("  {}: {} gil", group.heading(), gil(group.subtotal()));
            for line in &group.lines {
                println!("    • {}", line.describe());
            }
        }
        if !view.overbuys.is_empty() {
            println!("  Stacks can't be split, so you'll have extra: {}", view.overbuys.join(", "));
        }
        println!();
    }

    if !view.crafts.is_empty() {
        println!("Craft (in this order)");
        for step in &view.crafts {
            println!("  {}", step.line);
            for detail in &step.details {
                println!("      {detail}");
            }
            for block in &step.macros {
                println!("      {}:", block.label);
                for line in block.text.lines() {
                    println!("        {line}");
                }
            }
        }
        println!();
    }

    if !view.shortfalls.is_empty() {
        println!("Not enough on the market (and no vendor or gathering node), e.g. monster drops:");
        for s in &view.shortfalls {
            println!("  {}", s.summary);
            if let Some(listings) = &s.listings {
                println!("      {listings}");
            }
        }
    }

    if !view.blocked.is_empty() {
        println!("Why these are bought instead of crafted or gathered:");
        for line in &view.blocked {
            println!("  • {line}");
        }
        println!();
    }

    for note in &view.notes {
        println!("{note}");
    }
}

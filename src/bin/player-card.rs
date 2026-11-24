//! player-card: register a player by name and generate their QR card.
//!
//! The card payload is `<uuid> <name>`
//!
//! The new `uuid,name` row is appended to the players CSV
//! the ble-logger watches the file, so a running logger picks new players up
//! immediately.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;
use qrcode::QrCode;
use uuid::Uuid;

/// players.csv header when the tool creates the file.
const PLAYERS_HEADER: &str = "uuid,name";

#[derive(Parser)]
#[command(about = "Register a player and generate their QR card")]
struct Args {
    /// Player name (spaces allowed).
    name: String,
    /// Players CSV to append to (created with a header if missing).
    #[arg(long, default_value = "players.csv")]
    players: PathBuf,
    /// PNG output path. Default: slugified name, e.g. trevor-nichols.png.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Reprint a card for an existing UUID instead of generating one;
    /// the players CSV is left untouched.
    #[arg(long)]
    uuid: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let name = args.name.trim();
    if name.is_empty() || name.contains([',', '\n']) {
        bail!("player name must be non-empty and contain no commas/newlines");
    }

    let uuid = match &args.uuid {
        Some(u) => Uuid::parse_str(u)
            .context("--uuid must be a hyphenated UUID")?
            .to_string(),
        None => Uuid::new_v4().to_string(),
    };
    let payload = format!("{uuid} {name}");

    // QR: PNG for printing, unicode art for an instant on-screen scan.
    let code = QrCode::new(payload.as_bytes()).context("QR encode")?;
    let output = args
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{}.png", slugify(name))));
    code.render::<image::Luma<u8>>()
        .min_dimensions(400, 400)
        .build()
        .save(&output)
        .with_context(|| format!("write {}", output.display()))?;
    println!(
        "{}",
        code.render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build()
    );
    println!("payload: {payload}");
    println!("card: {}", output.display());

    if args.uuid.is_none() {
        register(&args.players, &uuid, name)?;
    }
    Ok(())
}

/// Append the player to the mapping CSV (created if missing). A duplicate
/// name gets a warning, not an error — two people can share a name.
fn register(path: &Path, uuid: &str, name: &str) -> Result<()> {
    if let Ok(file) = File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let mut fields = line.splitn(3, ',');
            let (row_uuid, row_name) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""));
            if row_uuid == uuid {
                bail!("uuid {uuid} is already registered to {row_name:?}");
            }
            if row_name == name {
                eprintln!(
                    "note: {name:?} is already registered (uuid {row_uuid}); adding another card"
                );
            }
        }
    }
    let empty = !path.exists() || path.metadata()?.len() == 0;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if empty {
        writeln!(file, "{PLAYERS_HEADER}")?;
    }
    writeln!(file, "{uuid},{name}")?;
    file.flush()?;
    println!("registered: {uuid},{name} -> {}", path.display());
    Ok(())
}

/// Lowercase filename stem: "Trevor Nichols" -> "trevor-nichols".
fn slugify(name: &str) -> String {
    let slug = name
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    if slug.is_empty() {
        "player".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn slugify_names() {
        assert_eq!(super::slugify("Trevor Nichols"), "trevor-nichols");
        assert_eq!(super::slugify("  Ann-Marie O'Brien "), "annmarie-obrien");
        assert_eq!(super::slugify("!!!"), "player");
    }
}

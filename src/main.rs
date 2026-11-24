//! BLE score-sync logger for the ESP-NOW timer's start node
//!
//! This logger writes each received line to the scores CSV, appending a
//! local wall-clock receipt timestamp

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::Local;
use clap::Parser;
use futures::StreamExt;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Advertised device name of the timer's start node.
const DEFAULT_DEVICE_NAME: &str = "ESPNOW Timer";
/// GATT service carrying the score-sync characteristics.
const SERVICE_UUID: &str = "35540000-2495-4e4f-5721-54494d455221";
/// READ+NOTIFY characteristic: pending scores as CSV lines.
const RESULTS_UUID: &str = "35540001-2495-4e4f-5721-54494d455221";
/// WRITE characteristic: `ACK <seq>` / `CLEAR`.
const COMMAND_UUID: &str = "35540002-2495-4e4f-5721-54494d455221";

/// Header of the scores CSV (one finished run per line).
const SCORES_HEADER: &str = "seq,player_uuid,elapsed_us,at_ms,received_at";

/// How long to scan for the timer before giving up (and retrying).
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);
/// Wait between reconnect attempts.
const RETRY_WAIT: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(about = "Log ESP-NOW timer scores over BLE to CSV files")]
struct Args {
    /// Scores output file (appended to).
    #[arg(long, default_value = "scores.csv")]
    scores: PathBuf,
    /// Player UUID -> name mapping CSV: maintained by hand, watched for
    /// changes (edits take effect without a restart).
    #[arg(long, default_value = "players.csv")]
    players: PathBuf,
    /// Device name to look for while scanning.
    #[arg(long, default_value = DEFAULT_DEVICE_NAME)]
    device_name: String,
    /// Bluetooth adapter to use (substring of its name/info, e.g.
    /// "hci0"). Default: the first adapter.
    #[arg(long)]
    adapter: Option<String>,
    /// Fallback full re-read period (seconds); notifications also
    /// trigger reads, so this is only a safety net.
    #[arg(long, default_value_t = 5)]
    poll_interval_secs: u64,
    /// Do not ACK written scores back to the device; once its pending
    /// buffer outgrows the BLE MTU, the newest scores are truncated.
    #[arg(long)]
    no_ack: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut sink = Sink::open(&args.scores, &args.players, !args.no_ack)?;
    let mut players_watch = watch_file(&args.players);
    println!("scores -> {}", args.scores.display());
    println!("players <- {} (read-only, watched)", args.players.display());

    loop {
        match session(&args, &mut sink, &mut players_watch).await {
            Ok(()) => break, // clean Ctrl+C
            Err(e) => {
                eprintln!("{e:#}");
                println!("retrying in {} s...", RETRY_WAIT.as_secs());
                tokio::time::sleep(RETRY_WAIT).await;
            }
        }
    }
    Ok(())
}

/// One connection's lifetime: find the timer, subscribe, then store
/// scores until the link drops or the user quits.
async fn session(
    args: &Args,
    sink: &mut Sink,
    players_watch: &mut Option<mpsc::UnboundedReceiver<()>>,
) -> Result<()> {
    let adapter = pick_adapter(args.adapter.as_deref()).await?;
    let device = find_device(&adapter, &args.device_name).await?;
    device.connect().await.context("connect")?;
    device
        .discover_services()
        .await
        .context("discover services")?;
    println!("connected to {:?}", args.device_name);

    let chars = device.characteristics();
    let lookup = |uuid: &str| -> Result<Characteristic> {
        let uuid = Uuid::parse_str(uuid).expect("valid UUID constant");
        chars
            .iter()
            .find(|c| c.uuid == uuid)
            .cloned()
            .with_context(|| format!("characteristic {uuid} not found"))
    };
    let results = lookup(RESULTS_UUID)?;
    // Only needed to ack; don't fail a --no-ack run without it.
    let command = (!args.no_ack).then(|| lookup(COMMAND_UUID)).transpose()?;

    device.subscribe(&results).await.context("subscribe")?;
    let mut notifications = device.notifications().await.context("notifications")?;

    // Catch up with everything pending, then stay live.
    sync(&device, &results, command.as_ref(), sink).await?;

    // SIGTERM, for clean shutdown as a background/daemon process (Ctrl+C
    // covers SIGINT). Pending forever on non-unix.
    let sigterm = async {
        #[cfg(unix)]
        {
            let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
            s.recv().await;
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    tokio::pin!(sigterm);

    let mut poll = tokio::time::interval(Duration::from_secs(args.poll_interval_secs.max(1)));
    loop {
        tokio::select! {
            note = notifications.next() => {
                if note.is_none() {
                    bail!("BLE notification stream ended (disconnected)");
                }
                sync(&device, &results, command.as_ref(), sink).await?;
            }
            _ = poll.tick() => {
                if !device.is_connected().await.unwrap_or(false) {
                    bail!("BLE disconnected");
                }
                sync(&device, &results, command.as_ref(), sink).await?;
            }
            // The players CSV changed on disk (or the watcher died —
            // lookup-miss reloads remain as a fallback).
            msg = async {
                match players_watch.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match msg {
                    Some(()) => sink.reload_players(),
                    None => *players_watch = None,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("ctrl-c: disconnecting");
                let _ = device.disconnect().await;
                return Ok(());
            }
            _ = &mut sigterm => {
                println!("SIGTERM: disconnecting");
                let _ = device.disconnect().await;
                return Ok(());
            }
        }
    }
}

/// Read the pending scores, store them, and ack what is safely on disk.
async fn sync(
    device: &Peripheral,
    results: &Characteristic,
    command: Option<&Characteristic>,
    sink: &mut Sink,
) -> Result<()> {
    let raw = device.read(results).await.context("read results")?;
    let text = String::from_utf8_lossy(&raw);
    if let (Some(seq), Some(command)) = (sink.store(text.trim())?, command) {
        let ack = format!("ACK {seq}");
        device
            .write(command, ack.as_bytes(), WriteType::WithResponse)
            .await
            .context("ack scores")?;
        sink.last_acked = seq;
        println!("acked scores through seq {seq}");
    }
    Ok(())
}

/// The Bluetooth adapter to talk through.
async fn pick_adapter(want: Option<&str>) -> Result<Adapter> {
    let manager = Manager::new().await.context("bluetooth manager")?;
    let adapters = manager.adapters().await.context("list adapters")?;
    match want {
        Some(name) => {
            for adapter in adapters {
                let info = adapter.adapter_info().await.unwrap_or_default();
                if info.contains(name) {
                    return Ok(adapter);
                }
            }
            bail!("no adapter matching {name:?}")
        }
        None => adapters
            .into_iter()
            .next()
            .context("no bluetooth adapter found"),
    }
}

/// Scan until the timer shows up (by advertised service or name).
async fn find_device(adapter: &Adapter, name: &str) -> Result<Peripheral> {
    let service = Uuid::parse_str(SERVICE_UUID).expect("valid UUID constant");
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start scan")?;
    let deadline = tokio::time::Instant::now() + SCAN_TIMEOUT;
    let mut found = None;
    while found.is_none() && tokio::time::Instant::now() < deadline {
        for p in adapter.peripherals().await.context("list peripherals")? {
            if let Some(props) = p.properties().await.context("peripheral properties")? {
                if props.local_name.as_deref() == Some(name) || props.services.contains(&service) {
                    found = Some(p);
                    break;
                }
            }
        }
        if found.is_none() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let _ = adapter.stop_scan().await;
    found.with_context(|| format!("device {name:?} not found within {SCAN_TIMEOUT:?}"))
}

/// Watch a file for changes: fires on each save (parent dir watched, so
/// write-temp-then-rename editors are seen). `None` when watching fails.
fn watch_file(path: &Path) -> Option<mpsc::UnboundedReceiver<()>> {
    let name = path.file_name()?.to_os_string();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !matches!(
                event.kind,
                notify::EventKind::Modify(_) | notify::EventKind::Create(_)
            ) {
                return;
            }
            if event
                .paths
                .iter()
                .any(|p| p.file_name() == Some(name.as_os_str()))
            {
                let _ = tx.send(());
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("cannot watch {}: {e:#}", path.display());
                return None;
            }
        };
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
        eprintln!("cannot watch {}: {e:#}", dir.display());
        return None;
    }
    // The watcher dies when dropped; leak it — one instance per
    // process, so nothing accumulates.
    Box::leak(Box::new(watcher));
    Some(rx)
}

/// Where received data goes: the scores CSV, the session's dedupe set,
/// the player map (read-only), and the ack cursor.
struct Sink {
    file: File,
    /// Device CSV lines (`seq,player_uuid,elapsed_us,at_ms`) already on
    /// disk, so reconnects and re-reads don't duplicate rows.
    seen: HashSet<String>,
    players_path: PathBuf,
    /// UUID -> player name, from the players CSV. Reloaded when the file
    /// changes on disk (and on lookup misses, as a fallback).
    players: HashMap<String, String>,
    last_acked: u32,
    ack: bool,
}

impl Sink {
    fn open(scores_path: &Path, players_path: &Path, ack: bool) -> Result<Self> {
        let file = open_append_with_header(scores_path, SCORES_HEADER)?;
        let seen = load_score_keys(scores_path)?;
        let players = load_players(players_path)?;
        Ok(Self {
            file,
            seen,
            players_path: players_path.to_path_buf(),
            players,
            last_acked: 0,
            ack,
        })
    }

    /// Store every complete, well-formed line of a results value; returns
    /// the safe ack point (highest seq), or `None` to skip acking.
    fn store(&mut self, value: &str) -> Result<Option<u32>> {
        let mut max_seq = 0u32;
        let mut all_valid = true;
        for line in value.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
            let Some((seq, player)) = parse_score_line(line) else {
                eprintln!("ignoring malformed score line: {line:?}");
                all_valid = false;
                continue;
            };
            max_seq = max_seq.max(seq);
            if self.seen.insert(line.to_string()) {
                let received_at = Local::now().to_rfc3339();
                writeln!(self.file, "{line},{received_at}")?;
                self.file.flush()?;
                let name = self.player_name(player);
                let shown = if name.is_empty() {
                    player
                } else {
                    name.as_str()
                };
                println!("score #{seq}: {shown}");
            }
        }
        Ok((self.ack && all_valid && max_seq > self.last_acked).then_some(max_seq))
    }

    /// Reload the players CSV from disk (called when the watched file
    /// changes; also the fallback when watching is unavailable).
    fn reload_players(&mut self) {
        match load_players(&self.players_path) {
            Ok(players) => {
                println!(
                    "players reloaded: {} entries from {}",
                    players.len(),
                    self.players_path.display()
                );
                self.players = players;
            }
            Err(e) => eprintln!("reloading {}: {e:#}", self.players_path.display()),
        }
    }

    /// The name for a player UUID ("" when unknown). A miss reloads the
    /// players CSV first, in case the file watch hasn't fired yet.
    fn player_name(&mut self, uuid: &str) -> String {
        if uuid == "-" {
            return String::new();
        }
        if !self.players.contains_key(uuid) {
            self.reload_players();
            if !self.players.contains_key(uuid) {
                eprintln!(
                    "unknown player {uuid}: add them to {}",
                    self.players_path.display()
                );
            }
        }
        self.players.get(uuid).cloned().unwrap_or_default()
    }
}

/// Validate a device CSV line `seq,player_uuid,elapsed_us,at_ms`,
/// returning `(seq, player_uuid)`.
fn parse_score_line(line: &str) -> Option<(u32, &str)> {
    let mut fields = line.split(',');
    let seq = fields.next()?.parse().ok()?;
    let player = fields.next()?;
    fields.next()?.parse::<u64>().ok()?; // elapsed_us
    fields.next()?.parse::<u64>().ok()?; // at_ms
    if fields.next().is_some() || player.is_empty() {
        return None;
    }
    Some((seq, player))
}

/// Open `path` for appending, creating it with a header row if needed.
fn open_append_with_header(path: &Path, header: &str) -> Result<File> {
    let empty = !path.exists() || path.metadata()?.len() == 0;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if empty {
        writeln!(file, "{header}")?;
        file.flush()?;
    }
    Ok(file)
}

/// The dedupe set of an existing scores CSV: each row's device line
/// (first four columns, without `received_at`/`player_name`).
fn load_score_keys(path: &Path) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    let Ok(file) = File::open(path) else {
        return Ok(keys); // doesn't exist yet
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.is_empty() || line.starts_with(SCORES_HEADER) {
            continue;
        }
        let key = line.split(',').take(4).collect::<Vec<_>>().join(",");
        keys.insert(key);
    }
    Ok(keys)
}

/// The players CSV as a UUID -> name map (columns by header name, else
/// the first two). Plain CSV: no quoting, so no commas in names.
fn load_players(path: &Path) -> Result<HashMap<String, String>> {
    let mut players = HashMap::new();
    let Ok(file) = File::open(path) else {
        return Ok(players); // doesn't exist yet
    };
    let mut lines = BufReader::new(file).lines().map_while(Result::ok);
    let Some(header) = lines.next() else {
        return Ok(players);
    };
    let columns: Vec<&str> = header.split(',').collect();
    let at =
        |want: &str, fallback: usize| columns.iter().position(|&c| c == want).unwrap_or(fallback);
    let (uuid_at, name_at) = (at("uuid", 0), at("name", 1));
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if let (Some(uuid), Some(name)) = (fields.get(uuid_at), fields.get(name_at)) {
            players.insert(uuid.to_string(), name.to_string());
        }
    }
    Ok(players)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_line_parsing() {
        assert_eq!(
            parse_score_line("3,af1d45ff-2a68-4abc-b0c0-bc875b30bc25,682585,27444"),
            Some((3, "af1d45ff-2a68-4abc-b0c0-bc875b30bc25"))
        );
        assert_eq!(parse_score_line("4,-,123,456"), Some((4, "-")));
        assert_eq!(parse_score_line("x,-,123,456"), None);
        assert_eq!(parse_score_line("4,-,123"), None);
        assert_eq!(parse_score_line("4,-,123,456,extra"), None);
    }

    #[tokio::test]
    async fn file_watch_fires() {
        let dir = std::env::temp_dir().join(format!("ble-logger-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("players.csv");
        std::fs::write(&file, "uuid,name\n").unwrap();
        let mut rx = watch_file(&file).expect("watcher");
        std::fs::write(&file, "uuid,name\nx,Y\n").unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("watch event within 5 s")
            .expect("channel open");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod signal_tests {
    #[tokio::test]
    async fn sigterm_is_caught() {
        let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        let me = std::process::id().to_string();
        std::process::Command::new("kill")
            .args(["-TERM", &me])
            .status()
            .expect("kill");
        tokio::time::timeout(std::time::Duration::from_secs(5), s.recv())
            .await
            .expect("SIGTERM received within 5 s")
            .expect("stream open");
    }
}

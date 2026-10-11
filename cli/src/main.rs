mod client;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use client::{is_image, Client};

/// CLI for the ESPuino Label Printer API.
#[derive(Parser)]
#[command(
    name = "esplp",
    version,
    about = "Upload and manage printable RFID card labels",
    long_about = None
)]
struct Cli {
    /// Base URL of the API server.
    #[arg(long, env = "ESPLP_SERVER", default_value = "http://127.0.0.1:8080", global = true)]
    server: String,

    /// Operator bearer token. Needed for admin-only commands (`prefix list`);
    /// harmless on customer-level ones.
    #[arg(long, env = "ESPLP_TOKEN", global = true)]
    token: Option<String>,

    /// Print raw JSON responses (default) or a short human summary.
    #[arg(long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage prefixes (one prefix = one customer order / set of cards).
    Prefix {
        #[command(subcommand)]
        cmd: PrefixCmd,
    },
    /// Manage individual cards.
    Card {
        #[command(subcommand)]
        cmd: CardCmd,
    },
    /// Upload a single image as a card.
    Upload(UploadArgs),
    /// Upload every image in a folder to one prefix (batch).
    Batch(BatchArgs),
}

#[derive(Subcommand)]
enum PrefixCmd {
    /// Create a prefix, optionally choosing its uuid.
    Create {
        /// Explicit uuid; omitted = server assigns one.
        #[arg(long)]
        uuid: Option<String>,
    },
    /// List all prefixes.
    List,
    /// Show one prefix and its card count.
    Get { prefix: String },
}

#[derive(Subcommand)]
enum CardCmd {
    /// List the cards (index) in a prefix.
    List { prefix: String },
    /// Show one card's metadata.
    Get { prefix: String, id: i64 },
    /// Delete a card and its image.
    Delete { prefix: String, id: i64 },
}

#[derive(Args)]
struct UploadArgs {
    /// Target prefix uuid.
    prefix: String,
    /// Image file to upload.
    file: PathBuf,
    /// Card id (index within the prefix). Omitted = server assigns the next id.
    #[arg(long)]
    id: Option<i64>,
    /// Card name. Defaults to the file's stem.
    #[arg(long)]
    name: Option<String>,
    /// Optional serial number.
    #[arg(long)]
    serial_number: Option<String>,
    /// Optional black & white back-panel text (song list, etc.).
    #[arg(long)]
    back_text: Option<String>,
}

#[derive(Args)]
struct BatchArgs {
    /// Target prefix uuid.
    prefix: String,
    /// Folder containing card images.
    dir: PathBuf,
    /// Assign sequential ids starting at this number (PUT) instead of letting
    /// the server assign them (POST).
    #[arg(long)]
    start_id: Option<i64>,
    /// Recurse into subfolders.
    #[arg(long)]
    recursive: bool,
    /// Card name template. Placeholders: {stem} {filename} {index}.
    #[arg(long, default_value = "{stem}")]
    name_template: String,
    /// Prefix for auto-generated serial numbers: "<serial-prefix><index>".
    #[arg(long)]
    serial_prefix: Option<String>,
    /// Zero-padded width of the numeric part of the serial number.
    #[arg(long, default_value_t = 3)]
    serial_width: usize,
    /// Back-panel text applied to every card.
    #[arg(long)]
    back_text: Option<String>,
    /// Show what would be uploaded without sending anything.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = Client::new(cli.server.clone(), cli.token.clone())?;
    let quiet = cli.quiet;

    match cli.command {
        Command::Prefix { cmd } => match cmd {
            PrefixCmd::Create { uuid } => {
                emit(client.create_prefix(uuid.as_deref()).await?, quiet)
            }
            PrefixCmd::List => emit(client.list_prefixes().await?, quiet),
            PrefixCmd::Get { prefix } => emit(client.get_prefix(&prefix).await?, quiet),
        },
        Command::Card { cmd } => match cmd {
            CardCmd::List { prefix } => emit(client.list_cards(&prefix).await?, quiet),
            CardCmd::Get { prefix, id } => emit(client.get_card(&prefix, id).await?, quiet),
            CardCmd::Delete { prefix, id } => {
                client.delete_card(&prefix, id).await?;
                if !quiet {
                    println!("deleted card {id} from {prefix}");
                }
                Ok(())
            }
        },
        Command::Upload(a) => {
            if !a.file.is_file() {
                bail!("not a file: {}", a.file.display());
            }
            let name = a
                .name
                .unwrap_or_else(|| file_stem(&a.file));
            let v = client
                .upload_card(
                    &a.prefix,
                    &a.file,
                    &name,
                    a.serial_number.as_deref(),
                    a.back_text.as_deref(),
                    a.id,
                )
                .await?;
            emit(v, quiet)
        }
        Command::Batch(a) => run_batch(&client, a, quiet).await,
    }
}

async fn run_batch(client: &Client, a: BatchArgs, quiet: bool) -> Result<()> {
    let files = collect_images(&a.dir, a.recursive)?;
    if files.is_empty() {
        bail!("no image files found in {}", a.dir.display());
    }

    let total = files.len();
    let mut ok = 0usize;
    let mut failed = 0usize;

    for (i, path) in files.iter().enumerate() {
        let index = i + 1;
        let name = render_name(&a.name_template, path, index);
        let serial = a
            .serial_prefix
            .as_ref()
            .map(|p| format!("{p}{:0width$}", index, width = a.serial_width));
        let id = a.start_id.map(|s| s + i as i64);

        if a.dry_run {
            println!(
                "[dry-run] {index}/{total} {} -> id={} name={name:?}{}",
                path.display(),
                id.map(|v| v.to_string()).unwrap_or_else(|| "auto".into()),
                serial.as_deref().map(|s| format!(" serial={s}")).unwrap_or_default(),
            );
            continue;
        }

        match client
            .upload_card(
                &a.prefix,
                path,
                &name,
                serial.as_deref(),
                a.back_text.as_deref(),
                id,
            )
            .await
        {
            Ok(v) => {
                ok += 1;
                if quiet {
                    println!("ok  {} -> id={}", path.display(), v.get("id").unwrap_or(&Value::Null));
                } else {
                    let id = v.get("id").and_then(|x| x.as_i64()).unwrap_or_default();
                    println!("[{index}/{total}] ok   {} -> card id {id}", path.display());
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("[{index}/{total}] FAIL {}: {e:#}", path.display());
            }
        }
    }

    if a.dry_run {
        println!("(dry-run) {total} file(s) would be uploaded to {}", a.prefix);
    } else {
        println!("done: {ok} uploaded, {failed} failed (of {total}) to {}", a.prefix);
    }
    if failed > 0 {
        bail!("{failed} upload(s) failed");
    }
    Ok(())
}

/// Collect image files, sorted for deterministic ordering.
fn collect_images(dir: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    let mut out = Vec::new();
    if recursive {
        for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
            let entry = entry.with_context(|| format!("walking {}", dir.display()))?;
            if entry.file_type().is_file() && is_image(entry.path()) {
                out.push(entry.path().to_path_buf());
            }
        }
    } else {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.is_file() && is_image(&path) {
                out.push(path);
            }
        }
        out.sort();
    }
    Ok(out)
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("card")
        .to_string()
}

fn render_name(template: &str, path: &Path, index: usize) -> String {
    let stem = file_stem(path);
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    template
        .replace("{stem}", &stem)
        .replace("{filename}", &filename)
        .replace("{index}", &index.to_string())
}

fn emit(v: Value, quiet: bool) -> Result<()> {
    if quiet && v.is_object() {
        if let Some(uuid) = v.get("uuid").and_then(|x| x.as_str()) {
            println!("{uuid}");
            return Ok(());
        }
        if let Some(id) = v.get("id").and_then(|x| x.as_i64()) {
            println!("{id}");
            return Ok(());
        }
    }
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}
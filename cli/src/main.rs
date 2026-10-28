mod client;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use client::{is_image, Client, UploadCard};

/// CLI for the ESPuino Label Printer API.
#[derive(Parser)]
#[command(
    name = "card-printer-cli",
    version,
    about = "Upload and manage printable RFID card labels",
    long_about = None
)]
struct Cli {
    /// Base URL of the API server.
    #[arg(
        long,
        env = "ESPLP_SERVER",
        default_value = "http://127.0.0.1:8080",
        global = true
    )]
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
    /// Create a prefix, optionally choosing its uuid and ribbon.
    Create {
        /// Explicit uuid; omitted = server assigns one.
        #[arg(long)]
        uuid: Option<String>,
        /// The ribbon this job prints with — the one choice per job, because a
        /// batch cannot mix ribbon loads (ymcko|ymckok).
        #[arg(long, value_name = "RIBBON")]
        ribbon: Option<String>,
    },
    /// List all prefixes, optionally filtered by status.
    List {
        /// Only show prefixes in this status
        /// (draft|payment_pending|paid|queued|printing|printed|cancelled).
        #[arg(long, value_name = "STATUS")]
        status: Option<String>,
    },
    /// Show one prefix and its card count.
    Get { prefix: String },
    /// Set a prefix's status (any transition). Requires the operator token.
    SetStatus {
        prefix: String,
        /// Target status
        /// (draft|payment_pending|paid|queued|printing|printed|cancelled).
        status: String,
        /// Optional external order reference (typically set when marking paid).
        #[arg(long)]
        order_ref: Option<String>,
    },
}

#[derive(Subcommand)]
enum CardCmd {
    /// List the cards (index) in a prefix.
    List { prefix: String },
    /// Show one card's metadata.
    Get { prefix: String, id: i64 },
    /// Delete a card and its image.
    Delete { prefix: String, id: i64 },
    /// Upload or replace a card's back image, which also makes the card's back
    /// an `image` back. Only a YMCKO job can print a colour back.
    BackImage {
        prefix: String,
        id: i64,
        /// Colour image to print on the back.
        file: PathBuf,
    },
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
    /// The card's serial number — the value another system may key a card on.
    /// On a card the print process reads a chip for, that is the physical RFID
    /// UID (12 decimal digits); here it is a serial you assign yourself.
    #[arg(long)]
    serial_number: Option<String>,
    /// What the card's back is (none|front|image|markdown). Omitted = none, a
    /// single-sided card; sending a --back-text implies markdown.
    #[arg(long, value_name = "BACK")]
    back: Option<String>,
    /// Back text (markdown: song list, etc.) for a markdown back. Only printed
    /// on a `markdown` back — an `image` back takes its picture through
    /// `card back-image` instead.
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
    /// Those are operator-assigned serials, not RFID UIDs (see --serial-number).
    #[arg(long)]
    serial_prefix: Option<String>,
    /// Zero-padded width of the numeric part of the serial number.
    #[arg(long, default_value_t = 3)]
    serial_width: usize,
    /// What every card's back is (none|front|image|markdown).
    #[arg(long, value_name = "BACK")]
    back: Option<String>,
    /// Back text (markdown) applied to every card.
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
            PrefixCmd::Create { uuid, ribbon } => emit(
                client
                    .create_prefix(uuid.as_deref(), ribbon.as_deref())
                    .await?,
                quiet,
            ),
            PrefixCmd::List { status } => {
                emit(client.list_prefixes(status.as_deref()).await?, quiet)
            }
            PrefixCmd::Get { prefix } => emit(client.get_prefix(&prefix).await?, quiet),
            PrefixCmd::SetStatus {
                prefix,
                status,
                order_ref,
            } => emit(
                client
                    .set_status(&prefix, &status, order_ref.as_deref())
                    .await?,
                quiet,
            ),
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
            CardCmd::BackImage { prefix, id, file } => {
                if !file.is_file() {
                    bail!("not a file: {}", file.display());
                }
                emit(client.upload_back_image(&prefix, id, &file).await?, quiet)
            }
        },
        Command::Upload(a) => {
            if !a.file.is_file() {
                bail!("not a file: {}", a.file.display());
            }
            let name = a.name.unwrap_or_else(|| file_stem(&a.file));
            let v = client
                .upload_card(
                    &a.prefix,
                    &a.file,
                    UploadCard {
                        name: &name,
                        serial_number: a.serial_number.as_deref(),
                        back: a.back.as_deref(),
                        back_text: a.back_text.as_deref(),
                        id: a.id,
                    },
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
                serial
                    .as_deref()
                    .map(|s| format!(" serial={s}"))
                    .unwrap_or_default(),
            );
            continue;
        }

        match client
            .upload_card(
                &a.prefix,
                path,
                UploadCard {
                    name: &name,
                    serial_number: serial.as_deref(),
                    back: a.back.as_deref(),
                    back_text: a.back_text.as_deref(),
                    id,
                },
            )
            .await
        {
            Ok(v) => {
                ok += 1;
                if quiet {
                    println!(
                        "ok  {} -> id={}",
                        path.display(),
                        v.get("id").unwrap_or(&Value::Null)
                    );
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
        println!(
            "(dry-run) {total} file(s) would be uploaded to {}",
            a.prefix
        );
    } else {
        println!(
            "done: {ok} uploaded, {failed} failed (of {total}) to {}",
            a.prefix
        );
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

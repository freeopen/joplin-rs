//! joplin-rs CLI: sync / doctor / list / show / export.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use joplin_rs_core::storage::Storage;
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::WebDavClient;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "joplin-rs",
    version,
    about = "Read-only Joplin client (WebDAV + E2EE)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Config file path (default: platform config dir /joplin-rs/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Data directory (default: platform data dir /joplin-rs)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Sync the vault from the WebDAV target (download only)
    Sync {
        /// E2EE password (prompted if the vault is encrypted and this is missing)
        #[arg(long, env = "JOPLIN_RS_PASSWORD")]
        password: Option<String>,
    },
    /// Check the sync target configuration and master key
    Doctor {
        #[arg(long, env = "JOPLIN_RS_PASSWORD")]
        password: Option<String>,
    },
    /// List notes (or another item type)
    List {
        #[arg(long, default_value = "note")]
        item_type: String,
        /// Folder id to filter notes by
        #[arg(long)]
        folder: Option<String>,
        /// Search query over title/body
        #[arg(long)]
        query: Option<String>,
    },
    /// Show a note (markdown body)
    Show { id: String },
    /// Export a note to a markdown file
    Export {
        id: String,
        /// Output file (default: stdout)
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    webdav_url: String,
    username: String,
    /// WebDAV account password
    #[serde(default)]
    password: Option<String>,
    /// E2EE master key password (distinct from the WebDAV password)
    #[serde(default)]
    e2ee_password: Option<String>,
}

fn config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("joplin-rs")
            .join("config.toml")
    })
}

fn data_dir(cli: &Cli) -> PathBuf {
    cli.data_dir.clone().unwrap_or_else(|| {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("joplin-rs")
    })
}

fn load_config(cli: &Cli) -> Result<Config> {
    let path = config_path(cli);
    if !path.exists() {
        bail!(
            "config not found at {}\nCreate it with:\n  webdav_url = \"https://example.com/dav/joplin\"\n  username = \"user\"\n  password = \"pass\"",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(&path).context("read config")?;
    toml::from_str(&raw).context("parse config.toml")
}

fn db(cli: &Cli) -> Result<Storage> {
    let dir = data_dir(cli);
    Storage::open(&dir.join("joplin-rs.db")).context("open database")
}

fn type_from_str(s: &str) -> Result<i64> {
    match s {
        "note" => Ok(joplin_rs_core::items::TYPE_NOTE),
        "folder" => Ok(joplin_rs_core::items::TYPE_FOLDER),
        "tag" => Ok(joplin_rs_core::items::TYPE_TAG),
        "resource" => Ok(joplin_rs_core::items::TYPE_RESOURCE),
        other => bail!("unknown item type {other} (note|folder|tag|resource)"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let cli = &cli;
    match &cli.command {
        Command::Sync { password } => {
            let cfg = load_config(cli)?;
            let storage = db(cli)?;
            let client = WebDavClient::new(
                &cfg.webdav_url,
                &cfg.username,
                cfg.password.as_deref().unwrap_or(""),
            )?;
            let engine = SyncEngine::new(&client, &storage);
            let password = resolve_password(&cfg, password.clone())?;
            let outcome = engine
                .sync(password.as_deref(), &mut |msg| {
                    println!("{msg}");
                })
                .await?;
            println!(
                "done: {} downloaded, {} deleted, {} resources, {} unsupported, {} pending decryption",
                outcome.downloaded, outcome.deleted, outcome.resources_downloaded, outcome.skipped_unsupported, outcome.pending_decryption.len()
            );
            if !outcome.pending_decryption.is_empty() {
                println!("items stored encrypted - run sync again with the E2EE password to decrypt them");
            }
            if !outcome.errors.is_empty() {
                eprintln!("{} errors:", outcome.errors.len());
                for e in &outcome.errors {
                    eprintln!("  - {e}");
                }
            }
        }
        Command::Doctor { password } => {
            let cfg = load_config(cli)?;
            let client = WebDavClient::new(
                &cfg.webdav_url,
                &cfg.username,
                cfg.password.as_deref().unwrap_or(""),
            )?;
            println!("url: {}", cfg.webdav_url);
            let tmp_storage = Storage::open_in_memory()?;
            let engine = SyncEngine::new(&client, &tmp_storage);
            let info = engine.fetch_sync_info().await.context("fetch info.json")?;
            println!("sync target version: {}", info.version);
            println!("e2ee: {}", info.e2ee_enabled());
            if info.e2ee_enabled() {
                let pw = resolve_password(&cfg, password.clone())?;
                match engine.unlock_master_key(&info, pw.as_deref().unwrap_or("")) {
                    Ok(_) => println!("master key: OK (password accepted)"),
                    Err(e) => {
                        println!("master key: FAILED - {e}");
                        bail!("master key check failed");
                    }
                }
            } else {
                println!("master key: n/a (encryption disabled)");
            }
        }
        Command::List {
            item_type,
            folder,
            query,
        } => {
            let storage = db(cli)?;
            let t = type_from_str(item_type)?;
            let items: Vec<joplin_rs_core::items::Item> = if t == joplin_rs_core::items::TYPE_NOTE {
                if let Some(q) = query {
                    storage.search_notes(q)?
                } else {
                    storage.notes(folder.as_deref().unwrap_or(""), true)?
                }
            } else {
                storage.items_by_type(t)?
            };
            for item in items {
                let id = item
                    .props
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let title = item
                    .props
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(no title)");
                let flag = if item
                    .props
                    .get("is_todo")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0)
                    == 1
                {
                    if item
                        .props
                        .get("todo_completed")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0)
                        > 0
                    {
                        "[x] "
                    } else {
                        "[ ] "
                    }
                } else {
                    ""
                };
                println!("{id}  {flag}{title}");
            }
        }
        Command::Show { id } => {
            let storage = db(cli)?;
            let note = storage.note(id)?.context("note not found")?;
            let title = note
                .props
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            println!("{title}");
            println!("{}", "=".repeat(title.len()));
            println!("{}", note.body);
        }
        Command::Export { id, output } => {
            let storage = db(cli)?;
            let note = storage.note(id)?.context("note not found")?;
            let title = note
                .props
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("note");
            let content = format!("# {title}\n\n{}", note.body);
            match output {
                Some(path) => {
                    std::fs::write(path, content).context("write output")?;
                    println!("wrote {}", path.display());
                }
                None => {
                    std::io::stdout().write_all(content.as_bytes())?;
                }
            }
        }
    }
    Ok(())
}

/// Resolve the E2EE password: CLI arg > config `e2ee_password` > prompt.
/// The WebDAV password is never used as the E2EE password.
fn resolve_password(cfg: &Config, cli_password: Option<String>) -> Result<Option<String>> {
    if let Some(p) = cli_password {
        return Ok(Some(p));
    }
    if let Some(p) = &cfg.e2ee_password {
        return Ok(Some(p.clone()));
    }
    print!("E2EE password (empty to skip decryption): ");
    std::io::stdout().flush()?;
    // read_password fails on a non-tty stdin; treat that as "skip decryption"
    // so scripted/CI runs can proceed without a password.
    let pw = rpassword::read_password().unwrap_or_default();
    if pw.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(pw))
    }
}

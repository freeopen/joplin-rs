//! joplin-rs CLI: sync / doctor / list / show / export.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use joplin_rs_core::secret::LocalKey;
use joplin_rs_core::storage::{SearchOptions, Storage};
use joplin_rs_core::sync::SyncEngine;
use joplin_rs_core::webdav::{TlsOptions, WebDavClient};
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use toml::Value;

#[derive(Parser)]
#[command(
    name = "joplin-rs",
    version = joplin_rs_core::GIT_VERSION,
    about = "Read-only Joplin client (WebDAV + E2EE)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Config file path (default: ~/.joplin/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Data directory (default: ~/.joplin)
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
    },
    /// Show a note (markdown body)
    Show { id: String },
    /// Search notes over title and body (all words must match)
    Search {
        /// Query words (AND); e.g. search meeting notes
        query: String,
        /// Restrict to a notebook id (see: list --item-type folder)
        #[arg(long)]
        folder: Option<String>,
        /// Restrict to notes with a tag whose title matches this text
        #[arg(long)]
        tag: Option<String>,
        /// Only todos
        #[arg(long)]
        todo: bool,
        /// Only completed todos (implies --todo)
        #[arg(long)]
        completed: bool,
        /// Show a content snippet under each hit
        #[arg(long)]
        snippets: bool,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Export a note to a markdown file
    Export {
        id: String,
        /// Output file (default: stdout)
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Store the WebDAV password in config.toml encrypted at rest.
    ///
    /// The password is encrypted with a machine-local key (~/.joplin/secret.key,
    /// generated on first use) and written as an `enc:v1:` value. With no
    /// --password and a TTY, the password is prompted twice without echo.
    SetPassword {
        /// New WebDAV password (prompted securely if omitted; reads one line
        /// from stdin when stdin is not a TTY)
        #[arg(long)]
        password: Option<String>,
        /// Also store the E2EE master-key password encrypted
        #[arg(long)]
        e2ee: bool,
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
    /// Accept self-signed/invalid TLS certificates (self-hosted servers)
    #[serde(default)]
    insecure_tls: bool,
    /// Path to a PEM CA certificate to trust (alternative to insecure_tls)
    #[serde(default)]
    ca_cert: Option<String>,
}

impl Config {
    fn tls_options(&self) -> TlsOptions {
        TlsOptions {
            insecure: self.insecure_tls,
            ca_path: self.ca_cert.as_ref().map(std::path::PathBuf::from),
        }
    }
}

/// Default base directory for config and data: ~/.joplin/
fn default_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".joplin")
}

fn config_path(cli: &Cli) -> PathBuf {
    cli.config
        .clone()
        .unwrap_or_else(|| default_base_dir().join("config.toml"))
}

fn data_dir(cli: &Cli) -> PathBuf {
    cli.data_dir.clone().unwrap_or_else(default_base_dir)
}

/// Path of the machine-local key used to encrypt config secrets.
fn secret_key_path(cli: &Cli) -> PathBuf {
    data_dir(cli).join("secret.key")
}

fn local_key(cli: &Cli) -> Result<LocalKey> {
    LocalKey::load_or_create(&secret_key_path(cli))
        .context("load local secret key (~/.joplin/secret.key)")
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

/// Resolve the plaintext WebDAV password: decrypts `enc:v1:` values with the
/// local key, passes legacy plaintext through.
fn webdav_password(cli: &Cli, cfg: &Config) -> Result<String> {
    let Some(stored) = cfg.password.as_deref() else {
        return Ok(String::new());
    };
    local_key(cli)?
        .decrypt(stored)
        .context("decrypt WebDAV password (wrong/missing ~/.joplin/secret.key?)")
}

/// Read a password without echo: --password wins, else interactive prompt
/// (with confirmation), else one line from stdin for scripted use.
fn read_new_password(cli_password: Option<String>, confirm: bool) -> Result<String> {
    if let Some(pw) = cli_password {
        return Ok(pw);
    }
    if std::io::stdin().is_terminal() {
        print!("New password: ");
        std::io::stdout().flush()?;
        let pw = rpassword::read_password().context("read password")?;
        if confirm {
            print!("Confirm password: ");
            std::io::stdout().flush()?;
            let again = rpassword::read_password().context("read password")?;
            if pw != again {
                bail!("passwords do not match");
            }
        }
        Ok(pw)
    } else {
        // Non-interactive: read a single line (piped input, e.g.
        // `cat pw.txt | joplin-rs set-password`).
        let mut pw = String::new();
        std::io::stdin()
            .read_line(&mut pw)
            .context("read password from stdin")?;
        // Trim only the trailing newline, never other characters.
        while pw.ends_with('\n') || pw.ends_with('\r') {
            pw.pop();
        }
        if pw.is_empty() {
            bail!("empty password from stdin");
        }
        Ok(pw)
    }
}

/// Rewrite one string key in the config file, preserving every other field,
/// comment-free TOML is round-tripped via toml::Value.
fn write_config_field(cli: &Cli, key: &str, value: String) -> Result<()> {
    let path = config_path(cli);
    let mut table: toml::Table = if path.exists() {
        let raw = std::fs::read_to_string(&path).context("read config")?;
        raw.parse().context("parse config.toml")?
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("create config dir")?;
        }
        toml::Table::new()
    };
    table.insert(key.to_owned(), Value::String(value));
    let raw = toml::to_string_pretty(&table).context("serialize config")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, raw).context("write config")?;
    std::fs::rename(&tmp, &path).context("install config.toml")?;
    Ok(())
}

fn cmd_set_password(cli: &Cli, password: Option<String>, e2ee: bool) -> Result<()> {
    let key = local_key(cli)?;
    let pw = read_new_password(password, true)?;
    let encrypted = key.encrypt(&pw).context("encrypt password")?;
    if e2ee {
        write_config_field(cli, "e2ee_password", encrypted)
            .context("store encrypted e2ee_password")?;
        println!(
            "encrypted E2EE password stored in {}",
            config_path(cli).display()
        );
    } else {
        write_config_field(cli, "password", encrypted).context("store encrypted password")?;
        println!(
            "encrypted WebDAV password stored in {}",
            config_path(cli).display()
        );
    }
    println!(
        "machine key: {} (keep this file; losing it makes stored passwords undecryptable)",
        secret_key_path(cli).display()
    );
    Ok(())
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

/// One line in `list` output: id + todo flag + title.
fn print_list_item(item: &joplin_rs_core::items::Item) {
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

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly (exit 141) when the downstream pipe closes, like every
    // other Unix CLI, instead of panicking on `list | head`.
    #[cfg(unix)]
    unsafe {
        extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }
        signal(13 /* SIGPIPE */, 0 /* SIG_DFL */);
    }

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
            let dav_pw = webdav_password(cli, &cfg)?;
            let client = WebDavClient::new_with_tls(
                &cfg.webdav_url,
                &cfg.username,
                &dav_pw,
                &cfg.tls_options(),
            )?;
            let engine = SyncEngine::new(&client, &storage);
            let password = resolve_password(cli, &cfg, password.clone())?;
            let outcome = engine
                .sync(password.as_deref(), &mut |msg| {
                    println!("{msg}");
                })
                .await?;
            println!(
                "done: {} downloaded, {} deleted, {} resources ({} pending), {} unsupported, {} pending decryption",
                outcome.downloaded, outcome.deleted, outcome.resources_downloaded,
                outcome.resources_pending, outcome.skipped_unsupported, outcome.pending_decryption.len()
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
            let dav_pw = webdav_password(cli, &cfg)?;
            let client = WebDavClient::new_with_tls(
                &cfg.webdav_url,
                &cfg.username,
                &dav_pw,
                &cfg.tls_options(),
            )?;
            println!("url: {}", cfg.webdav_url);
            let tmp_storage = Storage::open_in_memory()?;
            let engine = SyncEngine::new(&client, &tmp_storage);
            let info = engine.fetch_sync_info().await.context("fetch info.json")?;
            println!("sync target version: {}", info.version);
            println!("e2ee: {}", info.e2ee_enabled());
            if info.e2ee_enabled() {
                let pw = resolve_password(cli, &cfg, password.clone())?;
                match engine.unlock_master_key_any(&info, pw.as_deref().unwrap_or("")) {
                    Ok((_, key_id)) => {
                        let suffix =
                            if info.active_master_key_id().as_deref() == Some(key_id.as_str()) {
                                ""
                            } else {
                                ", not the active key"
                            };
                        println!("master key: OK (password accepted, key {key_id}{suffix})")
                    }
                    Err(e) => {
                        println!("master key: FAILED - {e}");
                        bail!("master key check failed");
                    }
                }
            } else {
                println!("master key: n/a (encryption disabled)");
            }
        }
        Command::List { item_type, folder } => {
            let storage = db(cli)?;
            let t = type_from_str(item_type)?;
            // Full-text/title+body search lives in the `search` command.
            if t == joplin_rs_core::items::TYPE_NOTE {
                let items = storage.notes(folder.as_deref().unwrap_or(""), true)?;
                for item in &items {
                    print_list_item(item);
                }
            } else {
                for item in storage.items_by_type(t)? {
                    print_list_item(&item);
                }
            }
        }
        Command::Search {
            query,
            folder,
            tag,
            todo,
            completed,
            snippets,
            json,
        } => {
            let storage = db(cli)?;
            let opts = SearchOptions {
                folder_id: folder.clone(),
                tag: tag.clone(),
                todo_only: *todo || *completed,
                completed_only: *completed,
            };
            let hits = storage.search_notes(query, &opts)?;
            if *json {
                let rows: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "id": h.id,
                            "title": h.title,
                            "snippet": h.snippet,
                            "is_todo": h.is_todo,
                            "completed": h.completed,
                            "folder_id": h.folder_id,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into())
                );
            } else {
                if hits.is_empty() {
                    println!("no matches");
                }
                for h in &hits {
                    let flag = if h.is_todo {
                        if h.completed {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    } else {
                        ""
                    };
                    println!("{}  {}{}", h.id, flag, h.title);
                    if *snippets {
                        println!("        {}", h.snippet);
                    }
                }
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
        Command::SetPassword { password, e2ee } => {
            cmd_set_password(cli, password.clone(), *e2ee)?;
        }
    }
    Ok(())
}

/// Resolve the E2EE password: CLI arg > decrypted config `e2ee_password` > prompt.
/// The WebDAV password is never used as the E2EE password.
fn resolve_password(
    cli: &Cli,
    cfg: &Config,
    cli_password: Option<String>,
) -> Result<Option<String>> {
    if let Some(p) = cli_password {
        return Ok(Some(p));
    }
    if let Some(stored) = &cfg.e2ee_password {
        return Ok(Some(local_key(cli)?.decrypt(stored).context(
            "decrypt e2ee_password (wrong/missing ~/.joplin/secret.key?)",
        )?));
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

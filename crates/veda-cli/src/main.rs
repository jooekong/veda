mod client;
mod config;
mod init;
mod status;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SearchDetail {
    Abstract,
    Overview,
    Full,
}

impl SearchDetail {
    fn as_str(self) -> &'static str {
        match self {
            SearchDetail::Abstract => "abstract",
            SearchDetail::Overview => "overview",
            SearchDetail::Full => "full",
        }
    }
}

#[derive(Parser)]
#[command(name = "veda", about = "Veda CLI client", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Server URL (overrides config)
    #[arg(long, global = true)]
    server: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Quick re-auth: store an existing key (typically copied from
    /// another machine) into ~/.config/veda/config.toml. Accepts both
    /// account keys (`vk_*`) and workspace keys (`wk_*`); the prefix
    /// determines which slot the key lands in. For first-time setup
    /// use `veda init` instead.
    Login {
        /// API key to use. `vk_*` is an account key (run `veda init`
        /// afterwards to pick a workspace); `wk_*` is a workspace key
        /// (data commands work immediately, but account-scope commands
        /// won't). No env-var fallback by design — make the switch
        /// explicit so a stale shell var can't silently override the
        /// config you meant to keep.
        #[arg(long)]
        api_key: String,
    },
    /// Show current config (server URL, key state, workspace) and a
    /// best-effort server reachability ping.
    Status,
    /// First-time setup. Default: zero-input anonymous onboard (server
    /// mints an account + default workspace + keys in one shot,
    /// nothing to type). Pass `--email` to register a named account
    /// instead, or `--login` to attach an existing one. The global
    /// `--server` flag picks the server URL.
    Init {
        /// Use an existing account (implies named mode). Skips
        /// `--name`; `--email` + `--password` are required.
        #[arg(long)]
        login: bool,
        /// Display name for new account (named mode only).
        #[arg(long)]
        name: Option<String>,
        /// Email for named account. Presence switches from anonymous
        /// to named onboarding.
        #[arg(long)]
        email: Option<String>,
        /// Pass via env or terminal prompt; `--password` on argv is
        /// visible in `ps`. Required in --non-interactive named mode.
        #[arg(long)]
        password: Option<String>,
        /// Workspace name (default "default")
        #[arg(long)]
        workspace: Option<String>,
        /// Fail with a clear error instead of prompting for missing
        /// fields. Designed for CI / scripts.
        #[arg(long)]
        non_interactive: bool,
    },
    /// Upgrade the anonymous account created by `veda init` into a
    /// named one by attaching email + password. The current API key
    /// keeps working; only the account's recoverable identity changes.
    Claim {
        #[arg(long)]
        email: String,
        /// Pass via terminal prompt or `VEDA_PASSWORD`; argv leaks via
        /// `ps`.
        #[arg(long)]
        password: Option<String>,
        /// Optional human-friendly name (replaces auto-generated
        /// `anon-xxxx`). Empty string is rejected.
        #[arg(long)]
        name: Option<String>,
    },
    /// Account management (hidden — use `veda init` instead; kept as
    /// an escape hatch for debugging).
    #[command(hide = true)]
    Account {
        #[command(subcommand)]
        action: AccountCmd,
    },
    /// Workspace management (hidden — `veda init` creates and selects
    /// the default workspace; kept as an escape hatch).
    #[command(hide = true)]
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCmd,
    },
    /// Copy file to server
    Cp {
        /// Local file path or "-" for stdin
        src: String,
        /// Remote path on server
        dst: String,
    },
    /// Read file from server
    Cat {
        /// Remote path
        path: String,
        /// Line range (e.g. "1:10")
        #[arg(long)]
        lines: Option<String>,
    },
    /// List directory
    Ls {
        /// Remote directory path
        #[arg(default_value = "/")]
        path: String,
    },
    /// Move/rename file
    Mv { src: String, dst: String },
    /// Delete file or directory
    Rm { path: String },
    /// Append content to a file
    Append {
        /// Remote path
        path: String,
        /// Content to append (or "-" for stdin)
        content: String,
    },
    /// Create directory
    Mkdir { path: String },
    /// Search files
    Search {
        query: String,
        #[arg(long, default_value = "hybrid")]
        mode: String,
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Detail level
        #[arg(long, value_enum, default_value_t = SearchDetail::Full)]
        detail_level: SearchDetail,
    },
    /// Grep file contents (substring match, returns file:line:content)
    Grep {
        /// Substring to find
        pattern: String,
        /// Optional path prefix to scope the scan (default: /)
        path: Option<String>,
        /// Case-insensitive match
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Maximum number of hits before stopping (1..=1000, default 100)
        #[arg(long, default_value = "100")]
        limit: usize,
    },
    /// Show the L0 abstract — a single condensed sentence about the file
    /// or directory. Use `veda overview` for the longer L1 prose.
    Abstract {
        /// Remote path
        path: String,
    },
    /// Show the L1 overview (~2k tokens of structured prose) for a file
    /// or directory. Pricier than `veda abstract`; use that first.
    Overview {
        /// Remote path
        path: String,
    },
    /// Collection management
    Collection {
        #[command(subcommand)]
        action: CollectionCmd,
    },
    /// Execute SQL query
    Sql { query: String },
    /// Configuration management (hidden — `veda init` / `veda login`
    /// handle the common cases; kept for direct edits).
    #[command(hide = true)]
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// Create a new account
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        email: String,
        #[arg(long)]
        password: String,
    },
    /// Login to get API key
    Login {
        #[arg(long)]
        email: String,
        #[arg(long)]
        password: String,
    },
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Create a new workspace
    Create {
        #[arg(long)]
        name: String,
    },
    /// List workspaces
    List,
    /// Select active workspace and create a workspace key
    Use {
        /// Workspace ID
        id: String,
    },
}

#[derive(Subcommand)]
enum CollectionCmd {
    /// Create a collection
    Create {
        name: String,
        /// Schema as JSON array
        #[arg(long)]
        schema: String,
        /// Embedding source field
        #[arg(long)]
        embed_source: Option<String>,
    },
    /// List collections
    List,
    /// Describe a collection (show schema details)
    Desc { name: String },
    /// Delete a collection
    Delete { name: String },
    /// Insert rows (JSON array from stdin or argument)
    Insert {
        name: String,
        /// JSON array of rows
        data: String,
    },
    /// Search a collection
    Search {
        name: String,
        query: String,
        #[arg(long, default_value = "5")]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Show current configuration
    Show,
    /// Set a configuration value
    Set { key: String, value: String },
}

fn mask_secret(s: &str) -> String {
    if s.len() <= 10 {
        "***".into()
    } else {
        format!("{}...{}", &s[..6], &s[s.len() - 4..])
    }
}

/// Prompt for a value with an optional `[default]` hint. Returns the
/// trimmed input, or the default if input was empty. Re-prompts on empty
/// input when there is no default. Errors on stdin EOF or read failure.
fn prompt_or(label: &str, default: Option<&str>) -> anyhow::Result<String> {
    use std::io::{BufRead, Write};
    loop {
        match default {
            Some(d) => print!("{label} [{d}]: "),
            None => print!("{label}: "),
        }
        std::io::stdout().flush()?;
        let mut buf = String::new();
        let n = std::io::stdin().lock().read_line(&mut buf)?;
        if n == 0 {
            anyhow::bail!("unexpected EOF on stdin while reading {label}");
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            if let Some(d) = default {
                return Ok(d.to_string());
            }
            // No default + empty input → prompt again.
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

/// Resolve an init param: prefer the flag, else prompt, else apply
/// default. In `--non-interactive` mode, missing-with-no-default is a
/// hard error rather than a prompt.
fn resolve_field(
    label: &str,
    flag: Option<String>,
    default: Option<&str>,
    non_interactive: bool,
    has_default: bool,
) -> anyhow::Result<String> {
    if let Some(v) = flag {
        let trimmed = v.trim();
        if trimmed.is_empty() && !has_default {
            anyhow::bail!("--{} cannot be empty", label.to_lowercase().replace(' ', "-"));
        }
        return Ok(if trimmed.is_empty() {
            default.unwrap_or("").to_string()
        } else {
            trimmed.to_string()
        });
    }
    if non_interactive {
        if let Some(d) = default {
            return Ok(d.to_string());
        }
        anyhow::bail!(
            "--non-interactive but --{} not provided",
            label.to_lowercase().replace(' ', "-")
        );
    }
    prompt_or(label, default)
}

/// Resolve the password specifically: never echo, never default. Reads
/// from `--password`, then `VEDA_PASSWORD`, then a tty prompt. Errors in
/// non-interactive mode if neither source is set.
fn resolve_password(flag: Option<String>, non_interactive: bool) -> anyhow::Result<String> {
    if let Some(v) = flag {
        if v.is_empty() {
            anyhow::bail!("--password cannot be empty");
        }
        return Ok(v);
    }
    if let Ok(v) = std::env::var("VEDA_PASSWORD") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if non_interactive {
        anyhow::bail!(
            "--non-interactive but neither --password nor $VEDA_PASSWORD set"
        );
    }
    let pw = rpassword::prompt_password("Password: ")?;
    if pw.is_empty() {
        anyhow::bail!("password cannot be empty");
    }
    Ok(pw)
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn resolve_field_uses_flag_value() {
        let out = resolve_field("Email", Some("a@b.com".into()), None, true, false).unwrap();
        assert_eq!(out, "a@b.com");
    }

    #[test]
    fn resolve_field_trims_flag_value() {
        let out = resolve_field("Email", Some("  a@b.com  ".into()), None, true, false).unwrap();
        assert_eq!(out, "a@b.com");
    }

    #[test]
    fn resolve_field_empty_flag_with_default_uses_default() {
        let out = resolve_field("Workspace", Some("".into()), Some("default"), true, true).unwrap();
        assert_eq!(out, "default");
    }

    #[test]
    fn resolve_field_empty_flag_without_default_errors() {
        let err = resolve_field("Email", Some("".into()), None, true, false).unwrap_err();
        assert!(err.to_string().contains("--email"), "msg: {err}");
    }

    #[test]
    fn resolve_field_non_interactive_no_flag_with_default_uses_default() {
        let out = resolve_field("Workspace", None, Some("default"), true, true).unwrap();
        assert_eq!(out, "default");
    }

    #[test]
    fn resolve_field_non_interactive_no_flag_no_default_errors() {
        let err = resolve_field("Email", None, None, true, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("non-interactive"), "msg: {msg}");
        assert!(msg.contains("--email"), "msg: {msg}");
    }

    #[test]
    fn resolve_password_uses_flag() {
        let out = resolve_password(Some("hunter2".into()), true).unwrap();
        assert_eq!(out, "hunter2");
    }

    #[test]
    fn resolve_password_empty_flag_errors() {
        let err = resolve_password(Some("".into()), true).unwrap_err();
        assert!(err.to_string().contains("--password"), "msg: {err}");
    }

    #[test]
    fn resolve_password_non_interactive_no_flag_no_env_errors() {
        // Make sure VEDA_PASSWORD isn't leaking in from the parent shell.
        // SAFETY: tests in this module mutate process env. Other tests
        // in this binary don't read VEDA_PASSWORD, so removal is local.
        unsafe {
            std::env::remove_var("VEDA_PASSWORD");
        }
        let err = resolve_password(None, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("non-interactive"), "msg: {msg}");
        assert!(msg.contains("VEDA_PASSWORD"), "msg: {msg}");
    }
}

async fn print_summary_layer(
    c: &client::Client,
    ws_key: &str,
    path: &str,
    endpoint: &str,
    label: &str,
    json_field: &str,
) -> anyhow::Result<()> {
    let (status, resp) = c.get_summary_layer(ws_key, path, endpoint).await?;
    match status {
        200 => {
            let data = &resp["data"];
            println!("Path: {}", data["path"].as_str().unwrap_or("?"));
            println!("\n--- {label} ---");
            println!("{}", data[json_field].as_str().unwrap_or("(none)"));
            Ok(())
        }
        202 => {
            let msg = resp["error"].as_str().unwrap_or("pending");
            println!("Summary not ready yet ({msg}). Retry in a few seconds.");
            std::process::exit(2);
        }
        501 => {
            let msg = resp["error"].as_str().unwrap_or("summary disabled");
            println!("Summary unavailable: {msg}");
            println!("(Ask Joe to add an [llm] section to the server config.)");
            std::process::exit(3);
        }
        404 => {
            let msg = resp["error"].as_str().unwrap_or("not found");
            anyhow::bail!("HTTP 404: {msg}");
        }
        _ => anyhow::bail!("unexpected HTTP {status}: {resp}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut cfg = config::CliConfig::load()?;

    if let Some(ref s) = cli.server {
        cfg.server_url = s.clone();
    }

    let c = client::Client::new(&cfg.server_url);

    match cli.command {
        Commands::Login { api_key } => {
            // Prefix-route by key type. Server-side, `vk_` is the
            // account key (mints workspaces, lists them) and `wk_` is
            // the workspace key (data-plane only). Storing a wk_ into
            // the api_key slot would give a confusing 401 on the first
            // command, so route up front.
            if api_key.starts_with("wk_") {
                init::apply_workspace_key(&mut cfg, api_key);
                cfg.save()?;
                println!("Workspace key saved.");
                println!(
                    "Data commands (cp/cat/ls/...) work now. The workspace ID is unknown — \
                     account-scope commands won't work; paste a `vk_` key with `veda login \
                     --api-key vk_…` if you need them."
                );
            } else if api_key.starts_with("vk_") {
                init::apply_login(&mut cfg, api_key);
                cfg.save()?;
                println!("API key saved to config.");
                println!("Run `veda init` (or `veda workspace use <id>`) to pick a workspace.");
            } else {
                anyhow::bail!(
                    "api_key must start with 'vk_' (account key) or 'wk_' (workspace key); \
                     got a key with neither prefix"
                );
            }
        }
        Commands::Status => {
            // Skip the ping when nothing is configured — there's no
            // server to talk to that the user opted into.
            let reachable = if cfg.api_key.is_some() || cfg.workspace_key.is_some() {
                Some(status::ping_server(&cfg.server_url).await)
            } else {
                None
            };
            print!("{}", status::render_status(&cfg, reachable));
        }
        Commands::Init {
            login,
            name,
            email,
            password,
            workspace,
            non_interactive,
        } => {
            // The global `--server` flag was already merged into cfg at the
            // top of main, so cfg.server_url is the source of truth.
            // Anonymous mode is genuinely zero-prompt — confirming the
            // server URL would defeat the "0-input" pitch and breaks
            // non-tty contexts (curl | sh, CI). Only prompt in named
            // mode when the user has signaled they want interaction.
            let is_anonymous =
                email.is_none() && !login && name.is_none() && workspace.is_none();
            let server_url = if non_interactive || is_anonymous {
                cfg.server_url.clone()
            } else {
                prompt_or("Server URL", Some(&cfg.server_url))?
            };

            // Anonymous onboard is the default: no email/password
            // required, one server round-trip, instantly usable.
            // Switch to named mode iff the user provided any flag
            // that signals "I want a recoverable / custom identity"
            // — --email, --login, --name, or --workspace (anon always
            // uses the "default" workspace, so a custom name implies
            // named mode).
            if is_anonymous {
                // Refuse to overwrite an existing identity. Running
                // `veda init` again after onboarding would silently
                // throw away the previous account binding, which is
                // hard to recover from. The user has to clear config
                // (or pass --email / --login) on purpose.
                if cfg.api_key.is_some() || cfg.workspace_key.is_some() {
                    anyhow::bail!(
                        "this machine is already onboarded (see `veda status`). \
                         To attach a different account use `veda login --api-key <key>`, \
                         or delete the config file first."
                    );
                }
                let new_client = client::Client::new(&server_url);
                let outcome_result =
                    init::run_anonymous(&new_client, &mut cfg, server_url).await;
                cfg.save()?;
                let outcome = outcome_result?;
                let cfg_path = config::CliConfig::default_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<config path>".into());
                println!();
                println!("✓ anonymous account created (id {})", outcome.account_id);
                println!("✓ default workspace ready (id {})", outcome.workspace_id);
                println!("✓ keys saved to {cfg_path}");
                println!();
                println!("Try: veda cp ./README.md /docs/readme.md");
                println!(
                    "Later, attach an email so you can recover this account from another \
                     machine: veda claim --email you@example.com"
                );
                return Ok(());
            }

            let email = resolve_field("Email", email, None, non_interactive, false)?;
            // In non-interactive named mode the user often only has
            // email + password (e.g. CI / agent). Derive name from
            // the email's local-part so they don't have to repeat
            // themselves; the server-side 409 fallback to login means
            // the name only matters when actually creating a new
            // account.
            let derived_name: Option<String> = email
                .split('@')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let name = if login {
                String::new()
            } else {
                resolve_field("Name", name.or(derived_name), None, non_interactive, false)?
            };
            let password = resolve_password(password, non_interactive)?;
            // Workspace is silent: `--workspace foo` overrides for the
            // rare power user, otherwise everyone gets `default`. No
            // prompt — knowing the workspace concept shouldn't be a
            // prerequisite for first-time onboarding.
            let workspace = workspace
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("default")
                .to_string();

            let params = init::InitParams {
                server_url,
                login,
                name,
                email,
                password,
                workspace,
            };
            let new_client = client::Client::new(&params.server_url);
            // Save unconditionally so partial successes (e.g. account
            // created but workspace-key mint failed) leave the user
            // with a real `api_key` they can recover with — the
            // contract documented in run_init's comment.
            let outcome_result = init::run_init(&new_client, &mut cfg, params).await;
            cfg.save()?;
            let outcome = outcome_result?;

            let cfg_path = config::CliConfig::default_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<config path>".into());
            println!();
            if outcome.created_account {
                println!("✓ account created (id {})", outcome.account_id);
            } else {
                println!("✓ logged in (account id {})", outcome.account_id);
            }
            if outcome.created_workspace {
                println!("✓ workspace created (id {})", outcome.workspace_id);
            } else {
                println!("✓ using existing workspace (id {})", outcome.workspace_id);
            }
            println!("✓ workspace key saved to {cfg_path}");
            println!();
            println!("Try: veda cp ./README.md /docs/readme.md");
        }
        Commands::Claim {
            email,
            password,
            name,
        } => {
            // Fall back to VEDA_API_KEY env when the config has no
            // key — supports "I pasted my anon vk_ into the shell but
            // didn't persist it yet" flow. Cfg is the source of truth
            // when both are set so a stale env var can't silently
            // hijack the claim to a wrong account.
            if cfg.api_key.as_deref().map_or(true, str::is_empty) {
                if let Ok(env_key) = std::env::var("VEDA_API_KEY") {
                    if !env_key.is_empty() {
                        cfg.api_key = Some(env_key);
                    }
                }
            }
            let password = resolve_password(password, false)?;
            let new_client = client::Client::new(&cfg.server_url);
            let account_id = init::run_claim(&new_client, &cfg, email, password, name).await?;
            println!("✓ account claimed (id {account_id})");
            println!("Your API key is unchanged; future logins use email + password.");
        }
        Commands::Account { action } => match action {
            AccountCmd::Create {
                name,
                email,
                password,
            } => {
                let resp = c.create_account(&name, &email, &password).await?;
                let api_key = resp["data"]["api_key"].as_str().unwrap_or("");
                cfg.api_key = Some(api_key.to_string());
                cfg.save()?;
                println!("Account created. API key saved to config.");
                println!("Account ID: {}", resp["data"]["account_id"]);
            }
            AccountCmd::Login { email, password } => {
                let resp = c.login(&email, &password).await?;
                let api_key = resp["data"]["api_key"].as_str().unwrap_or("");
                cfg.api_key = Some(api_key.to_string());
                cfg.save()?;
                println!("Logged in. API key saved.");
            }
        },
        Commands::Workspace { action } => match action {
            WorkspaceCmd::Create { name } => {
                let resp = c.create_workspace(cfg.api_key()?, &name).await?;
                println!(
                    "Workspace created: {}",
                    resp["data"]["id"].as_str().unwrap_or("")
                );
            }
            WorkspaceCmd::List => {
                let resp = c.list_workspaces(cfg.api_key()?).await?;
                if let Some(arr) = resp["data"].as_array() {
                    for ws in arr {
                        println!(
                            "{}\t{}",
                            ws["id"].as_str().unwrap_or(""),
                            ws["name"].as_str().unwrap_or("")
                        );
                    }
                }
            }
            WorkspaceCmd::Use { id } => {
                let resp = c.create_workspace_key(cfg.api_key()?, &id).await?;
                let wk = resp["data"]["key"].as_str().unwrap_or("");
                cfg.workspace_key = Some(wk.to_string());
                cfg.workspace_id = Some(id.clone());
                cfg.save()?;
                println!("Workspace {id} selected. Key saved to config.");
            }
        },
        Commands::Cp { src, dst } => {
            if src == "-" {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                let resp = c.write_file(cfg.ws_key()?, &dst, &buf).await?;
                println!("Written: revision {}", resp["data"]["revision"]);
            } else {
                let src_path = std::path::Path::new(&src);
                if src_path.is_dir() {
                    let n = cp_dir_recursive(&c, cfg.ws_key()?, src_path, &dst).await?;
                    println!("Uploaded {n} file(s) under {dst}");
                } else {
                    let content = std::fs::read_to_string(&src)?;
                    let resp = c.write_file(cfg.ws_key()?, &dst, &content).await?;
                    println!("Written: revision {}", resp["data"]["revision"]);
                }
            }
        }
        Commands::Cat { path, lines } => {
            let content = c.read_file(cfg.ws_key()?, &path, lines.as_deref()).await?;
            print!("{content}");
        }
        Commands::Ls { path } => {
            let resp = c.list_dir(cfg.ws_key()?, &path).await?;
            if let Some(arr) = resp["data"].as_array() {
                for entry in arr {
                    let name = entry["name"].as_str().unwrap_or("");
                    let is_dir = entry["is_dir"].as_bool().unwrap_or(false);
                    if is_dir {
                        println!("{name}/");
                    } else {
                        println!("{name}");
                    }
                }
            }
        }
        Commands::Mv { src, dst } => {
            c.rename_file(cfg.ws_key()?, &src, &dst).await?;
            println!("Moved {src} -> {dst}");
        }
        Commands::Rm { path } => {
            c.delete_file(cfg.ws_key()?, &path).await?;
            println!("Deleted {path}");
        }
        Commands::Append { path, content } => {
            let data = if content == "-" {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                content
            };
            c.append_file(cfg.ws_key()?, &path, &data).await?;
            println!("Appended {} bytes to {path}", data.len());
        }
        Commands::Mkdir { path } => {
            c.mkdir(cfg.ws_key()?, &path).await?;
            println!("Created directory {path}");
        }
        Commands::Search {
            query,
            mode,
            limit,
            detail_level,
        } => {
            let resp = c
                .search(cfg.ws_key()?, &query, &mode, limit, detail_level.as_str())
                .await?;
            if let Some(arr) = resp["data"].as_array() {
                for hit in arr {
                    let path = hit["path"].as_str().unwrap_or("?");
                    let score = hit["score"].as_f64().unwrap_or(0.0);
                    let st = hit["score_type"].as_str().unwrap_or("unknown");
                    let content = hit["content"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect::<String>();
                    print!("{score:.3}({st})\t{path}\t{content}");
                    if let Some(l0) = hit["l0_abstract"].as_str() {
                        print!("\n  L0: {l0}");
                    }
                    if let Some(l1) = hit["l1_overview"].as_str() {
                        let preview: String = l1.chars().take(120).collect();
                        print!("\n  L1: {preview}...");
                    }
                    println!();
                }
            }
        }
        Commands::Grep {
            pattern,
            path,
            ignore_case,
            limit,
        } => {
            let resp = c
                .grep(
                    cfg.ws_key()?,
                    &pattern,
                    path.as_deref(),
                    ignore_case,
                    limit,
                )
                .await?;
            if let Some(arr) = resp["data"].as_array() {
                for hit in arr {
                    let path = hit["path"].as_str().unwrap_or("?");
                    let line_no = hit["line_no"].as_u64().unwrap_or(0);
                    let line = hit["line"].as_str().unwrap_or("");
                    println!("{path}:{line_no}: {line}");
                }
            }
        }
        Commands::Abstract { path } => {
            print_summary_layer(&c, cfg.ws_key()?, &path, "abstract", "L0 Abstract", "l0_abstract")
                .await?;
        }
        Commands::Overview { path } => {
            print_summary_layer(&c, cfg.ws_key()?, &path, "overview", "L1 Overview", "l1_overview")
                .await?;
        }
        Commands::Collection { action } => match action {
            CollectionCmd::Create {
                name,
                schema,
                embed_source,
            } => {
                let schema_val: serde_json::Value = serde_json::from_str(&schema)?;
                let resp = c
                    .create_collection(cfg.ws_key()?, &name, &schema_val, embed_source.as_deref())
                    .await?;
                println!(
                    "Collection created: {}",
                    resp["data"]["id"].as_str().unwrap_or(&name)
                );
            }
            CollectionCmd::List => {
                let resp = c.list_collections(cfg.ws_key()?).await?;
                if let Some(arr) = resp["data"].as_array() {
                    for coll in arr {
                        println!(
                            "{}\t{}",
                            coll["name"].as_str().unwrap_or(""),
                            coll["status"].as_str().unwrap_or("")
                        );
                    }
                }
            }
            CollectionCmd::Desc { name } => {
                let resp = c.describe_collection(cfg.ws_key()?, &name).await?;
                let data = &resp["data"];
                println!("Name:       {}", data["name"].as_str().unwrap_or(""));
                println!("ID:         {}", data["id"].as_str().unwrap_or(""));
                println!(
                    "Type:       {}",
                    data["collection_type"].as_str().unwrap_or("")
                );
                println!("Status:     {}", data["status"].as_str().unwrap_or(""));
                println!(
                    "Embed Src:  {}",
                    data["embedding_source"].as_str().unwrap_or("-")
                );
                println!(
                    "Embed Dim:  {}",
                    data["embedding_dim"]
                        .as_i64()
                        .map(|d| d.to_string())
                        .unwrap_or("-".into())
                );
                if let Some(fields) = data["schema_json"].as_array() {
                    println!("Fields:");
                    for f in fields {
                        let fname = f["name"].as_str().unwrap_or("?");
                        let ftype = f["field_type"]
                            .as_str()
                            .or_else(|| f["type"].as_str())
                            .unwrap_or("?");
                        let idx = if f["index"].as_bool().unwrap_or(false) {
                            " [indexed]"
                        } else {
                            ""
                        };
                        let emb = if f["embed"].as_bool().unwrap_or(false) {
                            " [embed]"
                        } else {
                            ""
                        };
                        println!("  - {fname}: {ftype}{idx}{emb}");
                    }
                }
            }
            CollectionCmd::Delete { name } => {
                c.delete_collection(cfg.ws_key()?, &name).await?;
                println!("Deleted collection {name}");
            }
            CollectionCmd::Insert { name, data } => {
                let rows: serde_json::Value = serde_json::from_str(&data)?;
                c.insert_rows(cfg.ws_key()?, &name, &rows).await?;
                println!("Rows inserted into {name}");
            }
            CollectionCmd::Search { name, query, limit } => {
                let resp = c
                    .search_collection(cfg.ws_key()?, &name, &query, limit)
                    .await?;
                if let Some(arr) = resp["data"].as_array() {
                    for row in arr {
                        println!("{row}");
                    }
                }
            }
        },
        Commands::Sql { query } => {
            let resp = c.execute_sql(cfg.ws_key()?, &query).await?;
            if let Some(arr) = resp["data"].as_array() {
                for row in arr {
                    println!("{row}");
                }
            }
        }
        Commands::Config { action } => match action {
            ConfigCmd::Show => {
                println!("server_url: {}", cfg.server_url);
                println!(
                    "api_key: {}",
                    cfg.api_key
                        .as_deref()
                        .map(mask_secret)
                        .unwrap_or_else(|| "<not set>".into())
                );
                println!(
                    "workspace_id: {}",
                    cfg.workspace_id.as_deref().unwrap_or("<not set>")
                );
                println!(
                    "workspace_key: {}",
                    cfg.workspace_key
                        .as_deref()
                        .map(mask_secret)
                        .unwrap_or_else(|| "<not set>".into())
                );
            }
            ConfigCmd::Set { key, value } => {
                match key.as_str() {
                    "server_url" => cfg.server_url = value,
                    "api_key" => cfg.api_key = Some(value),
                    "workspace_id" => cfg.workspace_id = Some(value),
                    "workspace_key" => cfg.workspace_key = Some(value),
                    _ => anyhow::bail!("unknown config key: {key}"),
                }
                cfg.save()?;
                println!("Config updated.");
            }
        },
    }

    Ok(())
}

/// Recursively upload every file under `src_root` to `dst_root` on the server.
/// Remote path = dst_root + path-relative-to-src_root. Skips empty directories.
/// Returns the number of files uploaded.
async fn cp_dir_recursive(
    client: &client::Client,
    ws_key: &str,
    src_root: &std::path::Path,
    dst_root: &str,
) -> anyhow::Result<usize> {
    let dst_root = dst_root.trim_end_matches('/');
    let mut files = Vec::new();
    collect_files(src_root, &mut files)?;
    let mut count = 0usize;
    for f in &files {
        let rel = f.strip_prefix(src_root)?;
        // POSIX path on the server side regardless of host OS
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let remote = format!("{dst_root}/{rel_str}");
        let content = std::fs::read_to_string(f).map_err(|e| {
            anyhow::anyhow!("read {} failed: {e} (binary files are not supported)", f.display())
        })?;
        client.write_file(ws_key, &remote, &content).await?;
        println!("  {} -> {remote}", f.display());
        count += 1;
    }
    Ok(count)
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // Use file_type() (does NOT follow symlinks) to avoid infinite recursion
        // through directory symlinks, and to prevent silently uploading files
        // outside the intended source root via a symlink escape.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            eprintln!("skip symlink: {}", entry.path().display());
            continue;
        }
        let p = entry.path();
        if ft.is_dir() {
            collect_files(&p, out)?;
        } else if ft.is_file() {
            out.push(p);
        }
    }
    Ok(())
}

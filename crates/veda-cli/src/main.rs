mod client;
mod config;
mod init;
mod status;
mod urlenc;
mod workspace;

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

    /// Use a non-active workspace profile for this command only
    /// (does not change the config). Alias must already exist in the
    /// config — add it first with `veda workspace add <alias>`.
    #[arg(long, global = true)]
    workspace: Option<String>,

    /// Emit machine-readable JSON instead of the human-friendly
    /// default. Currently affects `ls`, `search`, `grep`, `layout`,
    /// `collection search`, and `sql`. Other commands either
    /// already emit JSON (`sql` payload rows) or only print
    /// status messages — they ignore the flag.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Show current config (server URL, key state, workspace) and a
    /// best-effort server reachability ping.
    Status {
        /// Show this workspace's indexing backlog (pending / processing /
        /// dead counts of files not yet searchable).
        #[arg(long)]
        index: bool,
        /// With --index: poll every 5s until the backlog drains to zero.
        /// Exits non-zero if any task is dead (permanently failed).
        #[arg(long, requires = "index")]
        wait: bool,
    },
    /// One-stop auth entry. Five mutually-exclusive modes selected by
    /// flags:
    ///
    /// - **anonymous** (no flags) — server mints account + workspace +
    ///   both keys in a single round-trip; zero prompts.
    /// - **named** (`--email X`) — register a fresh email/password
    ///   account. `--name` defaults to the email's local-part.
    /// - **login** (`--login --email X`) — attach an existing account
    ///   (server returns its existing api_key + a fresh wk_ for the
    ///   default workspace).
    /// - **upgrade** (`--upgrade --email X`) — attach email/password
    ///   to the current anonymous account; api_key keeps working.
    /// - **import-key** (`--import-key vk_…|wk_…`) — paste a key
    ///   copied from another machine. Existing `config.toml` is
    ///   moved aside to `config.toml.bak.<unix-ts>` first so the old
    ///   identity is recoverable. For `vk_` keys we then auto-mint a
    ///   workspace key for the server's `default` workspace so the
    ///   user can immediately run data commands.
    Init {
        /// Login mode: attach an existing account (`--email` + password).
        #[arg(long, conflicts_with_all = ["upgrade", "import_key"])]
        login: bool,
        /// Upgrade mode: turn the current anonymous account into a
        /// named one. Requires an existing `vk_` and a new
        /// `--email` (+ password). The current api_key keeps working.
        #[arg(long, conflicts_with_all = ["login", "import_key"])]
        upgrade: bool,
        /// Import mode: paste a `vk_…` or `wk_…` key copied from
        /// another machine. Existing config.toml is renamed to
        /// `config.toml.bak.<unix-ts>` before writing the new key.
        /// Incompatible with the named / login / upgrade flags.
        #[arg(long, conflicts_with_all = ["login", "upgrade", "email", "password", "name", "workspace_name"])]
        import_key: Option<String>,
        /// Display name (named mode only). Defaults to the email's
        /// local-part when omitted.
        #[arg(long)]
        name: Option<String>,
        /// Email for named / login / upgrade modes. Presence (without
        /// `--login` / `--upgrade`) selects named mode.
        #[arg(long)]
        email: Option<String>,
        /// Pass via env (`VEDA_PASSWORD`) or terminal prompt; `--password`
        /// on argv is visible in `ps`. Required in --non-interactive
        /// named / login / upgrade modes.
        #[arg(long)]
        password: Option<String>,
        /// Server-side workspace name to create or select (default
        /// "default"). Named / login modes only.
        #[arg(long)]
        workspace_name: Option<String>,
        /// Fail with a clear error instead of prompting for missing
        /// fields. Designed for CI / scripts.
        #[arg(long)]
        non_interactive: bool,
    },
    /// Workspace profile management — add / switch / list / rm local
    /// aliases for server-side workspaces. `veda init` already creates
    /// and selects "default"; only needed when juggling multiple
    /// workspaces from one machine. Short alias: `veda ws`.
    #[command(alias = "ws")]
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
        /// Upload everything, ignoring .gitignore / .vedaignore rules.
        /// The built-in skip list (.git, node_modules, .DS_Store, ...)
        /// still applies. Only meaningful when src is a directory.
        #[arg(long)]
        no_ignore: bool,
    },
    /// Read a file's text. PDF/Word documents print their extracted
    /// text; use --raw for the original bytes (or `veda cp` to download).
    Cat {
        /// Remote path
        path: String,
        /// 1-indexed inclusive line range, e.g. `1:20` for lines 1
        /// through 20, or `42:` for line 42 to end-of-file.
        #[arg(long, conflicts_with_all = ["head", "tail", "raw"])]
        range: Option<String>,
        /// Show the first N lines (server-side range, equivalent
        /// to `--range 1:N`).
        #[arg(long, conflicts_with_all = ["range", "tail", "raw"])]
        head: Option<usize>,
        /// Show the last N lines (fetches whole file then slices
        /// locally — there's no server endpoint for tail offsets).
        #[arg(long, conflicts_with_all = ["range", "head", "raw"])]
        tail: Option<usize>,
        /// Output the original bytes verbatim (no text extraction) —
        /// what `cat` did for binaries before extracted-text reads.
        #[arg(long)]
        raw: bool,
    },
    /// List directory
    Ls {
        /// Remote directory path
        #[arg(default_value = "/")]
        path: String,
    },
    /// Move/rename file
    Mv { src: String, dst: String },
    /// Delete files or directories
    Rm {
        /// Remote paths (one or more)
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,
    },
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
        /// Restrict the search to a subtree (e.g. `/docs`). Omit to search the whole workspace.
        #[arg(long)]
        path: Option<String>,
    },
    /// Ask the knowledge base a question — one-shot RAG answer with `[n]`
    /// citations (server-side retrieval; needs the server to have an LLM
    /// configured). May take 10-90 seconds. With --json, prints the raw
    /// response (answer/citations/hit_count) for scripts.
    Ask {
        /// The question (max 1024 chars)
        question: String,
        /// Restrict retrieval to a subtree (e.g. `/wiki`)
        #[arg(long)]
        path: Option<String>,
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
    /// Show how the workspace is organised: its top-level areas, each with
    /// a one-line summary and a file count, plus workspace totals. The
    /// cheapest way to get oriented in a workspace you don't know — one
    /// call instead of `ls` followed by an `abstract` per directory.
    Layout,
    /// Collection management
    Collection {
        #[command(subcommand)]
        action: CollectionCmd,
    },
    /// Execute SQL query
    Sql { query: String },
    /// Configuration management (hidden — `veda init` handles the
    /// common cases; kept for direct edits like the `install.sh`
    /// `config set server_url` step).
    #[command(hide = true)]
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Mint a workspace key and store it under <alias> for future use.
    /// Without --workspace-id, the server creates a fresh workspace
    /// named after the alias. With --workspace-id, mints a key for an
    /// existing workspace (useful when sharing a workspace across
    /// machines).
    Add {
        /// Local alias (used with `--workspace <alias>` and `switch`).
        alias: String,
        /// Existing server workspace id. Omit to create a new workspace.
        #[arg(long)]
        workspace_id: Option<String>,
    },
    /// Set the active workspace profile. Future commands without
    /// `--workspace` use this one.
    Switch {
        /// Alias to switch to (must already exist).
        alias: String,
    },
    /// List configured workspace profiles. Active one is marked with ★.
    List,
    /// Remove a local workspace profile (alias-only, does NOT revoke the
    /// wk_ key on the server — revoke it from the console or
    /// `DELETE /v1/workspaces/{id}/keys/{key_id}`). The active profile
    /// cannot be removed; switch first.
    Rm {
        alias: String,
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

/// Resolve the global `--workspace <alias>` flag against the parsed
/// config + command. Returns the validated alias to use as the
/// in-memory active profile (or `None` if the flag wasn't given).
///
/// - Errors when the flag is combined with `veda workspace` subcommands:
///   those commands take their target alias as a positional arg, and
///   mixing in a global override created a soft-bypass of `workspace
///   rm`'s active-alias guard. Reject up front instead of trying to
///   layer the two semantics.
/// - Errors when the alias isn't present in `[workspaces.…]` so users
///   get an immediate "alias not configured" message instead of a
///   confusing "no workspace selected" from the first data command.
fn resolve_workspace_override(
    cfg: &config::CliConfig,
    flag: Option<&str>,
    command: &Commands,
) -> anyhow::Result<Option<String>> {
    let Some(ws) = flag else {
        return Ok(None);
    };
    if matches!(command, Commands::Workspace { .. }) {
        anyhow::bail!(
            "`--workspace <alias>` cannot be combined with `veda workspace` subcommands; \
             those take the alias directly as a positional arg"
        );
    }
    cfg.workspace_for(ws)
        .map_err(|e| anyhow::anyhow!("--workspace {ws}: {e}"))?;
    Ok(Some(ws.to_string()))
}

/// One-line "<Verb> <path> in workspace '<alias>'" banner. Pure
/// string formatter so tests can assert the wording without poking
/// stdin/stdout.
fn announce_text(verb: &str, path: &str, workspace_alias: &str) -> String {
    let pretty = capitalise_first(verb);
    format!("{pretty} {path} in workspace '{workspace_alias}'")
}

/// Print a one-line "<verb> <path> in workspace '<alias>'" banner so
/// a user (or operator reading agent logs) can spot a command that
/// hit the wrong workspace. Returns `Ok(())` for "go ahead", `Err`
/// for "user said no".
///
/// On a TTY this is interactive with a y/N prompt (default no). On a
/// non-TTY (script, agent, pipe) it never blocks — but it still
/// prints to stderr, so the workspace alias shows up in agent logs
/// for the caller to verify after the fact.
///
/// `confirm` controls whether we wait for an answer on a TTY. Bulk
/// non-destructive writes (`cp`, `mv`, `append`, `mkdir`) pass
/// `confirm=false` and just announce; `rm` passes `confirm=true`.
fn confirm_or_announce(
    workspace_alias: &str,
    verb: &str,
    path: &str,
    confirm: bool,
) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    // TTY check on stdin, not stdout: `veda rm /x > out.log` keeps
    // stdin attached to the terminal but redirects stdout, and the
    // user still wants the confirmation prompt to fire. Looking at
    // stdout instead would silently delete in that case. Prompt is
    // written to stderr so a redirected stdout still sees just the
    // command's normal output.
    let interactive_stdin = std::io::stdin().is_terminal();
    if confirm && interactive_stdin {
        eprint!("Will {verb} {path} in workspace '{workspace_alias}' — confirm? [y/N] ");
        std::io::stderr().flush()?;
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        let t = buf.trim().to_lowercase();
        if t != "y" && t != "yes" {
            anyhow::bail!("aborted");
        }
    } else {
        // Non-interactive (or non-destructive) path: announce but
        // don't block. Goes to stderr for the same reason — keeps
        // stdout clean for pipes.
        eprintln!("{}", announce_text(verb, path, workspace_alias));
    }
    Ok(())
}

fn capitalise_first(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().chain(cs).collect(),
        None => String::new(),
    }
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

    // ── announce_text / capitalise_first ───────────────────────────

    #[test]
    fn capitalise_first_uppercases_only_first_codepoint() {
        assert_eq!(capitalise_first("delete"), "Delete");
        assert_eq!(capitalise_first(""), "");
        assert_eq!(capitalise_first("移动"), "移动"); // CJK has no case → no-op
    }

    #[test]
    fn announce_text_includes_verb_path_and_workspace_alias() {
        // Banner contract: verb capitalised, path + alias inline so
        // grepping agent logs for the alias is trivial.
        let line = announce_text("delete", "/notes/foo.md", "default");
        assert_eq!(line, "Delete /notes/foo.md in workspace 'default'");
    }
}

#[cfg(test)]
mod cli_parse_tests {
    //! Pins clap routing for the two flags that move in this change:
    //! the new global `--workspace <alias>` and the renamed
    //! `init --workspace-name <name>`. A regression that silently
    //! shadowed the global with the local would be invisible
    //! otherwise.
    use super::*;
    use clap::Parser;

    #[test]
    fn global_workspace_flag_carries_alias_through_to_cli() {
        let cli = Cli::try_parse_from([
            "veda",
            "--workspace",
            "archive",
            "ls",
            "/docs",
        ])
        .unwrap();
        assert_eq!(cli.workspace.as_deref(), Some("archive"));
        match cli.command {
            Commands::Ls { path } => assert_eq!(path, "/docs"),
            _ => panic!("expected Ls subcommand"),
        }
    }

    #[test]
    fn init_subcommand_accepts_workspace_name_not_workspace() {
        // The legacy `--workspace` on Init was renamed to
        // `--workspace-name` so the global flag (profile alias)
        // doesn't clash. A regression that re-added a local
        // `--workspace` would either fail to parse this command line
        // (because `--workspace` is now eaten by the global before
        // Init sees it) or attach the value to the wrong field.
        let cli = Cli::try_parse_from([
            "veda",
            "init",
            "--workspace-name",
            "scratch",
            "--non-interactive",
        ])
        .unwrap();
        match cli.command {
            Commands::Init { workspace_name, .. } => {
                assert_eq!(workspace_name.as_deref(), Some("scratch"));
            }
            _ => panic!("expected Init subcommand"),
        }
    }

    #[test]
    fn workspace_subcommand_is_unhidden_in_help() {
        // `Workspace` must appear in the top-level help (no `hide=true`).
        // Easiest check is to ask clap to render help and look for the
        // subcommand name in the output.
        use clap::CommandFactory;
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("workspace"), "help: {help}");
        // Account / Config should stay hidden (escape hatches only).
        assert!(!help.contains("\n  account"), "account leaked: {help}");
        assert!(!help.contains("\n  config"), "config leaked: {help}");
    }

    #[test]
    fn workspace_override_rejected_when_combined_with_workspace_subcmd() {
        // Critical regression from Codex review: `--workspace ghost
        // workspace rm default` used to skip validation but still
        // set ghost as active in memory, which let `rm` think
        // `default` wasn't active and delete it (then save a dangling
        // active pointer). This test pins the up-front rejection.
        let cfg = config::CliConfig::default();
        let cmd = Commands::Workspace {
            action: WorkspaceCmd::List,
        };
        let err = resolve_workspace_override(&cfg, Some("ghost"), &cmd).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot be combined"), "msg: {msg}");
    }

    #[test]
    fn workspace_override_rejected_when_alias_missing() {
        let cfg = config::CliConfig::default();
        let cmd = Commands::Ls { path: "/".into() };
        let err = resolve_workspace_override(&cfg, Some("ghost"), &cmd).unwrap_err();
        // Error path must namespace under the flag name so users see
        // exactly which CLI arg was wrong.
        assert!(err.to_string().contains("--workspace ghost"), "msg: {err}");
    }

    #[test]
    fn workspace_override_passes_when_alias_exists_and_not_workspace_subcmd() {
        let mut cfg = config::CliConfig::default();
        cfg.set_active_profile(
            "default",
            config::WorkspaceEntry {
                id: Some("ws-1".into()),
                key: "wk-1".into(),
            },
        );
        cfg.workspaces.insert(
            "archive".into(),
            config::WorkspaceEntry {
                id: Some("ws-2".into()),
                key: "wk-2".into(),
            },
        );
        let cmd = Commands::Ls { path: "/".into() };
        let out = resolve_workspace_override(&cfg, Some("archive"), &cmd).unwrap();
        assert_eq!(out.as_deref(), Some("archive"));
    }

    #[test]
    fn workspace_override_returns_none_when_flag_absent() {
        let cfg = config::CliConfig::default();
        let cmd = Commands::Status { index: false, wait: false };
        let out = resolve_workspace_override(&cfg, None, &cmd).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn workspace_add_parses_alias_and_optional_id() {
        let cli = Cli::try_parse_from([
            "veda",
            "workspace",
            "add",
            "scratch",
            "--workspace-id",
            "ws-uuid-1",
        ])
        .unwrap();
        match cli.command {
            Commands::Workspace {
                action: WorkspaceCmd::Add { alias, workspace_id },
            } => {
                assert_eq!(alias, "scratch");
                assert_eq!(workspace_id.as_deref(), Some("ws-uuid-1"));
            }
            _ => panic!("expected workspace add"),
        }
    }

    // ── init mode exclusivity (clap conflicts_with_all) ────────────
    //
    // The new `veda init` collapses what used to be `login` / `claim` /
    // `login --api-key` into a single subcommand with mutually
    // exclusive mode flags. clap enforces the exclusion at parse time
    // so the impl never has to second-guess what mode it's in. These
    // tests pin the contract: any combination that would put us into
    // two modes at once must fail before main() is entered.

    /// Local helper: clap's error type is Debug but Cli isn't, so the
    /// blanket `Result::unwrap_err` bound trips. Pull the error out
    /// manually instead.
    fn expect_clap_err(argv: &[&str]) -> clap::Error {
        match Cli::try_parse_from(argv) {
            Ok(_) => panic!("expected clap to reject {argv:?}, but it parsed"),
            Err(e) => e,
        }
    }

    #[test]
    fn init_login_and_upgrade_are_mutually_exclusive() {
        let err = expect_clap_err(&[
            "veda",
            "init",
            "--login",
            "--upgrade",
            "--email",
            "x@y.com",
        ]);
        let msg = err.to_string();
        assert!(
            msg.contains("--login") && msg.contains("--upgrade"),
            "expected mutual-exclusion error, got: {msg}"
        );
    }

    #[test]
    fn init_upgrade_and_import_key_are_mutually_exclusive() {
        // Symmetry with login+upgrade: codex review flagged this pair
        // as missing from the test matrix. Pin it so a future
        // conflicts_with_all rewrite can't silently lose the exclusion.
        let err = expect_clap_err(&[
            "veda",
            "init",
            "--upgrade",
            "--import-key",
            "vk_abc",
            "--email",
            "x@y.com",
        ]);
        let msg = err.to_string();
        assert!(
            (msg.contains("--upgrade") && msg.contains("--import-key"))
                || msg.contains("cannot be used"),
            "expected upgrade+import-key exclusion error, got: {msg}"
        );
    }

    #[test]
    fn init_import_key_excludes_login() {
        let err = expect_clap_err(&[
            "veda",
            "init",
            "--import-key",
            "vk_abc",
            "--login",
        ]);
        let msg = err.to_string();
        assert!(msg.contains("--import-key") || msg.contains("--login"),
            "expected exclusion error, got: {msg}");
    }

    #[test]
    fn init_import_key_excludes_email_and_password() {
        // --import-key is a pure paste-the-key flow; combining it with
        // --email / --password / --name would suggest the user wants
        // *both* to swap identity AND to register/login at the same
        // time. clap must reject up front to keep the dispatch in
        // run_init_command unambiguous (it only consults mode flags
        // in fixed priority).
        let err = expect_clap_err(&[
            "veda",
            "init",
            "--import-key",
            "vk_abc",
            "--email",
            "x@y.com",
        ]);
        let msg = err.to_string();
        assert!(msg.contains("--import-key") || msg.contains("--email"),
            "expected exclusion error, got: {msg}");
    }

    #[test]
    fn init_anonymous_parses_with_no_flags() {
        let cli = Cli::try_parse_from(["veda", "init"]).unwrap();
        match cli.command {
            Commands::Init {
                login,
                upgrade,
                import_key,
                email,
                ..
            } => {
                assert!(!login);
                assert!(!upgrade);
                assert!(import_key.is_none());
                assert!(email.is_none());
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn init_import_key_carries_value_through() {
        let cli = Cli::try_parse_from([
            "veda",
            "init",
            "--import-key",
            "vk_pasted",
        ])
        .unwrap();
        match cli.command {
            Commands::Init { import_key, .. } => {
                assert_eq!(import_key.as_deref(), Some("vk_pasted"));
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn init_upgrade_with_email_parses() {
        let cli = Cli::try_parse_from([
            "veda",
            "init",
            "--upgrade",
            "--email",
            "j@x.com",
            "--non-interactive",
            "--password",
            "p",
        ])
        .unwrap();
        match cli.command {
            Commands::Init { upgrade, email, password, .. } => {
                assert!(upgrade);
                assert_eq!(email.as_deref(), Some("j@x.com"));
                assert_eq!(password.as_deref(), Some("p"));
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn removed_login_subcommand_is_gone() {
        // `veda login --api-key …` is no longer a thing — its
        // semantics moved under `veda init --import-key`. A regression
        // that resurrected the top-level subcommand would parse this
        // line; the new world fails it at clap.
        let err = expect_clap_err(&["veda", "login", "--api-key", "vk_abc"]);
        let msg = err.to_string();
        assert!(
            msg.contains("unrecognized subcommand")
                || msg.contains("unexpected argument")
                || msg.contains("invalid subcommand"),
            "expected clap to reject 'veda login', got: {msg}"
        );
    }

    #[test]
    fn removed_claim_subcommand_is_gone() {
        // Same expectation for `veda claim`: replaced by
        // `veda init --upgrade --email …`.
        let err = expect_clap_err(&["veda", "claim", "--email", "j@x.com"]);
        let msg = err.to_string();
        assert!(
            msg.contains("unrecognized subcommand")
                || msg.contains("unexpected argument")
                || msg.contains("invalid subcommand"),
            "expected clap to reject 'veda claim', got: {msg}"
        );
    }

    // ── PR3a: --json / cat slice flags / ws alias ──────────────────

    #[test]
    fn global_json_flag_routes_through_to_cli() {
        let cli = Cli::try_parse_from(["veda", "--json", "ls", "/"]).unwrap();
        assert!(cli.json);
        match cli.command {
            Commands::Ls { path } => assert_eq!(path, "/"),
            _ => panic!("expected Ls"),
        }
    }

    #[test]
    fn global_json_flag_default_false() {
        let cli = Cli::try_parse_from(["veda", "ls", "/"]).unwrap();
        assert!(!cli.json);
    }

    #[test]
    fn cat_range_head_tail_are_mutually_exclusive() {
        let err = expect_clap_err(&["veda", "cat", "/x", "--range", "1:5", "--head", "10"]);
        let msg = err.to_string();
        assert!(
            msg.contains("--range") || msg.contains("--head") || msg.contains("cannot be used"),
            "expected exclusion error, got: {msg}"
        );
        let err = expect_clap_err(&["veda", "cat", "/x", "--head", "10", "--tail", "5"]);
        assert!(
            err.to_string().contains("--head")
                || err.to_string().contains("--tail")
                || err.to_string().contains("cannot be used"),
            "got: {err}"
        );
    }

    #[test]
    fn cat_parses_each_slice_flag_individually() {
        let cli = Cli::try_parse_from(["veda", "cat", "/x", "--range", "1:20"]).unwrap();
        match cli.command {
            Commands::Cat { range, head, tail, .. } => {
                assert_eq!(range.as_deref(), Some("1:20"));
                assert!(head.is_none());
                assert!(tail.is_none());
            }
            _ => panic!("expected Cat"),
        }
        let cli = Cli::try_parse_from(["veda", "cat", "/x", "--head", "10"]).unwrap();
        match cli.command {
            Commands::Cat { head, .. } => assert_eq!(head, Some(10)),
            _ => panic!("expected Cat"),
        }
        let cli = Cli::try_parse_from(["veda", "cat", "/x", "--tail", "3"]).unwrap();
        match cli.command {
            Commands::Cat { tail, .. } => assert_eq!(tail, Some(3)),
            _ => panic!("expected Cat"),
        }
    }

    #[test]
    fn removed_cat_lines_flag_is_gone() {
        // `--lines` was renamed to `--range` (codex review of plan
        // flagged the name as ambiguous with "limit line count").
        // A regression that re-added it should fail at parse.
        let err = expect_clap_err(&["veda", "cat", "/x", "--lines", "1:5"]);
        let msg = err.to_string();
        assert!(
            msg.contains("--lines") || msg.contains("unexpected"),
            "expected clap to reject --lines, got: {msg}"
        );
    }

    #[test]
    fn workspace_ws_alias_works() {
        // The `ws` alias is a typing-saver for `veda workspace …`.
        // Pins that `veda ws list` parses to the same `Workspace`
        // subcommand as the long form.
        let cli = Cli::try_parse_from(["veda", "ws", "list"]).unwrap();
        match cli.command {
            Commands::Workspace { action: WorkspaceCmd::List } => {}
            _ => panic!("expected Workspace::List via ws alias"),
        }
        let cli =
            Cli::try_parse_from(["veda", "ws", "add", "scratch"]).unwrap();
        match cli.command {
            Commands::Workspace { action: WorkspaceCmd::Add { alias, .. } } => {
                assert_eq!(alias, "scratch");
            }
            _ => panic!("expected Workspace::Add via ws alias"),
        }
    }
}

/// Byte counts for humans. Binary units, one decimal below 10 so `9.7 MB`
/// stays readable while `512 KB` doesn't get a pointless `.0`.
///
/// The unit is chosen against the *rounded* value, not the raw one:
/// picking it first lets 1048575 print as `1024 KB` instead of `1.0 MB`,
/// because rounding happens after the unit is already locked in.
fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    // Byte counts are COUNT/SUM results and cannot be negative; a negative
    // here means a broken response, and 0 is a less confusing answer than
    // `-9223372036854775808 B`.
    let n = n.max(0);
    let mut v = n as f64;
    let mut u = 0;
    while u < UNITS.len() - 1 && round_at_precision(v) >= 1024.0 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        return format!("{n} B");
    }
    // Branch on the rounded value too, or 9.999 prints as "10.0 KB" —
    // a decimal the >= 10 rule says it shouldn't have.
    if round_at_precision(v) < 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.0} {}", UNITS[u])
    }
}

/// Cells an abstract is indented by under its entry's header line.
const LAYOUT_INDENT: usize = 4;

/// Make an LLM-written summary safe to print.
///
/// A newline in an abstract forges what looks like a whole extra entry —
/// the summariser is a language model, so a stray line break is a normal
/// failure, not a hostile one. Control characters (ESC in particular) also
/// get folded to spaces so a response can't drive the terminal.
///
/// Length is *not* capped: an L0 runs 200-500 characters and the point of
/// this listing is to read them. Fitting them on screen is the wrapper's
/// job, not a truncator's.
fn clean_abstract(s: &str) -> String {
    s.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Greedily wrap `text` to `limit` terminal cells.
///
/// Breaks are measured in display width, not chars — the abstracts are
/// full of CJK, where one char is two cells. Break opportunities sit
/// before a word (so English never splits mid-word) and on either side of
/// a wide char (so a Chinese sentence, which contains no spaces at all,
/// still breaks anywhere). A single word longer than `limit` is cut by
/// character, because the alternative is overflowing the terminal.
fn wrap_display(text: &str, limit: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    let limit = limit.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut buf_w = 0usize;
    // Byte offset in `buf` of the latest place we are allowed to break.
    let mut brk: Option<usize> = None;
    let mut prev: Option<char> = None;

    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if let Some(p) = prev {
            let wide = |c: char| c.width().unwrap_or(0) > 1;
            if ch != ' ' && !buf.is_empty() && (p == ' ' || wide(p) || wide(ch)) {
                brk = Some(buf.len());
            }
        }
        if buf_w + cw > limit && !buf.is_empty() {
            match brk.filter(|b| *b > 0) {
                // Break at the last word / wide-char boundary and carry
                // everything after it down to the next line.
                Some(b) => {
                    let rest = buf.split_off(b);
                    lines.push(buf.trim_end().to_string());
                    buf = rest;
                }
                // One unbreakable run wider than the line: hard-cut here.
                None => {
                    lines.push(buf.trim_end().to_string());
                    buf = String::new();
                }
            }
            buf_w = buf.width();
            brk = None;
        }
        prev = Some(ch);
        // A space that lands at a line break is consumed by it rather than
        // indenting the next line.
        if buf.is_empty() && ch == ' ' {
            continue;
        }
        buf.push(ch);
        buf_w += cw;
    }
    let tail = buf.trim_end();
    if !tail.is_empty() {
        lines.push(tail.to_string());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// What `v` becomes once printed: one decimal below 10, whole above.
fn round_at_precision(v: f64) -> f64 {
    if v < 10.0 {
        (v * 10.0).round() / 10.0
    } else {
        v.round()
    }
}

/// Render `GET /v1/layout` for a terminal.
///
/// Wraps to the terminal width when stdout is a TTY. Down a pipe there is
/// no width to respect and wrapping would only break `grep`, so each
/// abstract goes out as one long line instead.
fn print_layout(data: &serde_json::Value) {
    use std::io::IsTerminal;

    let wrap = std::io::stdout().is_terminal().then(|| {
        terminal_size::terminal_size()
            .map(|(terminal_size::Width(w), _)| usize::from(w))
            .unwrap_or(80)
            // Below this the indent eats the text; wrap as if 40 and let
            // the terminal do whatever it does.
            .max(40)
    });
    print!("{}", render_layout(data, wrap));
}

/// Split out from `print_layout` so the layout is testable — wrapping and
/// the control-character defences are invisible unless you can assert on
/// the rendered string.
///
/// One block per entry: a `name  meta` header, then the full abstract
/// indented beneath it. The abstract is never truncated; `wrap_width` is
/// `None` when nothing is watching (a pipe) and the whole abstract goes on
/// one line.
fn render_layout(data: &serde_json::Value, wrap_width: Option<usize>) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let entries = data["entries"].as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        return "(empty workspace — nothing uploaded yet)\n".to_string();
    }

    let rows: Vec<(String, String, Option<String>)> = entries
        .iter()
        .map(|e| {
            let is_dir = e["is_dir"].as_bool().unwrap_or(false);
            let path = e["path"].as_str().unwrap_or("?");
            let name = path.strip_prefix('/').unwrap_or(path);
            let name = if is_dir { format!("{name}/") } else { name.to_string() };
            let meta = if is_dir {
                // A negative count is a broken response, not "minus one
                // file" — drop it rather than render `-1 files`.
                match e["file_count"].as_i64().filter(|n| *n >= 0) {
                    Some(1) => "1 file".to_string(),
                    Some(n) => format!("{n} files"),
                    None => String::new(),
                }
            } else {
                e["size_bytes"]
                    .as_i64()
                    .filter(|n| *n >= 0)
                    .map(human_bytes)
                    .unwrap_or_default()
            };
            // An abstract that is nothing but control characters cleans up
            // to "" — that is no abstract, not an empty indented line.
            let abs = e["abstract"]
                .as_str()
                .map(clean_abstract)
                .filter(|a| !a.is_empty());
            (name, meta, abs)
        })
        .collect();

    let indent = " ".repeat(LAYOUT_INDENT);
    let mut prev_was_block = false;
    for (name, meta, abs) in &rows {
        // Blank line whenever either neighbour is a multi-line block, so
        // the indented text always reads as belonging to the header above
        // it. A run of bare headers stays packed like `ls`.
        if (prev_was_block || abs.is_some()) && !out.is_empty() {
            out.push('\n');
        }
        // An entry with neither a count nor an abstract would otherwise
        // ship a line of trailing spaces.
        let _ = writeln!(out, "{}", format!("{name}  {meta}").trim_end());
        if let Some(a) = abs {
            match wrap_width {
                Some(w) => {
                    for line in wrap_display(a, w.saturating_sub(LAYOUT_INDENT)) {
                        let _ = writeln!(out, "{indent}{line}");
                    }
                }
                None => {
                    let _ = writeln!(out, "{indent}{a}");
                }
            }
        }
        prev_was_block = abs.is_some();
    }

    let stats = &data["stats"];
    let _ = writeln!(
        out,
        "\n{} files, {} directories, {}",
        stats["total_files"].as_i64().unwrap_or(0),
        stats["total_directories"].as_i64().unwrap_or(0),
        human_bytes(stats["total_bytes"].as_i64().unwrap_or(0))
    );
    // Only say something when the answer is incomplete — a fully-summarised
    // workspace needs no footnote.
    if data["truncated"].as_bool().unwrap_or(false) {
        let _ = writeln!(
            out,
            "(more top-level entries exist than shown — use `veda ls /` for the full list)"
        );
    }
    match data["summary_state"].as_str() {
        Some("partial") => {
            let _ = writeln!(
                out,
                "(some summaries are still being generated, or never will be for empty dirs)"
            );
        }
        Some("disabled") => {
            let _ = writeln!(out, "(summaries are disabled on this server — no LLM configured)");
        }
        _ => {}
    }
    out
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

/// Dispatch the `veda init` subcommand across its five modes. Split
/// out of `main()` so the heavy branching (and its prompts) doesn't
/// dwarf the rest of the match. clap's `conflicts_with_all` already
/// rejects illegal mode combinations before we get here.
#[allow(clippy::too_many_arguments)]
async fn run_init_command(
    mut cfg: config::CliConfig,
    server_flag_set: bool,
    login: bool,
    upgrade: bool,
    import_key: Option<String>,
    name: Option<String>,
    email: Option<String>,
    password: Option<String>,
    workspace_name: Option<String>,
    non_interactive: bool,
) -> anyhow::Result<()> {
    // ── mode 1: --import-key ────────────────────────────────────────
    if let Some(key) = import_key {
        // Backup the existing file before clobbering it. Use the
        // canonical path (not whatever's in memory) so the safety
        // net is the same regardless of how cfg was loaded.
        let cfg_path = config::CliConfig::default_path()?;
        let bak = init::backup_config(&cfg_path)?;
        let server_url = cfg.server_url.clone();
        let kind = init::apply_import_key(&mut cfg, key, server_url)?;
        // For account keys (vk_), mint a default workspace key so
        // data commands work right after import — saves the user a
        // separate `veda workspace add default` step.
        //
        // A vk_ minted on another machine usually has a default
        // workspace already on the server. POST /v1/workspaces on a
        // duplicate name surfaces from the store as a 500 (Storage
        // class), not 409, so a naive `run_workspace_add(_, None)`
        // would crash for the very flow this branch was built for.
        // Find-or-create up front: list workspaces, look for the
        // alias by name, pass Some(id) to short-circuit the
        // server-side create (run_workspace_add path 2 mints a key
        // against an existing id; path 1 creates).
        //
        // cfg.save() runs only on full success — any failure before
        // the wk_ is minted leaves the file alone (it's already
        // moved aside into the .bak), so a retry of the same
        // `veda init --import-key` cleanly redoes the flow.
        if matches!(kind, init::ImportedKeyKind::Account) {
            let new_client = client::Client::new(&cfg.server_url);
            let alias = config::DEFAULT_WORKSPACE_ALIAS.to_string();
            let api_key = cfg.api_key.clone().unwrap_or_default();
            let existing_id =
                init::find_workspace_id_by_name(&new_client, &api_key, &alias).await?;
            workspace::run_workspace_add(&new_client, &mut cfg, alias, existing_id).await?;
        }
        // For workspace keys (wk_), resolve which workspace the key
        // belongs to so status shows a real id. Best-effort — a server
        // without /v1/whoami leaves id unset and status backfills later.
        if matches!(kind, init::ImportedKeyKind::Workspace) {
            let new_client = client::Client::new(&cfg.server_url);
            init::backfill_active_workspace_id(&new_client, &mut cfg).await;
        }
        cfg.save()?;
        println!();
        if let Some(p) = bak {
            println!("✓ previous config backed up to {}", p.display());
        }
        match kind {
            init::ImportedKeyKind::Account => {
                println!("✓ account key imported; default workspace key minted");
            }
            init::ImportedKeyKind::Workspace => {
                println!("✓ workspace key imported");
            }
        }
        println!("Try: veda status");
        return Ok(());
    }

    // ── mode 2: --upgrade (attach email/password to current anon) ──
    if upgrade {
        // Fall back to VEDA_API_KEY when the config has no key —
        // supports the "I pasted my anon vk_ into the shell but didn't
        // persist it yet" flow. Cfg wins when both are set so a stale
        // env var can't silently hijack the upgrade to a wrong account.
        if cfg.api_key.as_deref().map_or(true, str::is_empty) {
            if let Ok(env_key) = std::env::var("VEDA_API_KEY") {
                if !env_key.is_empty() {
                    cfg.api_key = Some(env_key);
                }
            }
        }
        let email = resolve_field("Email", email, None, non_interactive, false)?;
        let password = resolve_password(password, non_interactive)?;
        let new_client = client::Client::new(&cfg.server_url);
        let account_id = init::run_claim(&new_client, &cfg, email, password, name).await?;
        println!("✓ account upgraded (id {account_id})");
        println!("Your API key is unchanged; future logins use email + password.");
        return Ok(());
    }

    // ── mode 3 / 4 / 5: anonymous, named, login ────────────────────
    //
    // The global `--server` flag was already merged into cfg at the
    // top of main, so cfg.server_url is the source of truth.
    // Anonymous mode is genuinely zero-prompt — confirming the
    // server URL would defeat the "0-input" pitch and breaks
    // non-tty contexts (curl | sh, CI). Only prompt in named
    // mode when the user has signaled they want interaction.
    let _ = server_flag_set; // retained for symmetry; cfg already merged
    let is_anonymous = email.is_none() && !login && name.is_none() && workspace_name.is_none();
    let server_url = if non_interactive || is_anonymous {
        cfg.server_url.clone()
    } else {
        prompt_or("Server URL", Some(&cfg.server_url))?
    };

    if is_anonymous {
        // Refuse to overwrite an existing identity. Running `veda
        // init` again after onboarding would silently throw away the
        // previous account binding, which is hard to recover from.
        // Use `--import-key` to swap identities (with backup) or
        // delete the config file first.
        if cfg.api_key.is_some() || !cfg.workspaces.is_empty() {
            anyhow::bail!(
                "this machine is already onboarded (see `veda status`). \
                 To swap identities use `veda init --import-key <key>` \
                 (creates a config backup), or delete the config file first."
            );
        }
        let new_client = client::Client::new(&server_url);
        let outcome_result = init::run_anonymous(&new_client, &mut cfg, server_url).await;
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
             machine: veda init --upgrade --email you@example.com"
        );
        return Ok(());
    }

    let email = resolve_field("Email", email, None, non_interactive, false)?;
    // In non-interactive named mode the user often only has email +
    // password (e.g. CI / agent). Derive name from the email's
    // local-part so they don't have to repeat themselves; the
    // server-side 409 fallback to login means the name only matters
    // when actually creating a new account.
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
    let workspace = workspace_name
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
    Ok(())
}

/// `veda status --index [--wait]` — indexing backlog for the active
/// workspace. With --wait, polls every 5s until pending+processing hit
/// zero; exits non-zero when dead > 0 so CI can gate on "uploaded AND
/// searchable AND nothing failed".
async fn run_index_status(
    c: &client::Client,
    cfg: &config::CliConfig,
    wait: bool,
) -> anyhow::Result<()> {
    loop {
        let st = c.index_status(cfg.active_wk()?).await?;
        let pending = st["data"]["pending"].as_i64().unwrap_or(0);
        let processing = st["data"]["processing"].as_i64().unwrap_or(0);
        let dead = st["data"]["dead"].as_i64().unwrap_or(0);
        println!("indexing: {pending} pending, {processing} processing, {dead} dead");
        if dead > 0 {
            anyhow::bail!(
                "{dead} file(s) permanently failed to index — ask an operator to inspect the outbox dead letters"
            );
        }
        if !wait || pending + processing == 0 {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut cfg = config::CliConfig::load()?;

    if let Some(ref s) = cli.server {
        cfg.server_url = s.clone();
    }
    if let Some(ws) = resolve_workspace_override(&cfg, cli.workspace.as_deref(), &cli.command)? {
        cfg.active_workspace = Some(ws);
    }

    let c = client::Client::new(&cfg.server_url);
    // Capture before the match consumes `cli.command`. Most handlers
    // ignore it; ls/search/grep/collection-search/sql flip output
    // formats on it.
    let json_output = cli.json;

    match cli.command {
        Commands::Status { index, wait } => {
            if index {
                run_index_status(&c, &cfg, wait).await?;
                return Ok(());
            }
            // Skip the ping when nothing is configured — there's no
            // server to talk to that the user opted into.
            let reachable =
                if cfg.api_key.is_some() || !cfg.workspaces.is_empty() || cfg.env_key.is_some() {
                    Some(status::ping_server(&cfg.server_url).await)
                } else {
                    None
                };
            // A pasted-wk_ profile starts with no workspace id; resolve
            // it once via /v1/whoami and persist so status shows a real
            // id instead of "(id unknown)". Save failure is tolerable —
            // the id still renders this run and backfills again next time.
            // Skipped in env-key mode: nothing configured to backfill.
            if reachable == Some(true)
                && cfg.env_key.is_none()
                && init::backfill_active_workspace_id(&c, &mut cfg).await
            {
                let _ = cfg.save();
            }
            print!("{}", status::render_status(&cfg, reachable));
        }
        Commands::Init {
            login,
            upgrade,
            import_key,
            name,
            email,
            password,
            workspace_name,
            non_interactive,
        } => {
            run_init_command(
                cfg,
                cli.server.is_some(),
                login,
                upgrade,
                import_key,
                name,
                email,
                password,
                workspace_name,
                non_interactive,
            )
            .await?;
            return Ok(());
        }
        Commands::Workspace { action } => match action {
            WorkspaceCmd::Add { alias, workspace_id } => {
                // Save unconditionally — `run_workspace_add` may
                // mutate cfg in the create-then-mint path (path 1,
                // empty-key placeholder) before a mint failure
                // returns Err. Persisting that placeholder is what
                // lets the user retry with `veda workspace add
                // <alias>` and hit the repair branch instead of
                // leaving the server-side workspace as an orphan.
                let result = workspace::run_workspace_add(&c, &mut cfg, alias, workspace_id).await;
                cfg.save()?;
                let out = result?;
                let mut extras = Vec::new();
                if out.repaired {
                    extras.push("repaired existing alias");
                }
                if out.auto_switched {
                    extras.push("switched to it");
                }
                let suffix = if extras.is_empty() {
                    String::new()
                } else {
                    format!("; {}", extras.join("; "))
                };
                println!(
                    "added workspace '{}' (id {}){suffix}",
                    out.alias, out.workspace_id
                );
            }
            WorkspaceCmd::Switch { alias } => {
                let prev = workspace::run_workspace_switch(&mut cfg, alias.clone())?;
                cfg.save()?;
                println!("switched: {prev} → {alias}");
            }
            WorkspaceCmd::List => {
                if cfg.workspaces.is_empty() {
                    println!("(no workspace profiles configured — run `veda init`)");
                } else {
                    let active = cfg.active_alias().unwrap_or("").to_string();
                    let mut active_seen = false;
                    for (alias, entry) in &cfg.workspaces {
                        let marker = if alias == &active {
                            active_seen = true;
                            "★"
                        } else {
                            " "
                        };
                        let id = entry.id.as_deref().unwrap_or("?");
                        let key_warn = if entry.key.is_empty() {
                            "  ⚠ key missing"
                        } else {
                            ""
                        };
                        println!("{marker} {alias}\t{id}{key_warn}");
                    }
                    // Dangling active_workspace: the value points at a
                    // profile that no longer exists. status renders a
                    // similar nudge — print the same here so users
                    // running `workspace list` to diagnose see it.
                    if !active.is_empty() && !active_seen {
                        println!(
                            "⚠ active_workspace='{active}' is not in the list above; \
                             run `veda workspace switch <alias>` to fix"
                        );
                    }
                }
            }
            WorkspaceCmd::Rm { alias } => {
                workspace::run_workspace_rm(&mut cfg, &alias)?;
                cfg.save()?;
                println!(
                    "removed workspace profile '{alias}' \
                     (local alias only; server-side wk_ not revoked — \
                     revoke it from the console if needed)"
                );
            }
        },
        Commands::Cp {
            src,
            dst,
            no_ignore,
        } => {
            // Non-destructive announcement: cp writes a new revision,
            // so a wrong workspace is recoverable. Skip the blocking
            // prompt but still print the workspace alias.
            let active = cfg.active_alias().unwrap_or("?").to_string();
            confirm_or_announce(&active, "copy to", &dst, false)?;
            if src == "-" {
                use std::io::Read;
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;
                let resp = c.write_file(cfg.active_wk()?, &dst, buf).await?;
                println!("Written: revision {}", resp["data"]["revision"]);
            } else {
                let src_path = std::path::Path::new(&src);
                if src_path.is_dir() {
                    let stats =
                        cp_dir_recursive(&c, cfg.active_wk()?, src_path, &dst, no_ignore).await?;
                    if stats.failed > 0 {
                        // Rerunning is cheap: already-uploaded files dedup
                        // server-side via If-None-Match, so only the failed
                        // ones actually re-upload.
                        anyhow::bail!(
                            "uploaded {} file(s), {} FAILED (listed above) — \
                             fix and rerun the same command to retry just the failures",
                            stats.uploaded,
                            stats.failed
                        );
                    }
                    println!("Uploaded {} file(s) under {dst}", stats.uploaded);
                    // Batch uploads index asynchronously — tell the user how
                    // to know when everything is searchable. Best-effort:
                    // pre-index-status servers 404 here, stay silent then.
                    if let Ok(st) = c.index_status(cfg.active_wk()?).await {
                        let queued = st["data"]["pending"].as_i64().unwrap_or(0)
                            + st["data"]["processing"].as_i64().unwrap_or(0);
                        if queued > 0 {
                            println!(
                                "{queued} file(s) queued for indexing — check: veda status --index [--wait]"
                            );
                        }
                    }
                } else {
                    let content = read_file_bytes(&src)?;
                    let resp = c.write_file(cfg.active_wk()?, &dst, content).await?;
                    println!("Written: revision {}", resp["data"]["revision"]);
                }
            }
        }
        Commands::Cat { path, range, head, tail, raw } => {
            // clap's conflicts_with_all already rejects > 1 of these;
            // here we just translate to the server-side `lines`
            // parameter shape (1-indexed inclusive A:B, with B empty
            // = to EOF). `--tail` is the only one we can't express
            // server-side, so it goes through the slice-after-fetch
            // path.
            if let Some(n) = tail {
                let content = c.read_file_text(cfg.active_wk()?, &path).await?;
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(n);
                // Print in original order, preserving a trailing
                // newline only when the source had one.
                let trailing = if content.ends_with('\n') { "\n" } else { "" };
                print!("{}{trailing}", lines[start..].join("\n"));
            } else if raw {
                // Original bytes verbatim: binary (pdf/image/jar)
                // round-trips losslessly when redirected to a file.
                let bytes = c.read_file(cfg.active_wk()?, &path, None).await?;
                use std::io::Write;
                std::io::stdout().write_all(&bytes)?;
            } else if let Some(line_spec) = match (range.as_deref(), head) {
                (Some(r), _) => Some(r.to_string()),
                (None, Some(n)) => Some(format!("1:{n}")),
                (None, None) => None,
            } {
                let bytes = c
                    .read_file(cfg.active_wk()?, &path, Some(&line_spec))
                    .await?;
                // Line slicing implies text; the server already rejects
                // line reads on a binary blob, but guard the local decode.
                let content = String::from_utf8(bytes).map_err(|_| {
                    anyhow::anyhow!("'{path}' is binary; --range/--head need text")
                })?;
                print!("{content}");
            } else {
                // Whole-file read defaults to the text view: plain text
                // comes back as-is, and extractable binaries (pdf/word)
                // return their server-side extracted text.
                let content = c.read_file_text(cfg.active_wk()?, &path).await?;
                print!("{content}");
            }
        }
        Commands::Ls { path } => {
            let resp = c.list_dir(cfg.active_wk()?, &path).await?;
            if json_output {
                if let Some(arr) = resp["data"].as_array() {
                    for entry in arr {
                        println!("{entry}");
                    }
                }
            } else if let Some(arr) = resp["data"].as_array() {
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
            let active = cfg.active_alias().unwrap_or("?").to_string();
            confirm_or_announce(
                &active,
                "move into",
                &format!("{src} → {dst}"),
                false,
            )?;
            c.rename_file(cfg.active_wk()?, &src, &dst).await?;
            println!("Moved {src} -> {dst}");
        }
        Commands::Rm { paths } => {
            // rm is the only data-plane command that's irreversible
            // against the wrong workspace, so this is the one we ask
            // for explicit y/N confirmation on a TTY.
            let active = cfg.active_alias().unwrap_or("?").to_string();
            confirm_or_announce(&active, "delete", &paths.join(" "), true)?;
            // Keep deleting past individual failures (mirrors cp's
            // per-file tolerance); report and exit non-zero at the end.
            let mut failed = 0usize;
            for path in &paths {
                match c.delete_file(cfg.active_wk()?, path).await {
                    Ok(_) => println!("Deleted {path}"),
                    Err(e) => {
                        eprintln!("Failed {path}: {e}");
                        failed += 1;
                    }
                }
            }
            if failed > 0 {
                anyhow::bail!("{failed}/{} deletions failed", paths.len());
            }
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
            c.append_file(cfg.active_wk()?, &path, &data).await?;
            println!("Appended {} bytes to {path}", data.len());
        }
        Commands::Mkdir { path } => {
            c.mkdir(cfg.active_wk()?, &path).await?;
            println!("Created directory {path}");
        }
        Commands::Search {
            query,
            mode,
            limit,
            detail_level,
            path,
        } => {
            let resp = c
                .search(
                    cfg.active_wk()?,
                    &query,
                    &mode,
                    limit,
                    detail_level.as_str(),
                    path.as_deref(),
                )
                .await?;
            if json_output {
                if let Some(arr) = resp["data"].as_array() {
                    for hit in arr {
                        println!("{hit}");
                    }
                }
            } else if let Some(arr) = resp["data"].as_array() {
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
        Commands::Ask { question, path } => {
            let resp = match c.ask(cfg.active_wk()?, &question, path.as_deref()).await {
                Ok(r) => r,
                Err(e) => {
                    // 501/429 are expected states, not crashes — translate
                    // them into actionable one-liners before the generic
                    // error path takes over.
                    let msg = e.to_string();
                    if msg.contains("FEATURE_DISABLED") {
                        anyhow::bail!("问答未启用:server 未配置 LLM([llm] 缺失)。可改用 `veda search`。");
                    }
                    if msg.contains("THROTTLED") {
                        anyhow::bail!("问答并发已满(每 workspace 上限),稍后重试。");
                    }
                    return Err(e);
                }
            };
            if json_output {
                println!("{}", resp["data"]);
            } else {
                let data = &resp["data"];
                println!("{}", data["answer"].as_str().unwrap_or(""));
                if let Some(cites) = data["citations"].as_array().filter(|c| !c.is_empty()) {
                    println!("\n———\n出处:");
                    // Same-file citations collapse into one line (a file
                    // cited for two passages is still one source to read).
                    let mut seen: Vec<&str> = Vec::new();
                    for c in cites {
                        if let Some(p) = c["path"].as_str() {
                            if !seen.contains(&p) {
                                seen.push(p);
                                println!("  {p}");
                            }
                        }
                    }
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
                    cfg.active_wk()?,
                    &pattern,
                    path.as_deref(),
                    ignore_case,
                    limit,
                )
                .await?;
            if json_output {
                if let Some(arr) = resp["data"].as_array() {
                    for hit in arr {
                        println!("{hit}");
                    }
                }
            } else if let Some(arr) = resp["data"].as_array() {
                for hit in arr {
                    let path = hit["path"].as_str().unwrap_or("?");
                    let line_no = hit["line_no"].as_u64().unwrap_or(0);
                    let line = hit["line"].as_str().unwrap_or("");
                    println!("{path}:{line_no}: {line}");
                }
            }
        }
        Commands::Abstract { path } => {
            print_summary_layer(&c, cfg.active_wk()?, &path, "abstract", "L0 Abstract", "l0_abstract")
                .await?;
        }
        Commands::Overview { path } => {
            print_summary_layer(&c, cfg.active_wk()?, &path, "overview", "L1 Overview", "l1_overview")
                .await?;
        }
        Commands::Layout => {
            let resp = c.workspace_layout(cfg.active_wk()?).await?;
            // Without this check a response missing `data` renders as
            // "empty workspace" — a broken server would look like an empty
            // one, which is the worst way to be wrong here.
            let data = &resp["data"];
            if !data.is_object() || !data["entries"].is_array() {
                anyhow::bail!(
                    "unexpected /v1/layout response (no data.entries array): {resp}"
                );
            }
            if json_output {
                println!("{data}");
            } else {
                print_layout(data);
            }
        }
        Commands::Collection { action } => match action {
            CollectionCmd::Create {
                name,
                schema,
                embed_source,
            } => {
                let schema_val: serde_json::Value = serde_json::from_str(&schema)?;
                let resp = c
                    .create_collection(cfg.active_wk()?, &name, &schema_val, embed_source.as_deref())
                    .await?;
                println!(
                    "Collection created: {}",
                    resp["data"]["id"].as_str().unwrap_or(&name)
                );
            }
            CollectionCmd::List => {
                let resp = c.list_collections(cfg.active_wk()?).await?;
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
                let resp = c.describe_collection(cfg.active_wk()?, &name).await?;
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
                c.delete_collection(cfg.active_wk()?, &name).await?;
                println!("Deleted collection {name}");
            }
            CollectionCmd::Insert { name, data } => {
                let rows: serde_json::Value = serde_json::from_str(&data)?;
                c.insert_rows(cfg.active_wk()?, &name, &rows).await?;
                println!("Rows inserted into {name}");
            }
            CollectionCmd::Search { name, query, limit } => {
                let resp = c
                    .search_collection(cfg.active_wk()?, &name, &query, limit)
                    .await?;
                // collection-search and sql already print one JSON
                // object per line — the same shape as --json mode.
                // The flag is accepted for consistency but doesn't
                // change behavior here.
                let _ = json_output;
                if let Some(arr) = resp["data"].as_array() {
                    for row in arr {
                        println!("{row}");
                    }
                }
            }
        },
        Commands::Sql { query } => {
            let resp = c.execute_sql(cfg.active_wk()?, &query).await?;
            let _ = json_output;
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
                    "active_workspace: {}",
                    cfg.active_alias().unwrap_or("<not set>")
                );
                if cfg.workspaces.is_empty() {
                    println!("workspaces: (none)");
                } else {
                    println!("workspaces:");
                    for (alias, entry) in &cfg.workspaces {
                        println!(
                            "  {alias}: id={} key={}",
                            entry.id.as_deref().unwrap_or("(unknown)"),
                            mask_secret(&entry.key)
                        );
                    }
                }
            }
            ConfigCmd::Set { key, value } => {
                // Only top-level scalars are settable here. Workspace
                // entries are richer (id+key) and minted via the
                // server — use `veda workspace add` for those.
                match key.as_str() {
                    "server_url" => cfg.server_url = value,
                    "api_key" => cfg.api_key = Some(value),
                    "active_workspace" => {
                        cfg.workspace_for(&value)?;
                        cfg.active_workspace = Some(value);
                    }
                    _ => anyhow::bail!(
                        "unknown config key: {key} (use `veda workspace add` for workspace entries)"
                    ),
                }
                cfg.save()?;
                println!("Config updated.");
            }
        },
    }

    Ok(())
}

/// Read a local file's raw bytes for upload — text and binary alike. The
/// server sniffs UTF-8 to decide text vs blob storage, so the client no
/// longer pre-validates encoding (PDFs / images / jars upload as-is).
fn read_file_bytes(src: impl AsRef<std::path::Path>) -> anyhow::Result<Vec<u8>> {
    let src = src.as_ref();
    std::fs::read(src).map_err(|e| anyhow::anyhow!("read {} failed: {e}", src.display()))
}

#[derive(Debug)]
struct CpStats {
    uploaded: usize,
    failed: usize,
}

/// A batch upload where every single request fails is a systemic
/// problem (server down, revoked key), not a per-file one — abort
/// instead of grinding through thousands of doomed requests.
const MAX_CONSECUTIVE_FAILURES: usize = 10;

/// Recursively upload every file under `src_root` to `dst_root` on the server.
/// Remote path = dst_root + path-relative-to-src_root. Skips empty directories
/// and ignored names (see `collect_files`). A file that fails to upload is
/// reported and skipped so one bad filename can't strand the rest of the
/// batch; only a run of consecutive failures aborts.
async fn cp_dir_recursive(
    client: &client::Client,
    ws_key: &str,
    src_root: &std::path::Path,
    dst_root: &str,
    no_ignore: bool,
) -> anyhow::Result<CpStats> {
    let dst_root = dst_root.trim_end_matches('/');
    let mut files = Vec::new();
    let found = collect_files(src_root, &mut files, no_ignore)?;
    // We deliberately do NOT report a count of skipped entries: gitignored
    // paths never reach the iterator, so any total we printed would be a
    // number we cannot actually compute. Report what we do know instead.
    if !no_ignore && found.rules_seen {
        eprintln!(
            "  ({} file{} to upload; .gitignore/.vedaignore rules applied — \
             --no-ignore to upload everything)",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        );
    }
    let mut stats = CpStats { uploaded: 0, failed: 0 };
    let mut consecutive = 0usize;
    for f in &files {
        let rel = f.strip_prefix(src_root)?;
        // POSIX path on the server side regardless of host OS
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let remote = format!("{dst_root}/{rel_str}");
        let outcome = match read_file_bytes(f) {
            Ok(content) => client.write_file(ws_key, &remote, content).await.map(|_| ()),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => {
                stats.uploaded += 1;
                consecutive = 0;
                println!("  {} -> {remote}", f.display());
            }
            Err(e) => {
                stats.failed += 1;
                consecutive += 1;
                eprintln!("  FAILED {remote}: {e:#}");
                if consecutive >= MAX_CONSECUTIVE_FAILURES {
                    anyhow::bail!(
                        "aborting after {consecutive} consecutive failures \
                         ({} uploaded, {} failed) — server or key problem?",
                        stats.uploaded,
                        stats.failed
                    );
                }
            }
        }
    }
    Ok(stats)
}

/// Directory names never worth uploading to a knowledge workspace:
/// VCS internals and tool/editor caches add thousands of junk files.
const IGNORED_DIRS: &[&str] = &[".git", "__pycache__", ".idea", "node_modules"];
/// File names never worth uploading: macOS Finder droppings, plus the
/// `gitdir:` pointer file that takes `.git`'s place in worktrees and
/// submodule checkouts (IGNORED_DIRS only matches the directory form).
const IGNORED_FILES: &[&str] = &[".DS_Store", ".git"];

/// What `collect_files` found, beyond the file list itself.
#[derive(Debug)]
pub(crate) struct CollectOutcome {
    /// An ignore file was actually present somewhere in the tree. Drives the
    /// "rules applied" hint — checking only the source root would miss a
    /// `sub/.gitignore`, which does take effect.
    pub rules_seen: bool,
}

/// Collect every uploadable file under `root`.
///
/// Ignore semantics, deliberately narrow: only `.gitignore` and `.vedaignore`
/// files *inside the source tree* apply, plus the built-in skip list. We do
/// not read `.ignore` (a ripgrep convention that outranks `.gitignore`), the
/// user's global gitignore, or `.git/info/exclude` — those would make the
/// same directory upload different content on different machines, and the
/// only thing they bought us (`.DS_Store`) is already in IGNORED_FILES.
fn collect_files(
    root: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
    no_ignore: bool,
) -> anyhow::Result<CollectOutcome> {
    let mut b = ignore::WalkBuilder::new(root);
    b
        // Dotfiles are real content in a knowledge base (.github/, .env.example,
        // .cursor/rules). The crate skips them by default, which would silently
        // drop them relative to the old hand-rolled walk.
        .hidden(false)
        // All four of these must be off or the walker still climbs to the
        // filesystem root looking for ignore files (`Ignore::add_parents`
        // only short-circuits when parents/git_ignore/git_exclude/git_global
        // are ALL false). Ancestor rules never *match* once parents is off,
        // but they are still parsed — so a malformed glob in some ~/.gitignore
        // would surface as a walk error and abort an unrelated upload.
        //
        // `.gitignore` is therefore honoured as a *custom* ignore filename
        // rather than through git_ignore. Same syntax, no ancestor probing,
        // and no dependency on whether the source is a git repo at all.
        // Registration order is precedence: the later name wins, so
        // `.vedaignore` can override `.gitignore`.
        .parents(false)
        .ignore(false)
        .git_global(false)
        .git_exclude(false)
        .git_ignore(false)
        // Do NOT follow symlinks: avoids infinite recursion through directory
        // symlinks and prevents silently uploading files outside the source
        // root via a symlink escape.
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    if !no_ignore {
        b.add_custom_ignore_filename(".gitignore");
        b.add_custom_ignore_filename(".vedaignore");
    }
    // Prune the built-in skip list BEFORE descending. Testing these names in
    // the loop below instead would let the walker descend into .git and yield
    // .git/config — whose file name is "config", not in IGNORED_DIRS — so the
    // entire directory would be uploaded.
    b.filter_entry(|e| {
        // Depth 0 is the source root the user named explicitly: honour it even
        // if it is called `node_modules`. (The crate happens to skip filtering
        // at depth 0 today, but its filter_entry docs promise the predicate is
        // applied to all entries, so don't rely on that.)
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        if e.file_type().is_some_and(|t| t.is_dir()) {
            !IGNORED_DIRS.contains(&name.as_ref())
        } else {
            !IGNORED_FILES.contains(&name.as_ref())
        }
    });

    let mut rules_seen = false;
    for entry in b.build() {
        let entry = entry?;
        // A malformed ignore file does NOT fail the walk — the crate attaches
        // the parse error to the (successful) directory entry and carries on
        // with fewer rules in effect. Swallowing that is how `veda cp` would
        // quietly upload the whole of target/ after a typo in .gitignore, so
        // treat it as fatal: the user must see it, and re-running after a fix
        // is free (uploads dedup by content hash).
        if let Some(err) = entry.error() {
            anyhow::bail!(
                "ignore rules under {} could not be parsed: {err}",
                entry.path().display()
            );
        }
        // file_type() reflects the symlink itself (follow_links is off).
        let Some(ft) = entry.file_type() else {
            continue; // stdin, not reachable for a directory walk
        };
        if ft.is_symlink() {
            // Depth 0 is the source root. walkdir enters an explicitly named
            // root symlink but still reports it as one, so announcing a skip
            // here would contradict the files we then upload from inside it.
            if entry.depth() > 0 {
                eprintln!("skip symlink: {}", entry.path().display());
            }
            continue;
        }
        if ft.is_file() {
            let name = entry.file_name();
            if name == ".gitignore" || name == ".vedaignore" {
                rules_seen = true;
            }
            out.push(entry.into_path());
        }
    }
    Ok(CollectOutcome { rules_seen })
}

#[cfg(test)]
mod cp_bytes_tests {
    //! `veda cp` uploads raw bytes for both text and binary; the server
    //! sniffs UTF-8 to pick text vs blob storage. The client no longer
    //! rejects binary before the HTTP call.
    use super::read_file_bytes;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn reads_utf8_text_bytes() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all("中文 ✓ 🎉".as_bytes()).unwrap();
        let out = read_file_bytes(f.path()).unwrap();
        assert_eq!(out, "中文 ✓ 🎉".as_bytes());
    }

    #[test]
    fn reads_binary_with_nul_verbatim() {
        // PDF / PNG / ELF contain NUL bytes — previously rejected client-side,
        // now read as-is for blob upload.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"%PDF-1.7\0\xff\xc0binary").unwrap();
        let out = read_file_bytes(f.path()).unwrap();
        assert_eq!(out, b"%PDF-1.7\0\xff\xc0binary");
    }

    #[test]
    fn empty_file_reads_empty() {
        let f = NamedTempFile::new().unwrap();
        let out = read_file_bytes(f.path()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn missing_path_yields_read_error() {
        let err = read_file_bytes("/nonexistent/path/abc.txt").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("read") && msg.contains("/nonexistent"), "msg: {msg}");
    }
}

#[cfg(test)]
mod layout_render_tests {
    use super::{human_bytes, render_layout, wrap_display};
    use serde_json::json;
    use unicode_width::UnicodeWidthStr;

    /// Lines that start an entry, i.e. everything the reader will take as a
    /// path. Abstracts are indented, so anything unindented is either a
    /// header or the trailing stats.
    fn unindented(out: &str) -> Vec<&str> {
        out.lines().filter(|l| !l.is_empty() && !l.starts_with("    ")).collect()
    }

    /// The abstract lines of the rendered output, indent stripped.
    fn indented(out: &str) -> Vec<&str> {
        out.lines().filter_map(|l| l.strip_prefix("    ")).collect()
    }

    /// The block shape: `name  meta` on the header, abstract indented
    /// underneath. A CJK name needs no padding to line anything up any
    /// more, but it still has to survive intact with its `/` suffix.
    #[test]
    fn each_entry_is_a_header_line_plus_an_indented_abstract() {
        let out = render_layout(
            &json!({
                "stats": {"total_files": 9, "total_directories": 2, "total_bytes": 1024},
                "summary_state": "ready", "truncated": false,
                "entries": [
                    {"path": "/文档中心", "is_dir": true, "file_count": 42, "abstract": "中文目录名"},
                    {"path": "/docs", "is_dir": true, "file_count": 7, "abstract": "英文目录名"},
                    {"path": "/README.md", "is_dir": false, "size_bytes": 4096, "abstract": "文件"}
                ]
            }),
            Some(80),
        );
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec![
                "文档中心/  42 files",
                "    中文目录名",
                "",
                "docs/  7 files",
                "    英文目录名",
                "",
                "README.md  4.0 KB",
                "    文件",
                "",
                "9 files, 2 directories, 1.0 KB",
            ],
            "rendered:\n{out}"
        );
    }

    /// A blank line has to separate anything multi-line, or an indented
    /// abstract reads as belonging to the wrong header. A listing with no
    /// abstracts at all must *not* get them — that would turn `ls` into a
    /// double-spaced page for no gain.
    #[test]
    fn blank_lines_separate_blocks_but_not_bare_headers() {
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [
                    {"path": "/a", "is_dir": true, "file_count": 1},
                    {"path": "/b", "is_dir": true, "file_count": 2},
                    {"path": "/c", "is_dir": true, "file_count": 3, "abstract": "有摘要"},
                    {"path": "/d", "is_dir": true, "file_count": 4},
                    {"path": "/e", "is_dir": true, "file_count": 5}
                ]
            }),
            Some(80),
        );
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec![
                "a/  1 file",
                "b/  2 files",
                "",
                "c/  3 files",
                "    有摘要",
                "",
                "d/  4 files",
                "e/  5 files",
                "",
                "0 files, 0 directories, 0 B",
            ],
            "rendered:\n{out}"
        );
    }

    #[test]
    fn entry_without_count_or_abstract_has_no_trailing_space() {
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [{"path": "/a", "is_dir": true}]
            }),
            Some(80),
        );
        let first = out.lines().next().unwrap();
        assert_eq!(first, first.trim_end(), "trailing whitespace in {first:?}");
    }

    /// Malformed / partial entries must degrade, never panic — this render
    /// path sits between the user and a server response it does not control.
    /// Non-object elements matter as much as missing fields: the renderer
    /// indexes into every element as if it were a map.
    #[test]
    fn malformed_entries_do_not_panic() {
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [
                    {"path": "/keep", "is_dir": true, "file_count": 1},
                    {"path": "/a"},
                    {"path": 123, "is_dir": true},
                    {"path": "/b", "is_dir": false, "abstract": null},
                    {},
                    null,
                    42,
                    "not-an-object",
                    []
                ]
            }),
            Some(80),
        );
        // Every element still produces exactly one header, and the
        // well-formed one is untouched by its malformed neighbours. None of
        // them has an abstract, so they pack with no blank lines.
        let rows = out.lines().take_while(|l| !l.is_empty()).count();
        assert_eq!(rows, 9, "one row per element:\n{out}");
        assert!(out.contains("keep/  1 file"), "{out}");
    }

    /// `entries` that isn't an array, or a `data` that isn't an object,
    /// must not masquerade as an empty workspace. The renderer treats them
    /// as empty; the caller is what has to reject them (see Commands::Layout).
    #[test]
    fn non_array_entries_render_as_empty_so_the_caller_must_validate() {
        for bad in [json!(null), json!({"entries": "oops"}), json!(42)] {
            let out = render_layout(&bad, Some(80));
            assert!(
                out.contains("empty workspace"),
                "renderer contract changed for {bad}: {out}"
            );
        }
    }

    #[test]
    fn footnotes_only_appear_when_the_answer_is_incomplete() {
        let complete = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [{"path": "/a", "is_dir": true, "file_count": 1, "abstract": "x"}]
            }),
            Some(80),
        );
        assert!(!complete.contains('('), "clean layout needs no footnote:\n{complete}");

        let degraded = render_layout(
            &json!({
                "stats": {}, "summary_state": "disabled", "truncated": true,
                "entries": [{"path": "/a", "is_dir": true, "file_count": 1}]
            }),
            Some(80),
        );
        assert!(degraded.contains("more top-level entries"), "{degraded}");
        assert!(degraded.contains("summaries are disabled"), "{degraded}");
    }

    #[test]
    fn empty_workspace_says_so() {
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false, "entries": []
            }),
            Some(80),
        );
        assert!(out.contains("empty workspace"), "{out}");
    }

    #[test]
    fn human_bytes_switches_units_and_drops_pointless_decimals() {
        for (n, want) in [
            (0_i64, "0 B"),
            (512, "512 B"),
            (1023, "1023 B"),
            (1024, "1.0 KB"),
            (4096, "4.0 KB"),
            // >= 10 in a unit loses the decimal: "524 KB", not "524.3 KB".
            (536_870, "524 KB"),
            (10 * 1024 * 1024, "10 MB"),
            (1024_i64.pow(4), "1.0 TB"),
            // Carry: rounding pushes this to 1024 KB, which must promote to
            // MB rather than print a value the unit can't hold.
            (1_048_575, "1.0 MB"),
            (1_048_576, "1.0 MB"),
            // Rounds to exactly 10 — the ">= 10 has no decimal" rule has to
            // be applied to the rounded value, not the raw one.
            (10_239, "10 KB"),
            // Counts can't be negative; a broken response shows 0, not a
            // 20-digit negative.
            (-1, "0 B"),
            (i64::MIN, "0 B"),
            // TB is the last unit: absurd inputs keep counting in TB rather
            // than walking off the end of the table.
            (i64::MAX, "8388608 TB"),
        ] {
            assert_eq!(human_bytes(n), want, "human_bytes({n})");
        }
    }

    /// A line break in an LLM-written abstract would otherwise render as a
    /// whole extra entry — invented data that looks exactly like the real
    /// thing. Indentation is what tells them apart now, so the forged text
    /// must never reach column zero.
    #[test]
    fn abstract_newline_cannot_forge_an_entry() {
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [
                    {"path": "/a", "is_dir": true, "file_count": 1,
                     "abstract": "第一行\n伪造的第二行  99 files  假的"},
                    {"path": "/b", "is_dir": true, "file_count": 2, "abstract": "正常"}
                ]
            }),
            None,
        );
        // The abstract's text is preserved — what must not survive is its
        // *structure*: two entries, and the second one is the real /b
        // rather than the forged row.
        assert_eq!(
            unindented(&out),
            vec!["a/  1 file", "b/  2 files", "0 files, 0 directories, 0 B"],
            "forged entry reached column zero:\n{out}"
        );
        assert!(
            out.contains("    第一行 伪造的第二行  99 files  假的"),
            "newline should fold to a space and stay indented:\n{out}"
        );
    }

    /// ESC in an abstract can repaint the screen, move the cursor, or hide
    /// the rest of the listing. It is folded like any other control char.
    #[test]
    fn escape_sequences_are_folded_to_spaces() {
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [{"path": "/a", "is_dir": true, "file_count": 1,
                             "abstract": "before\u{1b}[2Jafter\u{7}\u{d}tail"}]
            }),
            None,
        );
        assert!(!out.contains('\u{1b}'), "ESC survived:\n{out:?}");
        assert!(!out.contains('\u{7}'), "BEL survived:\n{out:?}");
        assert!(!out.contains('\r'), "CR survived:\n{out:?}");
        assert!(out.contains("before [2Jafter  tail"), "{out:?}");
    }

    /// An abstract that is nothing but whitespace and control characters
    /// cleans up to the empty string. That is *no* abstract — printing it
    /// would leave a stray indented blank line under the header.
    #[test]
    fn blank_abstract_is_treated_as_absent() {
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [
                    {"path": "/a", "is_dir": true, "file_count": 1, "abstract": "  \u{1b}\n "},
                    {"path": "/b", "is_dir": true, "file_count": 2}
                ]
            }),
            Some(80),
        );
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec!["a/  1 file", "b/  2 files", "", "0 files, 0 directories, 0 B"],
            "rendered:\n{out:?}"
        );
    }

    /// The whole point of the block layout: a real 300+ character L0 is
    /// shown in full. Nothing is capped, nothing is elided.
    #[test]
    fn long_abstract_is_shown_in_full() {
        let long = "这个目录收录了公司内部的技术文档与运维手册涵盖部署流程监控告警与故障排查".repeat(10);
        assert!(long.chars().count() > 300, "fixture too short");

        let wrapped = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [{"path": "/a", "is_dir": true, "file_count": 1, "abstract": long}]
            }),
            Some(80),
        );
        assert!(!wrapped.contains('…'), "abstract was elided:\n{wrapped}");
        // Pure CJK has no spaces, so the wrapped lines rejoin to exactly
        // the original text — nothing dropped at the break points.
        assert_eq!(indented(&wrapped).concat(), long, "text lost in wrapping:\n{wrapped}");
        assert!(indented(&wrapped).len() > 4, "expected several wrapped lines:\n{wrapped}");
    }

    /// Down a pipe there is no width to respect, and wrapping would split
    /// the abstract across lines that `grep` then can't match.
    #[test]
    fn no_wrap_width_keeps_the_abstract_on_one_line() {
        let long = "这个目录收录了公司内部的技术文档与运维手册".repeat(20);
        let out = render_layout(
            &json!({
                "stats": {}, "summary_state": "ready", "truncated": false,
                "entries": [{"path": "/a", "is_dir": true, "file_count": 1, "abstract": long}]
            }),
            None,
        );
        assert_eq!(indented(&out), vec![long.as_str()], "rendered:\n{out}");
    }

    /// Every wrapped line has to fit the terminal, indent included, or the
    /// terminal re-wraps it and the hanging indent falls apart. Measured in
    /// display cells: CJK is two cells per char.
    #[test]
    fn wrapped_lines_fit_the_terminal_width() {
        let mixed = "veda 的 layout 命令 renders a workspace 顶层地图 with per-directory \
                     摘要，每个条目包含名称、文件数和一段简短介绍 so that an agent can \
                     orient itself 而不需要逐个目录 ls 下去。"
            .repeat(4);
        for w in [40usize, 55, 80, 120] {
            let out = render_layout(
                &json!({
                    "stats": {}, "summary_state": "ready", "truncated": false,
                    "entries": [{"path": "/文档中心", "is_dir": true, "file_count": 3,
                                 "abstract": mixed}]
                }),
                Some(w),
            );
            for line in indented(&out) {
                assert!(
                    line.width() + 4 <= w,
                    "line is {} cells at width {w}: {line:?}",
                    line.width() + 4
                );
            }
        }
    }

    /// English breaks between words, never inside one — a wrapper that cuts
    /// at a byte or cell count would mangle every identifier in a summary.
    #[test]
    fn english_wraps_between_words() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let lines = wrap_display(text, 20);
        assert!(lines.len() > 2, "expected wrapping: {lines:?}");
        assert!(lines.iter().all(|l| l.width() <= 20), "{lines:?}");
        // Same words, same order, no fragments.
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>(),
            "words were split or lost: {lines:?}"
        );
    }

    /// A token with no break opportunity at all — a long URL or a hash —
    /// is cut by character. Overflowing the terminal is the worse option.
    #[test]
    fn unbreakable_run_is_hard_cut() {
        let blob = "a".repeat(200);
        let lines = wrap_display(&blob, 36);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert!(lines[..5].iter().all(|l| l.width() == 36), "{lines:?}");
        assert_eq!(lines.concat(), blob, "characters lost in the hard cut");
    }

    #[test]
    fn negative_counts_are_treated_as_missing() {
        let out = render_layout(
            &json!({
                "stats": {"total_files": -3, "total_bytes": -5},
                "summary_state": "ready", "truncated": false,
                "entries": [
                    {"path": "/a", "is_dir": true, "file_count": -1},
                    {"path": "/f", "is_dir": false, "size_bytes": -9}
                ]
            }),
            Some(80),
        );
        assert!(!out.contains("-1"), "{out}");
        assert!(!out.contains("-9"), "{out}");
        assert!(!out.contains("-5"), "{out}");
    }
}

#[cfg(test)]
mod cp_dir_tests {
    use super::{collect_files, cp_dir_recursive};
    use std::fs;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok_json() -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"success": true, "data": {"revision": 1}}))
    }

    /// Relative paths in walk order. Components are joined with "/" rather
    /// than via a string replace, so a Unix file name that legitimately
    /// contains a backslash stays one component.
    fn walk_order(root: &std::path::Path, no_ignore: bool) -> Vec<String> {
        let mut files = Vec::new();
        collect_files(root, &mut files, no_ignore).unwrap();
        files
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
    }

    /// Relative paths of everything `collect_files` would upload, sorted so
    /// membership assertions do not depend on traversal order.
    fn collected(root: &std::path::Path, no_ignore: bool) -> Vec<String> {
        let mut names = walk_order(root, no_ignore);
        names.sort();
        names
    }

    #[test]
    fn collect_files_skips_vcs_and_finder_junk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "x").unwrap();
        fs::create_dir(root.join("__pycache__")).unwrap();
        fs::write(root.join("__pycache__/a.pyc"), "x").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/b.txt"), "x").unwrap();
        fs::write(root.join("sub/.DS_Store"), "x").unwrap();
        fs::write(root.join(".DS_Store"), "x").unwrap();
        fs::write(root.join("a.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec!["a.txt", "sub/b.txt"]);
    }

    /// The built-in skip list must PRUNE, not name-match. If `.git` is only
    /// tested per-entry, the walker descends into it and yields
    /// `.git/objects/ab/cd` — whose file name is "cd", matching nothing — and
    /// the whole repo internals get uploaded. Nested files are the only
    /// assertion that can tell the two implementations apart.
    #[test]
    fn collect_files_prunes_ignored_dirs_rather_than_name_matching() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git/objects/ab")).unwrap();
        fs::write(root.join(".git/objects/ab/cd"), "x").unwrap();
        fs::write(root.join(".git/config"), "x").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg/lib")).unwrap();
        fs::write(root.join("node_modules/pkg/lib/index.js"), "x").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec!["keep.txt"]);
    }

    /// In a git worktree or submodule checkout `.git` is a plain file (a
    /// one-line `gitdir:` pointer), not a directory — the skip list must
    /// catch that spelling too, or the pointer file gets uploaded.
    #[test]
    fn collect_files_skips_a_worktree_gitdir_pointer_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".git"), "gitdir: /repo/.git/worktrees/x\n").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec!["keep.txt"]);
    }

    #[test]
    fn collect_files_honours_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/x"), "x").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec![".gitignore", "keep.txt"]);
    }

    #[test]
    fn collect_files_honours_vedaignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".vedaignore"), "*.log\n").unwrap();
        fs::write(root.join("a.log"), "x").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec![".vedaignore", "keep.txt"]);
    }

    /// The crate skips dotfiles by default. A knowledge base wants them:
    /// .github/, .env.example and .cursor/rules are real content. Dropping
    /// this would be a silent regression against the old hand-rolled walk.
    #[test]
    fn collect_files_keeps_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::write(root.join(".github/workflows/ci.yml"), "x").unwrap();
        fs::write(root.join(".env.example"), "x").unwrap();

        assert_eq!(
            collected(root, false),
            vec![".env.example", ".github/workflows/ci.yml"]
        );
    }

    /// The crate ignores .gitignore entirely outside a git repo unless
    /// require_git(false) is set — and does so silently.
    #[test]
    fn collect_files_honours_gitignore_outside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No .git anywhere: a plain documentation directory.
        fs::write(root.join(".gitignore"), "skip.txt\n").unwrap();
        fs::write(root.join("skip.txt"), "x").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec![".gitignore", "keep.txt"]);
    }

    #[test]
    fn collect_files_no_ignore_keeps_gitignored_but_still_prunes_builtins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/x"), "x").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "x").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(
            collected(root, true),
            vec![".gitignore", "keep.txt", "target/debug/x"]
        );
    }

    /// Ignore files above the source root must not apply: with
    /// require_git(false), parents(true) would read past the repo root and
    /// let a stray ~/.gitignore change what gets uploaded.
    #[test]
    fn collect_files_does_not_read_ignore_files_above_the_source_root() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        fs::write(parent.join(".gitignore"), "keep.txt\n").unwrap();
        let root = parent.join("src");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(&root, false), vec!["keep.txt"]);
    }

    /// `.ignore` is a ripgrep convention that outranks .gitignore. Honouring
    /// it would let a file the user wrote for a different tool silently
    /// change knowledge-base content.
    #[test]
    fn collect_files_does_not_read_dot_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".ignore"), "x.txt\n").unwrap();
        fs::write(root.join("x.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec![".ignore", "x.txt"]);
    }

    #[test]
    fn collect_files_honours_nested_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/.gitignore"), "*.tmp\n").unwrap();
        fs::write(root.join("sub/a.tmp"), "x").unwrap();
        fs::write(root.join("sub/b.txt"), "x").unwrap();
        fs::write(root.join("top.tmp"), "x").unwrap();

        // sub/*.tmp is filtered; the rule does not leak up to the root.
        assert_eq!(
            collected(root, false),
            vec!["sub/.gitignore", "sub/b.txt", "top.tmp"]
        );
    }

    /// Both the link target and the source root live inside one TempDir, so
    /// nothing outside the test's own scratch space is created or removed
    /// and cleanup still happens when an assertion fails.
    #[cfg(unix)]
    #[test]
    fn collect_files_skips_symlinks_without_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let root = dir.path().join("src");
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::write(outside.join("nested/secret.txt"), "x").unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link_dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("nested/secret.txt"), root.join("link_file"))
            .unwrap();

        // Neither the symlink itself nor anything behind it is uploaded.
        assert_eq!(collected(&root, false), vec!["keep.txt"]);
    }

    /// A symlink pointing at its own ancestor must not spin forever.
    #[cfg(unix)]
    #[test]
    fn collect_files_terminates_on_a_self_referential_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root, root.join("loop")).unwrap();

        assert_eq!(collected(root, false), vec!["keep.txt"]);
    }

    /// Walk order is deterministic: without an explicit sorter the crate
    /// yields directory entries in filesystem order, which varies.
    #[test]
    fn collect_files_walks_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for n in ["m.txt", "a.txt", "z.txt", "b.txt"] {
            fs::write(root.join(n), "x").unwrap();
        }
        fs::create_dir(root.join("adir")).unwrap();
        fs::write(root.join("adir/inner.txt"), "x").unwrap();

        assert_eq!(
            walk_order(root, false),
            vec!["a.txt", "adir/inner.txt", "b.txt", "m.txt", "z.txt"]
        );
    }

    /// A typo in an ignore file must not degrade to "upload everything":
    /// the crate keeps walking with fewer rules and reports the parse error
    /// out-of-band, which is exactly the silent failure this feature exists
    /// to prevent.
    #[test]
    fn collect_files_fails_loudly_on_a_malformed_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // `[z-a]` is an inverted character range — one of the few things
        // globset actually rejects (unbalanced brackets are tolerated).
        fs::write(root.join(".gitignore"), "target/\n[z-a]\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/x"), "x").unwrap();

        let mut files = Vec::new();
        let err = collect_files(root, &mut files, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ignore rules"), "got: {msg}");
    }

    /// An ignore file above the source root must not even be *parsed*: with
    /// git_ignore enabled the crate climbs to the filesystem root looking
    /// for one, and a malformed glob up there would abort this upload.
    #[test]
    fn collect_files_ignores_a_malformed_ignore_file_above_the_source_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "[z-a]\n").unwrap();
        let root = dir.path().join("src");
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/x"), "x").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(&root, false), vec![".gitignore", "keep.txt"]);
    }

    /// `.vedaignore` is registered after `.gitignore`, so it wins.
    #[test]
    fn vedaignore_overrides_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "notes.txt\n").unwrap();
        fs::write(root.join(".vedaignore"), "!notes.txt\n").unwrap();
        fs::write(root.join("notes.txt"), "x").unwrap();

        assert!(collected(root, false).contains(&"notes.txt".to_string()));
    }

    #[test]
    fn collect_files_no_ignore_also_disables_vedaignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".vedaignore"), "skip.txt\n").unwrap();
        fs::write(root.join("skip.txt"), "x").unwrap();

        assert!(collected(root, true).contains(&"skip.txt".to_string()));
    }

    /// `.git/info/exclude` is repo-local and invisible to collaborators;
    /// honouring it would make uploads depend on one machine's setup.
    #[test]
    fn collect_files_does_not_read_git_info_exclude() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(root.join(".git/info/exclude"), "keep.txt\n").unwrap();
        fs::write(root.join("keep.txt"), "x").unwrap();

        assert_eq!(collected(root, false), vec!["keep.txt"]);
    }

    /// A directory rule prunes the whole subtree rather than skipping one
    /// level, and its scope is the `.gitignore`'s own directory — not its
    /// siblings, not its parent. This is where the feature's value actually
    /// comes from: `target/` is never descended into, so its half-million
    /// files are never even stat'd.
    #[test]
    fn nested_gitignore_directory_rules_prune_whole_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("root.md"), "x").unwrap();

        // Rules live only in sub/, never at the root.
        fs::create_dir_all(root.join("sub/build/deep/deeper")).unwrap();
        fs::create_dir_all(root.join("sub/keep/inner/tmp")).unwrap();
        fs::create_dir_all(root.join("sub/dist")).unwrap();
        fs::write(root.join("sub/.gitignore"), "build/\n/only-here.txt\ndist\n").unwrap();
        fs::write(root.join("sub/build/a.o"), "x").unwrap();
        fs::write(root.join("sub/build/deep/deeper/b.o"), "x").unwrap();
        fs::write(root.join("sub/dist/c.js"), "x").unwrap();
        fs::write(root.join("sub/only-here.txt"), "x").unwrap();
        fs::write(root.join("sub/keep/ok.md"), "x").unwrap();
        // A second, deeper ignore file: nesting has no depth limit.
        fs::write(root.join("sub/keep/inner/.gitignore"), "tmp/\n").unwrap();
        fs::write(root.join("sub/keep/inner/tmp/e.txt"), "x").unwrap();
        fs::write(root.join("sub/keep/inner/f.md"), "x").unwrap();

        // Same names one level over: sub/'s rules must not reach them.
        fs::create_dir_all(root.join("other/build")).unwrap();
        fs::write(root.join("other/build/d.o"), "x").unwrap();
        fs::write(root.join("other/only-here.txt"), "x").unwrap();

        assert_eq!(
            collected(root, false),
            vec![
                "other/build/d.o",
                "other/only-here.txt",
                "root.md",
                "sub/.gitignore",
                "sub/keep/inner/.gitignore",
                "sub/keep/inner/f.md",
                "sub/keep/ok.md",
            ]
        );
    }

    #[test]
    fn collect_files_honours_nested_vedaignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/.vedaignore"), "*.tmp\n").unwrap();
        fs::write(root.join("sub/a.tmp"), "x").unwrap();
        fs::write(root.join("sub/b.txt"), "x").unwrap();

        assert_eq!(
            collected(root, false),
            vec!["sub/.vedaignore", "sub/b.txt"]
        );
    }

    #[test]
    fn rules_seen_is_reported_for_a_nested_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/.gitignore"), "*.tmp\n").unwrap();
        fs::write(root.join("sub/a.txt"), "x").unwrap();

        let mut files = Vec::new();
        // Only a subdirectory carries rules; checking the source root alone
        // would report "no rules" while they are demonstrably in effect.
        assert!(collect_files(root, &mut files, false).unwrap().rules_seen);

        let dir2 = tempfile::tempdir().unwrap();
        fs::write(dir2.path().join("a.txt"), "x").unwrap();
        let mut f2 = Vec::new();
        assert!(!collect_files(dir2.path(), &mut f2, false).unwrap().rules_seen);
    }

    /// The source root the user named explicitly is honoured even when its
    /// own name is on the built-in skip list.
    #[test]
    fn collect_files_does_not_filter_the_source_root_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("node_modules");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("index.js"), "x").unwrap();

        assert_eq!(collected(&root, false), vec!["index.js"]);
    }

    #[tokio::test]
    async fn cp_dir_continues_past_a_failed_file() {
        // The guidewiki incident: one 400 killed the remaining 13k files.
        // A per-file failure must not strand the rest of the batch.
        let server = MockServer::start().await;
        // Specific mock first — wiremock matches in mount order.
        Mock::given(method("PUT"))
            .and(path("/v1/fs/dst/bad.txt"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad path"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ok_json())
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bad.txt"), "x").unwrap();
        fs::write(dir.path().join("good1.txt"), "x").unwrap();
        fs::write(dir.path().join("good2.txt"), "x").unwrap();

        let client = super::client::Client::new(&server.uri());
        let stats = cp_dir_recursive(&client, "wk-test", dir.path(), "/dst", false)
            .await
            .unwrap();
        assert_eq!(stats.uploaded, 2);
        assert_eq!(stats.failed, 1);
    }

    #[tokio::test]
    async fn cp_dir_aborts_after_consecutive_failures() {
        // Every request failing = systemic (server down / bad key);
        // don't grind through thousands of doomed uploads.
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        for i in 0..12 {
            fs::write(dir.path().join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let client = super::client::Client::new(&server.uri());
        let err = cp_dir_recursive(&client, "wk-test", dir.path(), "/dst", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("consecutive"), "got: {err}");
    }
}










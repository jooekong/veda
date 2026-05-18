mod client;
mod config;
mod init;
mod status;
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
    /// default. Currently affects `ls`, `search`, `grep`,
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
    Status,
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
    },
    /// Read file from server. Three mutually-exclusive slice flags
    /// pick a subset; default streams the whole file.
    Cat {
        /// Remote path
        path: String,
        /// 1-indexed inclusive line range, e.g. `1:20` for lines 1
        /// through 20, or `42:` for line 42 to end-of-file.
        #[arg(long, conflicts_with_all = ["head", "tail"])]
        range: Option<String>,
        /// Show the first N lines (server-side range, equivalent
        /// to `--range 1:N`).
        #[arg(long, conflicts_with_all = ["range", "tail"])]
        head: Option<usize>,
        /// Show the last N lines (fetches whole file then slices
        /// locally — there's no server endpoint for tail offsets).
        #[arg(long, conflicts_with_all = ["range", "head"])]
        tail: Option<usize>,
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
    /// Remove a local workspace profile (alias-only, does NOT revoke
    /// the wk_ key on the server — there is no server endpoint for
    /// that yet). The active profile cannot be removed; switch first.
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

/// Destructiveness levels for [`confirm_or_announce`].
enum WriteAction {
    /// User-visible verb for the prompt / announcement banner.
    /// Examples: "delete", "move", "copy to".
    Verb(&'static str),
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
    action: WriteAction,
    path: &str,
    confirm: bool,
) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    let WriteAction::Verb(verb) = action;
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
        let cmd = Commands::Status;
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
        Commands::Status => {
            // Skip the ping when nothing is configured — there's no
            // server to talk to that the user opted into.
            let reachable = if cfg.api_key.is_some() || !cfg.workspaces.is_empty() {
                Some(status::ping_server(&cfg.server_url).await)
            } else {
                None
            };
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
                     (server-side wk_ key is not revoked — no endpoint yet)"
                );
            }
        },
        Commands::Cp { src, dst } => {
            // Non-destructive announcement: cp writes a new revision,
            // so a wrong workspace is recoverable. Skip the blocking
            // prompt but still print the workspace alias.
            let active = cfg.active_alias().unwrap_or("?").to_string();
            confirm_or_announce(&active, WriteAction::Verb("copy to"), &dst, false)?;
            if src == "-" {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                let resp = c.write_file(cfg.active_wk()?, &dst, &buf).await?;
                println!("Written: revision {}", resp["data"]["revision"]);
            } else {
                let src_path = std::path::Path::new(&src);
                if src_path.is_dir() {
                    let n = cp_dir_recursive(&c, cfg.active_wk()?, src_path, &dst).await?;
                    println!("Uploaded {n} file(s) under {dst}");
                } else {
                    let content = read_utf8_text_or_bail(&src)?;
                    let resp = c.write_file(cfg.active_wk()?, &dst, &content).await?;
                    println!("Written: revision {}", resp["data"]["revision"]);
                }
            }
        }
        Commands::Cat { path, range, head, tail } => {
            // clap's conflicts_with_all already rejects > 1 of these;
            // here we just translate to the server-side `lines`
            // parameter shape (1-indexed inclusive A:B, with B empty
            // = to EOF). `--tail` is the only one we can't express
            // server-side, so it goes through the slice-after-fetch
            // path.
            if let Some(n) = tail {
                let content = c.read_file(cfg.active_wk()?, &path, None).await?;
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(n);
                // Print in original order, preserving a trailing
                // newline only when the source had one.
                let trailing = if content.ends_with('\n') { "\n" } else { "" };
                print!("{}{trailing}", lines[start..].join("\n"));
            } else {
                let line_spec = match (range.as_deref(), head) {
                    (Some(r), _) => Some(r.to_string()),
                    (None, Some(n)) => Some(format!("1:{n}")),
                    (None, None) => None,
                };
                let content = c
                    .read_file(cfg.active_wk()?, &path, line_spec.as_deref())
                    .await?;
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
                WriteAction::Verb("move into"),
                &format!("{src} → {dst}"),
                false,
            )?;
            c.rename_file(cfg.active_wk()?, &src, &dst).await?;
            println!("Moved {src} -> {dst}");
        }
        Commands::Rm { path } => {
            // rm is the only data-plane command that's irreversible
            // against the wrong workspace, so this is the one we ask
            // for explicit y/N confirmation on a TTY.
            let active = cfg.active_alias().unwrap_or("?").to_string();
            confirm_or_announce(&active, WriteAction::Verb("delete"), &path, true)?;
            c.delete_file(cfg.active_wk()?, &path).await?;
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
        } => {
            let resp = c
                .search(cfg.active_wk()?, &query, &mode, limit, detail_level.as_str())
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

/// Read a local file as UTF-8 text, failing fast on binary input with a
/// clear "not a text file" message before any bytes leave the client.
///
/// Two checks layered:
///   1. **NUL byte sniff** over the full content. Catches binaries
///      (PDFs, JPEGs, ELF executables, etc.) whose first chunk happens
///      to look like valid UTF-8 — most have at least one NUL.
///   2. **`String::from_utf8` validation**. Catches non-NUL but
///      non-UTF-8 inputs (UTF-16 with BOM, ISO-8859-1 with high-bit
///      bytes, mojibake from a wrong save).
///
/// `std::fs::read_to_string` already does (2) implicitly but returns
/// the generic `std::io::ErrorKind::InvalidData` with no path or
/// remediation hint; the explicit path here gives the user something
/// they can act on.
fn read_utf8_text_or_bail(src: impl AsRef<std::path::Path>) -> anyhow::Result<String> {
    let src = src.as_ref();
    let bytes = std::fs::read(src)
        .map_err(|e| anyhow::anyhow!("read {} failed: {e}", src.display()))?;
    if bytes.contains(&0) {
        anyhow::bail!(
            "'{}' looks binary (contains NUL bytes); veda only accepts UTF-8 text \
             (PDFs / images / executables aren't supported)",
            src.display()
        );
    }
    String::from_utf8(bytes).map_err(|e| {
        anyhow::anyhow!(
            "'{}' is not valid UTF-8 ({}); veda only accepts UTF-8 text",
            src.display(),
            e.utf8_error()
        )
    })
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
        let content = read_utf8_text_or_bail(f)?;
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

#[cfg(test)]
mod cp_utf8_tests {
    //! `veda cp` rejects binary input before any HTTP call. The
    //! previous behavior — `std::fs::read_to_string` returning a
    //! generic `InvalidData` — gave users an opaque "stream did not
    //! contain valid UTF-8" with no path or remediation hint.
    use super::read_utf8_text_or_bail;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn plain_ascii_text_passes() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello, world\n").unwrap();
        let out = read_utf8_text_or_bail(f.path()).unwrap();
        assert_eq!(out, "hello, world\n");
    }

    #[test]
    fn utf8_with_multibyte_passes() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all("中文 ✓ 🎉".as_bytes()).unwrap();
        let out = read_utf8_text_or_bail(f.path()).unwrap();
        assert_eq!(out, "中文 ✓ 🎉");
    }

    #[test]
    fn nul_byte_in_middle_is_rejected_as_binary() {
        // PDF / PNG / ELF all contain NUL bytes; this is the
        // fast-fail signal even when the prefix happens to be ASCII.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"text\0more text").unwrap();
        let err = read_utf8_text_or_bail(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("looks binary"), "msg: {msg}");
        assert!(msg.contains("NUL"), "msg: {msg}");
        // Error must mention the path so a `for f in *.bin; do veda cp` loop
        // doesn't leave the user guessing which file tripped it.
        assert!(msg.contains(&f.path().display().to_string()), "msg: {msg}");
    }

    #[test]
    fn invalid_utf8_without_nul_is_rejected() {
        // ISO-8859-1 / Windows-1252 / a stray UTF-16 BOM: bytes with
        // the high bit set that don't form valid UTF-8 sequences.
        // 0xFF 0xFE is the UTF-16-LE BOM; 0xC3 alone is a UTF-8
        // continuation prefix without its trailing byte.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&[0xFF, 0xFE, b'a', b'b', 0xC3]).unwrap();
        let err = read_utf8_text_or_bail(f.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not valid UTF-8"), "msg: {msg}");
    }

    #[test]
    fn empty_file_passes() {
        let f = NamedTempFile::new().unwrap();
        let out = read_utf8_text_or_bail(f.path()).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn missing_path_yields_read_error() {
        // Should fail at the read step with a clear "read … failed"
        // message, not a panic or a UTF-8-shaped error.
        let err = read_utf8_text_or_bail("/nonexistent/path/abc.txt").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("read") && msg.contains("/nonexistent"), "msg: {msg}");
    }
}

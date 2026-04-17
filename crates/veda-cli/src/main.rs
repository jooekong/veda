mod client;
mod config;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "veda", about = "Veda CLI client")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Server URL (overrides config)
    #[arg(long, global = true)]
    server: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Account management
    Account {
        #[command(subcommand)]
        action: AccountCmd,
    },
    /// Workspace management
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
    Mv {
        src: String,
        dst: String,
    },
    /// Delete file or directory
    Rm {
        path: String,
    },
    /// Append content to a file
    Append {
        /// Remote path
        path: String,
        /// Content to append (or "-" for stdin)
        content: String,
    },
    /// Create directory
    Mkdir {
        path: String,
    },
    /// Search files
    Search {
        query: String,
        #[arg(long, default_value = "hybrid")]
        mode: String,
        #[arg(long, default_value = "10")]
        limit: usize,
    },
    /// Collection management
    Collection {
        #[command(subcommand)]
        action: CollectionCmd,
    },
    /// Execute SQL query
    Sql {
        query: String,
    },
    /// Configuration management
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
    Set {
        key: String,
        value: String,
    },
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
        Commands::Account { action } => match action {
            AccountCmd::Create { name, email, password } => {
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
                println!("Workspace created: {}", resp["data"]["id"].as_str().unwrap_or(""));
            }
            WorkspaceCmd::List => {
                let resp = c.list_workspaces(cfg.api_key()?).await?;
                if let Some(arr) = resp["data"].as_array() {
                    for ws in arr {
                        println!("{}\t{}", ws["id"].as_str().unwrap_or(""), ws["name"].as_str().unwrap_or(""));
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
            let content = if src == "-" {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                std::fs::read_to_string(&src)?
            };
            let resp = c.write_file(cfg.ws_key()?, &dst, &content).await?;
            println!("Written: revision {}", resp["data"]["revision"]);
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
        Commands::Search { query, mode, limit } => {
            let resp = c.search(cfg.ws_key()?, &query, &mode, limit).await?;
            if let Some(arr) = resp["data"].as_array() {
                for hit in arr {
                    let path = hit["path"].as_str().unwrap_or("?");
                    let score = hit["score"].as_f64().unwrap_or(0.0);
                    let content = hit["content"].as_str().unwrap_or("").chars().take(80).collect::<String>();
                    println!("{score:.3}\t{path}\t{content}");
                }
            }
        }
        Commands::Collection { action } => match action {
            CollectionCmd::Create { name, schema, embed_source } => {
                let schema_val: serde_json::Value = serde_json::from_str(&schema)?;
                let resp = c.create_collection(cfg.ws_key()?, &name, &schema_val, embed_source.as_deref()).await?;
                println!("Collection created: {}", resp["data"]["id"].as_str().unwrap_or(&name));
            }
            CollectionCmd::List => {
                let resp = c.list_collections(cfg.ws_key()?).await?;
                if let Some(arr) = resp["data"].as_array() {
                    for coll in arr {
                        println!("{}\t{}", coll["name"].as_str().unwrap_or(""), coll["status"].as_str().unwrap_or(""));
                    }
                }
            }
            CollectionCmd::Desc { name } => {
                let resp = c.describe_collection(cfg.ws_key()?, &name).await?;
                let data = &resp["data"];
                println!("Name:       {}", data["name"].as_str().unwrap_or(""));
                println!("ID:         {}", data["id"].as_str().unwrap_or(""));
                println!("Type:       {}", data["collection_type"].as_str().unwrap_or(""));
                println!("Status:     {}", data["status"].as_str().unwrap_or(""));
                println!("Embed Src:  {}", data["embedding_source"].as_str().unwrap_or("-"));
                println!("Embed Dim:  {}", data["embedding_dim"].as_i64().map(|d| d.to_string()).unwrap_or("-".into()));
                if let Some(fields) = data["schema_json"].as_array() {
                    println!("Fields:");
                    for f in fields {
                        let fname = f["name"].as_str().unwrap_or("?");
                        let ftype = f["field_type"].as_str().or_else(|| f["type"].as_str()).unwrap_or("?");
                        let idx = if f["index"].as_bool().unwrap_or(false) { " [indexed]" } else { "" };
                        let emb = if f["embed"].as_bool().unwrap_or(false) { " [embed]" } else { "" };
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
                let resp = c.search_collection(cfg.ws_key()?, &name, &query, limit).await?;
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
                println!("api_key: {}", cfg.api_key.as_deref().unwrap_or("<not set>"));
                println!("workspace_id: {}", cfg.workspace_id.as_deref().unwrap_or("<not set>"));
                println!("workspace_key: {}", if cfg.workspace_key.is_some() { "<set>" } else { "<not set>" });
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

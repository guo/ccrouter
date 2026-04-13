mod config;
mod handler;
mod server;
mod setup;
mod stream;
mod transform;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};
use tracing::info;

#[derive(Parser)]
#[command(name = "ccrouter", about = "Lightweight CLI proxy: route Claude Code to any LLM provider")]
#[command(version)]
struct Cli {
    /// Path to config file (default: ./ccrouter.toml or ~/.config/ccrouter/config.toml)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the proxy server (foreground)
    Start {
        /// Override listen port
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Wrap a command: proxy to ANTHROPIC_BASE_URL from env, no config file needed.
    ///
    /// Example:
    ///   ANTHROPIC_BASE_URL="https://my.gateway.com" ANTHROPIC_AUTH_TOKEN="sk-..." ccrouter run -- claude .
    Run {
        /// Force OpenAI format transform (default: anthropic pass-through)
        #[arg(long)]
        openai: bool,
        /// Command and arguments to run (default: claude)
        #[arg(last = true, default_values = ["claude"])]
        command: Vec<String>,
    },

    /// Switch the active profile (hot-reloads if proxy is running)
    Switch {
        /// Profile ID to activate
        profile: String,
    },

    /// Show proxy status and active profile
    Status,

    /// List all configured profiles
    List,

    /// Configure Claude Code to use ccrouter (writes to ~/.claude/settings.json)
    Setup {
        /// Override port (default: port from config)
        #[arg(short, long)]
        port: Option<u16>,
        /// Undo: remove ccrouter from Claude Code settings
        #[arg(long)]
        undo: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(config::find_config_path);
    load_dotenv_for_config(&config_path)?;

    match cli.command {
        Command::Start { port } => cmd_start(config_path, port).await,
        Command::Run { openai, command } => cmd_run(openai, command).await,
        Command::Switch { profile } => cmd_switch(config_path, profile),
        Command::Status => cmd_status(config_path),
        Command::List => cmd_list(config_path),
        Command::Setup { port, undo } => cmd_setup(config_path, port, undo),
    }
}

async fn cmd_start(config_path: PathBuf, port_override: Option<u16>) -> Result<()> {
    if !config_path.exists() {
        anyhow::bail!(
            "Config file not found: {}\n\nCreate one with:\n  {}",
            config_path.display(),
            example_config_hint()
        );
    }

    let mut cfg = config::load_config(&config_path)?;

    if let Some(port) = port_override {
        cfg.proxy.port = port;
    }

    let log_level = cfg.proxy.log_level.clone();
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| format!("ccrouter={}", log_level)),
        )
        .init();

    match cfg.active_profile() {
        Some(p) => info!("Active profile: {} — {} ({:?})", p.id, p.base_url, p.format),
        None => anyhow::bail!(
            "Active profile '{}' not found in config. Run `ccrouter list` to see available profiles.",
            cfg.active.profile
        ),
    }

    let (tx, rx) = mpsc::channel::<config::Config>(16);
    config::watch_config(config_path, tx)?;
    server::run(cfg, rx, None).await?;
    Ok(())
}

/// Spin up an ephemeral proxy using ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN from the
/// environment, run the given command with the proxy URL injected, then shut down.
async fn cmd_run(openai: bool, command: Vec<String>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "ccrouter=info".into()))
        .init();

    // Read provider from current environment
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .unwrap_or_default();

    let format = if openai {
        config::ApiFormat::OpenAI
    } else {
        config::ApiFormat::Anthropic
    };

    info!("Inline provider: {} ({:?})", base_url, format);

    // Find a free port
    let port = free_port()?;
    let proxy_url = format!("http://127.0.0.1:{}", port);

    // Build an ephemeral single-profile config
    let profile = config::Profile {
        id: "inline".to_string(),
        name: format!("inline → {}", base_url),
        base_url,
        api_key_env: String::new(),
        api_key_direct: if api_key.is_empty() { None } else { Some(api_key) },
        format,
        model_map: config::ModelMap::default(),
    };

    let cfg = config::Config {
        proxy: config::ProxySettings {
            port,
            host: "127.0.0.1".to_string(),
            log_level: "info".to_string(),
        },
        active: config::ActiveConfig { profile: "inline".to_string() },
        profiles: vec![profile],
    };

    // Shutdown channel: fires when the child process exits
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    // Spawn proxy in background
    let (_config_tx, config_rx) = mpsc::channel::<config::Config>(1);
    tokio::spawn(async move {
        if let Err(e) = server::run(cfg, config_rx, Some(shutdown_rx)).await {
            eprintln!("ccrouter proxy error: {}", e);
        }
    });

    // Give the server a moment to bind
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    info!("Proxy ready on {} — launching: {}", proxy_url, command.join(" "));

    // Run the child command with the proxy URL injected
    let (prog, args) = command.split_first().expect("command is non-empty");
    let status = tokio::process::Command::new(prog)
        .args(args)
        .env("ANTHROPIC_BASE_URL", &proxy_url)
        .env("ANTHROPIC_AUTH_TOKEN", "ccrouter-managed")
        // Clear conflicting key so Claude Code uses the token we set
        .env_remove("ANTHROPIC_API_KEY")
        .status()
        .await?;

    // Shut down the proxy
    let _ = shutdown_tx.send(());

    // Propagate the child's exit code
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Bind to port 0 to get a free port from the OS, then release it.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn cmd_switch(config_path: PathBuf, profile_id: String) -> Result<()> {
    let cfg = config::load_config(&config_path)?;

    if !cfg.profiles.iter().any(|p| p.id == profile_id) {
        let ids: Vec<_> = cfg.profiles.iter().map(|p| p.id.as_str()).collect();
        anyhow::bail!(
            "Profile '{}' not found. Available: {}",
            profile_id,
            ids.join(", ")
        );
    }

    config::write_active_profile(&config_path, &profile_id)?;
    println!("Switched to profile: {}", profile_id);
    println!("If the proxy is running, it will hot-reload within a second.");
    Ok(())
}

fn cmd_status(config_path: PathBuf) -> Result<()> {
    let cfg = config::load_config(&config_path)?;
    println!("Config: {}", config_path.display());
    println!("Active profile: {}", cfg.active.profile);
    println!("Proxy: {}:{}", cfg.proxy.host, cfg.proxy.port);

    match cfg.active_profile() {
        Some(p) => {
            println!("\nProvider:");
            println!("  Name:     {}", p.name);
            println!("  Base URL: {}", p.base_url);
            println!("  Format:   {:?}", p.format);
            let key_status = if p.api_key_env.is_empty() {
                "none".to_string()
            } else {
                match std::env::var(&p.api_key_env) {
                    Ok(k) if !k.is_empty() => format!("{} (set)", p.api_key_env),
                    _ => format!("{} (NOT SET)", p.api_key_env),
                }
            };
            println!("  API key:  {}", key_status);
        }
        None => println!("\nWarning: active profile '{}' not found in config", cfg.active.profile),
    }
    Ok(())
}

fn cmd_list(config_path: PathBuf) -> Result<()> {
    let cfg = config::load_config(&config_path)?;
    println!("Profiles ({}):\n", cfg.profiles.len());

    for p in &cfg.profiles {
        let active_marker = if p.id == cfg.active.profile { " <- active" } else { "" };
        println!("  [{}] {}{}", p.id, p.name, active_marker);
        println!("      {} ({:?})", p.base_url, p.format);
        if !p.api_key_env.is_empty() {
            let set = std::env::var(&p.api_key_env).map(|k| !k.is_empty()).unwrap_or(false);
            println!("      API key: {} ({})", p.api_key_env, if set { "set" } else { "NOT SET" });
        }
        println!();
    }
    Ok(())
}

fn cmd_setup(config_path: PathBuf, port_override: Option<u16>, undo: bool) -> Result<()> {
    if undo {
        return setup::deconfigure_claude();
    }

    let port = if let Some(p) = port_override {
        p
    } else if config_path.exists() {
        config::load_config(&config_path)?.proxy.port
    } else {
        15721
    };

    setup::configure_claude(port)?;
    println!("\nNow start the proxy:  ccrouter start");
    Ok(())
}

fn example_config_hint() -> &'static str {
    "cp ccrouter.toml ~/.config/ccrouter/config.toml   (use the example from the repo)"
}

fn load_dotenv_for_config(config_path: &std::path::Path) -> Result<()> {
    let env_path = config_path.with_file_name(".env");
    if !env_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&env_path)?;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');

        if key.is_empty() {
            continue;
        }

        if std::env::var_os(key).is_none() {
            unsafe { std::env::set_var(key, value); }
        }
    }

    Ok(())
}

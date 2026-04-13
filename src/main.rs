mod config;
mod handler;
mod server;
mod setup;
mod stream;
mod transform;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tokio::sync::mpsc;
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

    let config_path = cli
        .config
        .unwrap_or_else(config::find_config_path);

    match cli.command {
        Command::Start { port } => cmd_start(config_path, port).await,
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

    // Init tracing
    let log_level = cfg.proxy.log_level.clone();
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| format!("ccrouter={}", log_level)),
        )
        .init();

    match cfg.active_profile() {
        Some(p) => info!("Active profile: {} — {} ({:?})", p.id, p.base_url, p.format),
        None => {
            anyhow::bail!(
                "Active profile '{}' not found in config. Run `ccrouter list` to see available profiles.",
                cfg.active.profile
            );
        }
    }

    let (tx, rx) = mpsc::channel::<config::Config>(16);

    config::watch_config(config_path, tx)?;
    server::run(cfg, rx).await?;
    Ok(())
}

fn cmd_switch(config_path: PathBuf, profile_id: String) -> Result<()> {
    let cfg = config::load_config(&config_path)?;

    // Validate profile exists
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

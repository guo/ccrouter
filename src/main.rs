mod config;
mod daemon;
mod handler;
mod init;
mod responses;
mod server;
mod setup;
mod stream;
mod transform;

use anyhow::Result;
use clap::{Parser, Subcommand};
use daemon::{DaemonState, StopOutcome};
use std::path::PathBuf;
use std::time::Duration;
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
    /// Start the proxy server (foreground, or background with -d)
    Start {
        /// Override listen port
        #[arg(short, long)]
        port: Option<u16>,
        /// Run as a background daemon
        #[arg(short, long)]
        daemon: bool,
        /// Internal: marks the daemon worker process (hidden)
        #[arg(long, hide = true)]
        daemon_child: bool,
    },

    /// Wrap a command: proxy to ANTHROPIC_BASE_URL from env, no config file needed.
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

    /// Stop the background daemon
    Stop,

    /// Restart the background daemon
    Restart,

    /// Configure Claude Code to use ccrouter (writes to ~/.claude/settings.json)
    Setup {
        /// Override port (default: port from config)
        #[arg(short, long)]
        port: Option<u16>,
        /// Undo: remove ccrouter from Claude Code settings
        #[arg(long)]
        undo: bool,
    },

    /// Generate a starter config file at ~/.config/ccrouter/config.toml
    Init {
        /// Overwrite existing config
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(config::find_config_path);
    load_dotenv_for_config(&config_path)?;

    match cli.command {
        Command::Start { port, daemon, daemon_child } => {
            if daemon_child {
                cmd_start_child(config_path, port).await
            } else if daemon {
                cmd_start_daemon(config_path, port)
            } else {
                cmd_start_foreground(config_path, port).await
            }
        }
        Command::Run { openai, command } => cmd_run(openai, command).await,
        Command::Switch { profile } => cmd_switch(config_path, profile),
        Command::Status => cmd_status(config_path).await,
        Command::List => cmd_list(config_path),
        Command::Stop => cmd_stop(),
        Command::Restart => cmd_restart(config_path),
        Command::Setup { port, undo } => cmd_setup(config_path, port, undo),
        Command::Init { force } => init::cmd_init(force),
    }
}

// ── start: foreground ──────────────────────────────────────────────────

async fn cmd_start_foreground(config_path: PathBuf, port_override: Option<u16>) -> Result<()> {
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

    log_active_profile(&cfg);

    let (tx, rx) = mpsc::channel::<config::Config>(16);
    config::watch_config(config_path, tx)?;
    server::run(cfg, rx, None).await?;
    Ok(())
}

// ── start: daemon parent (spawns child) ────────────────────────────────

fn cmd_start_daemon(config_path: PathBuf, port_override: Option<u16>) -> Result<()> {
    if !config_path.exists() {
        anyhow::bail!(
            "Config file not found: {}\n\nCreate one with:\n  {}",
            config_path.display(),
            example_config_hint()
        );
    }

    // Pre-validate config before spawning
    let cfg = config::load_config(&config_path)?;
    if let Some(ref p) = cfg.active_profile() {
        println!("Starting daemon — {} ({:?})", p.name, p.format);
    }

    // Check if already running
    if daemon::check_and_clean_stale() {
        let pid = daemon::read_pid().unwrap();
        anyhow::bail!("ccrouter is already running (pid {})", pid);
    }

    let _ = port_override.unwrap_or(cfg.proxy.port);
    daemon::touch_log()?;

    let pid = daemon::spawn_detached(&config_path, port_override)
        .map_err(|e| anyhow::anyhow!("Failed to start daemon: {}", e))?;

    println!("Waiting for daemon (pid {}) to start...", pid);

    match daemon::wait_for_ready(Duration::from_secs(3)) {
        Some(state) => {
            println!(
                "ccrouter started (pid {}) — http://{}:{}",
                state.pid, state.host, state.port
            );
            println!("  config: {}", config_path.display());
            println!("  log:    {}", daemon::log_path().display());
            Ok(())
        }
        None => {
            eprintln!(
                "Daemon did not become ready in time. Check log:\n  {}",
                daemon::log_path().display()
            );
            std::process::exit(1);
        }
    }
}

// ── start: daemon child (actual worker) ────────────────────────────────

async fn cmd_start_child(config_path: PathBuf, port_override: Option<u16>) -> Result<()> {
    let mut cfg = config::load_config(&config_path)?;
    if let Some(port) = port_override {
        cfg.proxy.port = port;
    }

    let log_level = cfg.proxy.log_level.clone();

    // Init tracing to the daemon log file
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(daemon::log_path())?;
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| format!("ccrouter={}", log_level)),
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .init();

    log_active_profile(&cfg);

    let pid = std::process::id();
    daemon::write_pid_exclusive(pid)?;

    let host = cfg.proxy.host.clone();
    let port = cfg.proxy.port;
    let config_path_str = config_path.to_string_lossy().to_string();
    let log_path_str = daemon::log_path().to_string_lossy().to_string();

    // Shutdown channel: SIGTERM / SIGINT → graceful shutdown
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    install_signal_handlers(shutdown_tx);

    let (config_tx, config_rx) = mpsc::channel::<config::Config>(16);
    config::watch_config(config_path.clone(), config_tx)?;

    let state_for_cleanup = DaemonState {
        pid,
        port,
        host: host.clone(),
        started_at: daemon::now_secs(),
        config_path: config_path_str,
        log_path: log_path_str.clone(),
    };

    info!("Daemon child started (pid {})", pid);

    let on_ready = {
        let s = state_for_cleanup.clone();
        move || {
            if let Err(e) = daemon::write_state(&s) {
                tracing::error!("Failed to write daemon state: {}", e);
            }
        }
    };

    let result = server::run_with_ready(cfg, config_rx, Some(shutdown_rx), Some(on_ready)).await;

    // Cleanup on exit
    info!("Daemon shutting down");
    daemon::remove_runtime_files();

    result
}

#[cfg(unix)]
fn install_signal_handlers(shutdown_tx: oneshot::Sender<()>) {
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ).expect("Cannot install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        ).expect("Cannot install SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => info!("Received SIGTERM"),
            _ = sigint.recv() => info!("Received SIGINT"),
        }
        let _ = shutdown_tx.send(());
    });
}

#[cfg(not(unix))]
fn install_signal_handlers(shutdown_tx: oneshot::Sender<()>) {
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let _ = shutdown_tx.send(());
    });
}

// ── stop ───────────────────────────────────────────────────────────────

fn cmd_stop() -> Result<()> {
    match daemon::stop_daemon(Duration::from_secs(5))? {
        StopOutcome::NotRunning => {
            println!("ccrouter is not running.");
        }
        StopOutcome::Stale => {
            println!("Cleaned up stale pid file (process was already gone).");
        }
        StopOutcome::Stopped(pid) => {
            println!("ccrouter stopped (pid {}).", pid);
        }
        StopOutcome::Timeout(pid) => {
            eprintln!(
                "ccrouter (pid {}) did not exit in time. Check log:\n  {}",
                pid,
                daemon::log_path().display()
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

// ── restart ────────────────────────────────────────────────────────────

fn cmd_restart(config_path: PathBuf) -> Result<()> {
    // Capture prior port before stopping so we can preserve it across restart.
    let prior_port = daemon::read_state().map(|s| s.port);

    match daemon::stop_daemon(Duration::from_secs(5)) {
        Ok(StopOutcome::Stopped(pid)) => println!("Stopped previous instance (pid {}).", pid),
        Ok(StopOutcome::Stale) => println!("Cleaned up stale pid file."),
        Ok(StopOutcome::NotRunning) => println!("No previous instance running."),
        Ok(StopOutcome::Timeout(pid)) => {
            eprintln!("Previous instance (pid {}) did not exit in time.", pid);
            std::process::exit(1);
        }
        Err(e) => anyhow::bail!("Stop failed: {}", e),
    }

    cmd_start_daemon(config_path, prior_port)
}

// ── status ─────────────────────────────────────────────────────────────

async fn cmd_status(config_path: PathBuf) -> Result<()> {
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
            if matches!(p.format, config::ApiFormat::Anthropic) {
                println!("  Auth:     {:?}", p.auth_mode);
                println!("  Messages: {}", p.messages_path);
                println!("  Count:    {}", p.count_tokens_path);
            }
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

    // Daemon state
    println!("\nDaemon:");
    match daemon::read_state() {
        Some(state) if daemon::is_alive(state.pid) => {
            let started = chrono_from_epoch(state.started_at);
            println!("  Status:     running");
            println!("  PID:        {}", state.pid);
            println!("  Started:    {}", started);
            println!("  Log:        {}", state.log_path);

            // Health probe
            let url = format!("http://{}:{}/health", state.host, state.port);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build();
            match client {
                Ok(c) => match c.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => println!("  Health:     ok"),
                    Ok(resp) => println!("  Health:     {}", resp.status()),
                    Err(e) => println!("  Health:     unreachable ({})", e),
                },
                Err(e) => println!("  Health:     client build failed ({})", e),
            }
        }
        Some(state) => {
            println!("  Status:     stale pid file (pid {} gone)", state.pid);
            println!("  Log:        {}", state.log_path);
            daemon::remove_runtime_files();
        }
        None => match daemon::read_pid() {
            Some(pid) if daemon::is_alive(pid) => {
                println!("  Status:     running (pid {}) — state file missing", pid);
            }
            Some(_) => {
                println!("  Status:     stale pid file");
                daemon::remove_runtime_files();
            }
            None => println!("  Status:     not running"),
        },
    }

    Ok(())
}

fn chrono_from_epoch(secs: u64) -> String {
    // Simple UTC timestamp without pulling in chrono
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    // Approximate year/day calculation (good enough for display)
    let mut remaining_days = days as i64;
    let mut year = 1970i64;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        year += 1;
    }
    let month_days = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md {
            month = i;
            break;
        }
        remaining_days -= md;
    }
    let day = remaining_days + 1;

    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", year, month + 1, day, h, m, s)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ── list ───────────────────────────────────────────────────────────────

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

// ── run ────────────────────────────────────────────────────────────────

/// Spin up an ephemeral proxy using ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN from the
/// environment, run the given command with the proxy URL injected, then shut down.
async fn cmd_run(openai: bool, command: Vec<String>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "ccrouter=info".into()))
        .init();

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

    let port = free_port()?;
    let proxy_url = format!("http://127.0.0.1:{}", port);

    let profile = config::Profile {
        id: "inline".to_string(),
        name: format!("inline → {}", base_url),
        base_url,
        api_key_env: String::new(),
        format,
        model_map: config::ModelMap::default(),
        auth_mode: config::AnthropicAuthMode::Both,
        messages_path: "/v1/messages".to_string(),
        count_tokens_path: "/v1/messages/count_tokens".to_string(),
        inject_claude_code_beta: true,
        api_key_direct: if api_key.is_empty() { None } else { Some(api_key) },
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

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (_config_tx, config_rx) = mpsc::channel::<config::Config>(1);
    tokio::spawn(async move {
        if let Err(e) = server::run(cfg, config_rx, Some(shutdown_rx)).await {
            eprintln!("ccrouter proxy error: {}", e);
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    info!("Proxy ready on {} — launching: {}", proxy_url, command.join(" "));

    let (prog, args) = command.split_first().expect("command is non-empty");
    let status = tokio::process::Command::new(prog)
        .args(args)
        .env("ANTHROPIC_BASE_URL", &proxy_url)
        .env("ANTHROPIC_AUTH_TOKEN", "ccrouter-managed")
        .env_remove("ANTHROPIC_API_KEY")
        .status()
        .await?;

    let _ = shutdown_tx.send(());

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

// ── switch ─────────────────────────────────────────────────────────────

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

// ── setup ──────────────────────────────────────────────────────────────

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

// ── helpers ────────────────────────────────────────────────────────────

fn log_active_profile(cfg: &config::Config) {
    match cfg.active_profile() {
        Some(p) => {
            info!("Active profile: {} — {} ({:?})", p.id, p.name, p.format);
            if matches!(p.format, config::ApiFormat::Anthropic) {
                info!(
                    "Anthropic transport: auth_mode={:?}, messages_path={}, count_tokens_path={}, inject_claude_code_beta={}",
                    p.auth_mode,
                    p.messages_path,
                    p.count_tokens_path,
                    p.inject_claude_code_beta,
                );
            }
        }
        None => {
            tracing::error!(
                "Active profile '{}' not found in config.",
                cfg.active.profile
            );
        }
    }
}

fn example_config_hint() -> &'static str {
    "ccrouter init   (generates a starter config at ~/.config/ccrouter/config.toml)"
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
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

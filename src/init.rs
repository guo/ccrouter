use anyhow::{Context, Result};
use std::path::PathBuf;

/// Minimal starter config — enough to get going with Anthropic pass-through.
const STARTER_CONFIG: &str = r#"# ccrouter config
# Full example with more providers: https://github.com/guo/ccrouter/blob/master/ccrouter.toml

[proxy]
port = 15721
host = "127.0.0.1"

[active]
profile = "anthropic"

[[profiles]]
id = "anthropic"
name = "Official Anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
format = "anthropic"
"#;

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{}/.config/ccrouter", home))
}

pub fn cmd_init(force: bool) -> Result<()> {
    let dir = config_dir();
    let path = dir.join("config.toml");

    if path.exists() && !force {
        println!("Config already exists: {}", path.display());
        println!("Use --force to overwrite.");
        return Ok(());
    }

    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create directory: {}", dir.display()))?;

    std::fs::write(&path, STARTER_CONFIG)
        .with_context(|| format!("Cannot write {}", path.display()))?;

    println!("Created {}\n", path.display());
    println!("Next steps:");
    println!("  1. Set your API key:");
    println!("       export ANTHROPIC_API_KEY=sk-ant-...");
    println!("     Or save it to {}/.env", dir.display());
    println!("  2. Start the proxy:");
    println!("       ccrouter start -d");
    println!("  3. Point Claude Code at ccrouter:");
    println!("       ccrouter setup");
    println!();
    println!("More providers: https://github.com/guo/ccrouter/blob/master/ccrouter.toml");

    Ok(())
}

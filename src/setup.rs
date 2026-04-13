use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use tracing::info;

pub fn claude_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{}/.claude/settings.json", home))
}

/// Write ANTHROPIC_BASE_URL into ~/.claude/settings.json so Claude Code uses ccrouter.
pub fn configure_claude(port: u16) -> Result<()> {
    let path = claude_settings_path();

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory: {}", parent.display()))?;
    }

    // Load existing settings or start fresh
    let mut settings: Value = if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Cannot read {}", path.display()))?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Merge in the base URL
    let base_url = format!("http://127.0.0.1:{}", port);
    settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json root is not an object"))?
        .entry("env")
        .or_insert(json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("'env' is not an object"))?
        .insert("ANTHROPIC_BASE_URL".to_string(), json!(base_url));

    // Atomic write
    let tmp = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &path)?;

    info!(
        "Configured Claude Code to use ccrouter: {} → {}",
        path.display(),
        base_url
    );
    println!("✓ Claude Code configured: ANTHROPIC_BASE_URL = {}", base_url);
    println!("  Settings file: {}", path.display());
    Ok(())
}

/// Remove ccrouter's ANTHROPIC_BASE_URL from ~/.claude/settings.json.
pub fn deconfigure_claude() -> Result<()> {
    let path = claude_settings_path();
    if !path.exists() {
        println!("Settings file not found, nothing to undo.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&path)?;
    let mut settings: Value = serde_json::from_str(&content).unwrap_or(json!({}));

    if let Some(env) = settings.get_mut("env").and_then(|e| e.as_object_mut()) {
        env.remove("ANTHROPIC_BASE_URL");
    }

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&settings)?)?;
    std::fs::rename(&tmp, &path)?;

    println!("✓ Removed ANTHROPIC_BASE_URL from {}", path.display());
    Ok(())
}

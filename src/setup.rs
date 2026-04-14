use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use tracing::info;

pub fn claude_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{}/.claude/settings.json", home))
}

/// Write ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN into ~/.claude/settings.json
/// so Claude Code routes through ccrouter. The auth token is a placeholder — ccrouter
/// ignores it and uses the real credential from the active profile's `api_key_env`.
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

    let base_url = format!("http://127.0.0.1:{}", port);
    let env = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json root is not an object"))?
        .entry("env")
        .or_insert(json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("'env' is not an object"))?;

    env.insert("ANTHROPIC_BASE_URL".to_string(), json!(base_url));
    // Claude Code requires *some* token to be set even when the upstream doesn't need it.
    // ccrouter ignores this value and uses the active profile's api_key_env instead.
    env.insert(
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        json!(CCROUTER_PLACEHOLDER_TOKEN),
    );

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

/// Placeholder token written by `ccrouter setup`. We only remove tokens matching this
/// value on `--undo`, so user-managed tokens aren't clobbered.
const CCROUTER_PLACEHOLDER_TOKEN: &str = "ccrouter-managed";

/// Remove ccrouter's entries from ~/.claude/settings.json.
/// Removes `env.ANTHROPIC_BASE_URL` pointing at localhost, and `env.ANTHROPIC_AUTH_TOKEN`
/// only if it equals the ccrouter placeholder (to avoid clobbering a user's real token).
/// Cleans up the `env` object entirely if it's left empty.
pub fn deconfigure_claude() -> Result<()> {
    let path = claude_settings_path();
    if !path.exists() {
        println!("Settings file not found, nothing to undo.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&path)?;
    let mut settings: Value = serde_json::from_str(&content).unwrap_or(json!({}));

    let mut removed_url = false;
    let mut removed_token = false;
    let mut kept_user_token = false;

    if let Some(root) = settings.as_object_mut() {
        if let Some(env) = root.get_mut("env").and_then(|e| e.as_object_mut()) {
            // Always remove the base URL — setup is the only thing that writes it.
            if env.remove("ANTHROPIC_BASE_URL").is_some() {
                removed_url = true;
            }
            // Only remove the token if it matches the placeholder ccrouter itself wrote.
            let should_remove_token = env
                .get("ANTHROPIC_AUTH_TOKEN")
                .and_then(|v| v.as_str())
                .map(|s| s == CCROUTER_PLACEHOLDER_TOKEN)
                .unwrap_or(false);
            if should_remove_token {
                env.remove("ANTHROPIC_AUTH_TOKEN");
                removed_token = true;
            } else if env.contains_key("ANTHROPIC_AUTH_TOKEN") {
                kept_user_token = true;
            }

            // If env is now empty, drop it entirely.
            if env.is_empty() {
                root.remove("env");
            }
        }
    }

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&settings)?)?;
    std::fs::rename(&tmp, &path)?;

    match (removed_url, removed_token) {
        (false, false) => println!("Nothing to undo in {}", path.display()),
        _ => println!("✓ Removed ccrouter entries from {}", path.display()),
    }
    if kept_user_token {
        println!(
            "  Kept env.ANTHROPIC_AUTH_TOKEN (value doesn't match ccrouter's placeholder — \
             appears user-managed). Remove it manually if you want it gone."
        );
    }
    Ok(())
}

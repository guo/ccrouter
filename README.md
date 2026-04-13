# ccrouter

Lightweight CLI proxy that routes [Claude Code](https://claude.ai/code) to any LLM provider — OpenAI, Ollama, OpenRouter, Groq, DeepSeek, or any OpenAI-compatible endpoint. Switch providers instantly by editing one config file or running a single command.

## How it works

Claude Code reads `ANTHROPIC_BASE_URL` from `~/.claude/settings.json`. ccrouter listens on that address and either:

- **Pass-through** (`format = "anthropic"`): forwards the request as-is with your API key injected
- **Transform** (`format = "openai"`): converts the Anthropic Messages format → OpenAI Chat Completions, forwards it, and converts the response (including streaming SSE) back to Anthropic format

```
Claude Code → ccrouter (localhost:15721) → any LLM provider
```

## Install

**One-liner (macOS / Linux):**
```bash
curl -fsSL https://raw.githubusercontent.com/guo/ccrouter/master/install.sh | sh
```

**Manual download** — grab the binary for your platform from [Releases](https://github.com/guo/ccrouter/releases):

| Platform | Binary |
|---|---|
| macOS (Apple Silicon + Intel universal) | `ccrouter-*-universal-apple-darwin.tar.gz` |
| Linux x86_64 (static) | `ccrouter-*-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 (static) | `ccrouter-*-aarch64-unknown-linux-musl.tar.gz` |

**Build from source:**
```bash
git clone https://github.com/guo/ccrouter
cd ccrouter
cargo build --release
cp target/release/ccrouter ~/.local/bin/
```

## Quick start

```bash
# 1. Copy the example config
mkdir -p ~/.config/ccrouter
cp ccrouter.toml ~/.config/ccrouter/config.toml

# 2. Set your API keys
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

# 3. Start the proxy in the background
ccrouter start &
```

Then pick one of the two ways to point Claude Code at ccrouter:

### Option 1 — `ccrouter setup` (persistent)

Writes `ANTHROPIC_BASE_URL` into `~/.claude/settings.json` so every future `claude` invocation routes through ccrouter.

```bash
ccrouter setup
claude "hello"            # now routes through ccrouter
ccrouter setup --undo     # remove when you want stock Claude Code back
```

### Option 2 — inline env vars (temporary, no config touched)

Override just for this invocation. Your `~/.claude/settings.json` stays untouched, so you can flip between ccrouter and stock Claude Code freely.

```bash
ANTHROPIC_BASE_URL="http://localhost:15721" \
ANTHROPIC_AUTH_TOKEN="anything" \
  claude "hello"
```

The token can be any non-empty string — ccrouter ignores it and uses the real credential from the active profile's `api_key_env`. Claude Code just requires *something* to be set.

## Commands

```
ccrouter start              Start proxy (foreground)
ccrouter switch <profile>   Switch active provider (hot-reload, no restart)
ccrouter status             Show current provider and config
ccrouter list               List all configured profiles
ccrouter setup              Write ANTHROPIC_BASE_URL to ~/.claude/settings.json
ccrouter setup --undo       Remove ccrouter from Claude Code settings
```

## Config file

Location: `~/.config/ccrouter/config.toml` (or `./ccrouter.toml` in current directory)

```toml
[proxy]
port = 15721
host = "127.0.0.1"
log_level = "info"   # trace | debug | info | warn | error

[active]
profile = "openai"   # edit this (or run `ccrouter switch`) to change provider

[[profiles]]
id = "anthropic"
name = "Official Anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
format = "anthropic"  # pass-through, no transformation

[[profiles]]
id = "openai"
name = "OpenAI GPT"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
format = "openai"

[profiles.model_map]
"claude-3-5-sonnet-20241022" = "gpt-4o"
"claude-3-haiku-20240307"    = "gpt-4o-mini"
default_model                = "gpt-4o"

[[profiles]]
id = "ollama"
name = "Local Ollama"
base_url = "http://localhost:11434/v1"
api_key_env = ""   # no auth needed
format = "openai"

[profiles.model_map]
default_model = "qwen2.5-coder:32b"
```

See `ccrouter.toml` in this repo for a full example with OpenRouter, Groq, and DeepSeek profiles.

## Hot-reload

Edit `[active] profile` in the config file while the proxy is running — ccrouter detects the change within a second and switches providers without restarting. Or use the CLI:

```bash
ccrouter switch ollama    # switch to local Ollama instantly
ccrouter switch openai    # switch back to OpenAI
```

## Supported providers

| Profile | Format | Notes |
|---|---|---|
| Anthropic | `anthropic` | Pass-through, no transform |
| OpenAI | `openai` | Full request/response transform + streaming |
| Ollama | `openai` | Local models via OpenAI-compatible API |
| OpenRouter | `openai` | Any model via OpenRouter |
| Groq | `openai` | Fast inference |
| DeepSeek | `openai` | DeepSeek models |
| Any OpenAI-compatible | `openai` | Works with any `/v1/chat/completions` endpoint |

## Model mapping

Claude Code always sends Claude model names (e.g. `claude-3-5-sonnet-20241022`). Use `model_map` in each profile to remap them to whatever the provider expects:

```toml
[profiles.model_map]
"claude-3-5-sonnet-20241022" = "gpt-4o"
"claude-3-haiku-20240307"    = "gpt-4o-mini"
"claude-opus-4-5"            = "gpt-4o"
default_model                = "gpt-4o"   # catch-all for unmapped models
```

## Private providers

Add private or internal providers in `ccrouter.local.toml` alongside your main config. This file is gitignored — it is never committed.

```bash
cp ccrouter.local.toml.example ccrouter.local.toml
# edit ccrouter.local.toml with your private endpoints
```

`ccrouter.local.toml` is merged on top of `ccrouter.toml` at load time:
- Profiles with the same `id` replace the base config version
- New profiles are appended
- `[proxy]` and `[active]` in the local file override the base config if present

```toml
# ccrouter.local.toml  (gitignored)
[[profiles]]
id = "internal"
name = "Internal LLM Gateway"
base_url = "https://llm.internal.mycompany.com/v1"
api_key_env = "INTERNAL_LLM_KEY"
format = "openai"

[profiles.model_map]
default_model = "gpt-4o"
```

Hot-reload applies to the local file too — editing either file switches providers live.

## Requirements

- Rust 1.75+ (for building from source)
- Claude Code CLI

## License

MIT

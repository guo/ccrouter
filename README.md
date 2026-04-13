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

```bash
git clone https://github.com/guo/ccrouter
cd ccrouter
cargo build --release
cp target/release/ccrouter ~/.local/bin/   # or any directory in your PATH
```

## Quick start

```bash
# 1. Copy the example config
mkdir -p ~/.config/ccrouter
cp ccrouter.toml ~/.config/ccrouter/config.toml

# 2. Set your API keys
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

# 3. Point Claude Code at ccrouter (one-time setup)
ccrouter setup

# 4. Start the proxy in the background
ccrouter start &

# 5. Use Claude Code normally — it now routes through ccrouter
claude "hello"
```

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

## Requirements

- Rust 1.75+ (for building from source)
- Claude Code CLI

## License

MIT

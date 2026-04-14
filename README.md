# ccrouter

Lightweight CLI proxy that routes [Claude Code](https://claude.ai/code) to any Anthropic-compatible endpoint as a pass-through, or to OpenAI-format providers (OpenAI, Ollama, OpenRouter, Groq, DeepSeek, and similar) via on-the-fly request/response translation. Switch providers by editing one config file or running a single command — the change applies within a second, no restart.

## How it works

ccrouter listens on a local port (default `15721`). Claude Code is pointed at that port via `ANTHROPIC_BASE_URL` — either set inline for a single invocation, or written into `~/.claude/settings.json` by `ccrouter setup`. For each request, ccrouter either:

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

# 3. Start the proxy
ccrouter start -d           # run in background (daemon mode)
# Or foreground: ccrouter start
```

Then pick one of the two ways to point Claude Code at ccrouter:

### Option 1 — `ccrouter setup` (persistent)

Writes the following into `~/.claude/settings.json` → `env` so every future `claude` invocation routes through ccrouter:

- `ANTHROPIC_BASE_URL` — pointing at the ccrouter port
- `ANTHROPIC_AUTH_TOKEN` — placeholder value `"ccrouter-managed"` (Claude Code requires *some* token; ccrouter ignores it and uses the real credential from the active profile's `api_key_env`)

```bash
ccrouter setup
claude "hello"            # now routes through ccrouter
ccrouter setup --undo     # remove when you want stock Claude Code back
```

`--undo` removes `ANTHROPIC_BASE_URL` and the placeholder token. If you'd previously put a real token into `env.ANTHROPIC_AUTH_TOKEN` by hand, `--undo` leaves it alone and prints a notice — delete it yourself if you want it gone.

### Option 2 — inline env vars (temporary, no config touched)

Override just for this invocation. Your `~/.claude/settings.json` stays untouched, so you can flip between ccrouter and stock Claude Code freely.

```bash
ANTHROPIC_BASE_URL="http://localhost:15721" \
ANTHROPIC_AUTH_TOKEN="anything" \
  claude "hello"
```

The token can be any non-empty string — ccrouter ignores it and uses the real credential from the active profile's `api_key_env`. Claude Code just requires *something* to be set.

### Option 3 — `ccrouter run` (ephemeral, no config file needed)

One-shot proxy for when you just want to try a provider without maintaining a config. Reads `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` (or `ANTHROPIC_API_KEY`) from the environment, spins up a proxy on a free port, runs your command with `ANTHROPIC_BASE_URL` overridden to point at that port, and shuts down when the command exits.

```bash
# Anthropic pass-through (default)
ANTHROPIC_BASE_URL="https://my.gateway.com" \
ANTHROPIC_AUTH_TOKEN="sk-..." \
  ccrouter run -- claude "hello"

# OpenAI-format transform
ANTHROPIC_BASE_URL="https://api.openai.com/v1" \
ANTHROPIC_AUTH_TOKEN="sk-..." \
  ccrouter run --openai -- claude "hello"
```

Good for quick experiments, CI, or testing a gateway without touching `~/.claude/settings.json` or any config file.

## Commands

```
ccrouter start              Start proxy (foreground)
ccrouter start -d           Start proxy as a background daemon
ccrouter stop               Stop the daemon
ccrouter restart            Restart the daemon
ccrouter run -- <cmd>       Ephemeral proxy wrapping a single command
ccrouter switch <profile>   Switch active provider (hot-reload, no restart)
ccrouter status             Show current provider, daemon state, and health
ccrouter list               List all configured profiles
ccrouter setup [--port N]   Write ANTHROPIC_BASE_URL + placeholder token to ~/.claude/settings.json
ccrouter setup --undo       Remove ccrouter entries from Claude Code settings
```

### Daemon mode

`ccrouter start -d` runs the proxy as a background daemon. Runtime files are stored in `$XDG_STATE_HOME/ccrouter` (fallback `~/.local/state/ccrouter`):

- `daemon.pid` — process id
- `daemon.json` — pid, port, started_at, config_path, log_path
- `daemon.log` — stdout/stderr output

```bash
ccrouter start -d            # start daemon
ccrouter status              # check daemon state + health probe
ccrouter stop                # graceful shutdown
ccrouter restart             # stop, then start (preserves port)
```

The daemon responds to `SIGTERM`/`SIGINT` for graceful shutdown and hot-reloads config changes within 1 second.

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

### `.env` file (optional)

ccrouter looks for a `.env` file next to the config file and loads any unset variables before starting. So instead of `export`ing keys every session, you can just keep them alongside the config:

```bash
# ~/.config/ccrouter/.env
OPENAI_API_KEY=sk-...
ANTHROPIC_API_KEY=sk-ant-...
GROQ_API_KEY=gsk-...
```

Existing environment variables take precedence, so you can still override any key at invocation time.

### `auth_mode` (Anthropic pass-through)

Controls how ccrouter attaches the API key when forwarding to an Anthropic-format endpoint. Defaults to `both`, which is safe for official Anthropic. Third-party Anthropic-compatible gateways often need one specific mode:

| Value | Header behavior |
|---|---|
| `x_api_key` | Send `x-api-key: <key>` only (official Anthropic) |
| `bearer` | Send `Authorization: Bearer <key>` only (e.g. Z.ai) |
| `both` | Send both (default) |
| `none` | Send neither (the upstream handles auth some other way) |

```toml
[[profiles]]
id = "zai"
name = "Z.ai → GLM"
base_url = "https://api.z.ai/api/anthropic"
api_key_env = "ZAI_API_KEY"
format = "anthropic"
auth_mode = "bearer"
```

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
"claude-opus-4-6"            = "gpt-4o"
"claude-sonnet-4-6"          = "gpt-4o"
"claude-haiku-4-5-20251001"  = "gpt-4o-mini"
default_model                = "gpt-4o"   # catch-all for unmapped models
```

## Private providers

Add private or internal providers in `ccrouter.local.toml` alongside your main config. This repo's `.gitignore` already excludes it so you can safely keep it in a cloned tree; if you place it elsewhere, add it to your own ignore rules.

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

- Rust 1.75+ (only needed when building from source; see `rust-version` in `Cargo.toml`)
- Claude Code CLI

## License

MIT

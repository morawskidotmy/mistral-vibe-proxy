# mistral-vibe-proxy

A small transparent HTTP proxy that exposes Mistral's OpenAI-compatible API using the key stored by [Mistral Vibe](https://mistral.ai/products/vibe). If `vibe` works on your machine, this proxy works — you don't need to copy a key.

Written in Rust (axum + reqwest).

## What it does

- **Key resolution, mirroring `vibe` itself** (env wins, re-read every 60 s):
  1. `MISTRAL_API_KEY` in the proxy's environment
  2. `MISTRAL_API_KEY` in `$VIBE_HOME/.env` (default `~/.vibe/.env`)
  3. OS keyring via `secret-tool`: service `ai.mistral.vibe` (legacy `vibe`), account `MISTRAL_API_KEY`
- **Transparent forwarding** of every route to the upstream API — method, path, query and JSON body pass through verbatim; responses, including SSE streams, are relayed byte-for-byte.
- **Model alias mapping**: `model` ids matching Vibe's built-in aliases or `[[models]]` entries in `$VIBE_HOME/config.toml` are rewritten to the provider model name; unknown ids pass through untouched.
- **`/v1/models`** merges Vibe's aliases into the upstream listing and always returns the OpenAI `{"object":"list","data":[...]}` shape.
- **`x-affinity`**: requests carry an affinity id (taken from the client's `x-affinity` / `x-opencode-session-id` header, or generated) so related requests reuse upstream prompt caching.
- **Context-overflow rewriting**: upstream `400` responses of the form `Prompt N > M maximum context length` are rewritten into the common `context_length_exceeded` form, so clients (e.g. OpenCode) recognize them and can compact + retry.

## Usage

```sh
cargo build --release
./target/release/vibe-proxy
```

Then point any OpenAI-compatible client at `http://127.0.0.1:6933/v1`.

```sh
curl http://127.0.0.1:6933/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"vibe-default","messages":[{"role":"user","content":"hello"}]}'
```

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `VIBE_PROXY_HOST` | `127.0.0.1` | Bind address |
| `VIBE_PROXY_PORT` | `6933` | Listen port |
| `VIBE_PROXY_API_BASE` | `https://api.mistral.ai/v1` | Upstream base URL |
| `VIBE_PROXY_TIMEOUT` | `600` | Upstream timeout (seconds) |
| `VIBE_PROXY_API_KEY` | unset | If set, clients must present this key (`Authorization: Bearer` or `x-api-key`) |
| `VIBE_HOME` | `~/.vibe` | Vibe config directory (`.env`, `config.toml`) |
| `MISTRAL_API_KEY` | unset | Overrides keyring/dotenv resolution |

## Credits

Inspired by [thorstenkramm/opencode-mistral-vibe](https://github.com/thorstenkramm/opencode-mistral-vibe), an OpenCode plugin with the same key-handling approach.

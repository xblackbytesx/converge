# Converge

A self-hosted LLM gateway. One OpenAI-compatible endpoint in front of your
local models (llama-swap) and cloud providers (OpenAI, Anthropic, Gemini,
OpenRouter), with per-person API keys and personas: named system prompts
that every client can pick like a model.

[![CI](https://github.com/xblackbytesx/converge/actions/workflows/ci.yml/badge.svg)](https://github.com/xblackbytesx/converge/actions/workflows/ci.yml)
[![Release](https://github.com/xblackbytesx/converge/actions/workflows/release.yml/badge.svg)](https://github.com/xblackbytesx/converge/actions/workflows/release.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![Rust 1.92](https://img.shields.io/badge/Rust-1.92-B7410E?logo=rust&logoColor=white)](rust-toolchain.toml)

## What it does

- **Connections**: the upstreams it proxies to, each with its models. Local
  and cloud side by side, behind one base URL.
- **The OpenAI API, streaming included**: chat completions, the Responses
  API (passed through, or translated over chat for upstreams without it),
  embeddings, completions, rerank, images and speech.
- **Claude from any OpenAI client**: Anthropic connections are translated
  both ways, with tools, thinking and prompt caching.
- **Personas**: a system prompt plus settings, built on a model. Clients see
  `image-prompt-helper` in their model list next to the real models. The
  model behind a persona can be swapped at any time without any client
  noticing.
- **Playground**: chat with any model or persona, up to three side by
  side, with time to first token, tokens per second and token counts on
  every answer. Admins tune a persona as a draft there and save it.
- **People and keys**: admins and users, each user with their own keys and
  only the models and personas granted to them. A key can be limited to
  some of them.
- **Usage and limits**: usage per day, model, persona, key and person, with
  a CSV download. Limits per person: requests per minute, tokens per day
  and parallel requests per key.
- **Metrics**: a Prometheus endpoint on its own internal port.
- **Fast**: nothing on the request path touches the database, and responses
  stream straight through.
- **Private**: prompts and replies are never stored; usage is counts only.

## What it is not

- Not a chat app. The Playground is for trying things out; use any
  OpenAI-compatible client for the real work.
- No response cache, no enterprise tier, no master key login.

## Quick start

```sh
cp .env.example .env        # set POSTGRES_PASSWORD and CONVERGE_SECRET_KEY (make secret)
make up
make admin EMAIL=you@example.com
make password EMAIL=you@example.com
```

Then open `http://localhost:8080`. For a real deployment behind Traefik see
[deploy/README.md](deploy/README.md).

## Development

Everything runs through `make` and Docker; see the top of the
[Makefile](Makefile). Design decisions live in [AGENTS.md](AGENTS.md).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

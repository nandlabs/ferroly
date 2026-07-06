# ferroly

A self-contained Rust toolkit of enterprise utilities — a port of
[nandlabs/golly](https://github.com/nandlabs/golly). Everything ships in this one
crate as feature-gated modules; you compile only what you enable.

```toml
ferroly = { version = "0.1", features = ["genai", "codec"] }
```

```rust
use ferroly::codec::{json, Encode, Decode};
use ferroly::genai::{GenAiRegistry, Message};
```

## Modules (features)

| Feature | Module | What it provides |
|---|---|---|
| `errutils` | `ferroly::errutils` | `MultiError` aggregation |
| `codec` | `ferroly::codec` | `Value` model, `Encode`/`Decode`, JSON/XML/YAML, registry |
| `config` | `ferroly::config` | Layered env/file configuration |
| `fsutils` | `ferroly::fsutils` | Path checks + content-type detection |
| `lifecycle` | `ferroly::lifecycle` | Component start/stop orchestration |
| `clients` | `ferroly::clients` | Retry, circuit breaker, auth providers |
| `genai` | `ferroly::genai` | Provider-agnostic LLM interface + prompt templates |
| `openai`/`claude`/`ollama` | — | GenAI provider implementations |
| `turbo` | `ferroly::turbo` | Lightweight HTTP router |
| `rest` | `ferroly::rest` | HTTP client + server |
| `ws` | `ferroly::ws` | WebSocket client + server |
| `full` | — | Everything |

## Dependencies

The goal is a near dependency-free toolkit. The only external runtime dependencies
are `tokio` (and `tokio-rustls` for HTTPS). Encoding, config, templating,
MIME, and the HTTP/WebSocket stack are all implemented in-house. Derive macros come
from the companion `ferroly-derive` proc-macro crate (build-time only).

Cloud integrations live in separate crates: `ferroly-aws`, `ferroly-gcp`,
`ferroly-vault`.

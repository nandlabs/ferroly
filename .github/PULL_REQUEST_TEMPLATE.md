## Description

<!-- Provide a clear and concise description of your changes. -->

### Related Issue

<!-- IMPORTANT: Please do not create a Pull Request without creating/linking an issue first. -->
<!-- Any change needs to be discussed before proceeding. Failure to do so may result in rejection. -->

Closes #<!-- issue number -->

## Type of Change

<!-- Mark the relevant option with an "x". -->

- [ ] 🐛 Bug fix (non-breaking change that fixes an issue)
- [ ] ✨ New feature (non-breaking change that adds functionality)
- [ ] 💥 Breaking change (fix or feature that would cause existing functionality to change)
- [ ] 📝 Documentation update
- [ ] 🔧 Refactoring (no functional changes)
- [ ] ✅ Test update (adding or modifying tests)
- [ ] 🔨 Build / CI changes

## Affected Module(s)

<!-- Which Cargo feature(s) / crate(s) does this touch? e.g. codec, config, lifecycle, genai, vectorstore, http, turbo, rest, ws, clients, messaging, vfs, log, metrics, auth, ferroly-derive -->

-

## Changes Made

<!-- List the key changes made in this PR. -->

-

## Testing

<!-- Describe the tests you ran to verify your changes. -->
<!-- Make sure all CI checks pass before requesting review. -->

- [ ] I have added/updated unit and/or integration tests for my changes
- [ ] All existing tests pass (`cargo test --features full`)
- [ ] Code is formatted (`cargo fmt --all`) and lint-clean (`cargo clippy --all-targets --features full -- -D warnings`)
- [ ] I have tested this locally

### Test Output

<!-- Paste relevant test output here. -->

```
# cargo test --features full output
```

## Dependency Policy

<!-- Ferroly keeps a near-zero runtime dependency footprint: only tokio, plus rustls/tokio-rustls for TLS. -->

- [ ] This PR adds **no** new runtime dependencies, **or** I have explained and justified any new dependency below and it is already permitted by `deny.toml`
- [ ] `cargo deny check` passes (licenses, advisories, bans)

## Checklist

- [ ] My code follows the project's coding style and conventions
- [ ] I have performed a self-review of my code
- [ ] I have commented my code, particularly in hard-to-understand areas
- [ ] I have updated the documentation accordingly (README, `docs/`, and rustdoc)
- [ ] My changes generate no new warnings or errors
- [ ] I have read the [CONTRIBUTING](../CONTRIBUTING.md) guide
- [ ] I agree my contribution may be redistributed under either the [Apache 2.0](../LICENSE-APACHE) or [MIT](../LICENSE-MIT) license (project is dual-licensed; see [License](../CONTRIBUTING.md#license))

## Additional Context

<!-- Add any other context about the pull request here. -->

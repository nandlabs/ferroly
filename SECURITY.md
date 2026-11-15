# Security Policy

## Supported Versions

Ferroly is pre-1.0, and only the newest release line receives security fixes.

| Version                      | Supported          |
| ---------------------------- | ------------------ |
| 0.6.x (the newest release)   | :white_check_mark: |
| `main`                       | :white_check_mark: |
| 0.5.x and older              | :x:                |

Where fixes land:

- **`main` first.** Every fix is merged to `main` and released in the next
  version, which is tagged `v<version>` in this repository.
- **crates.io from the tag.** A release reaches
  [crates.io](https://crates.io/crates/ferroly) when a maintainer publishes its
  tag; for now that is a manual step, so crates.io can lag behind the tags.
  Check the newest version there against the repository's newest `v*` tag.
  At the time of writing 0.4.0, 0.5.0 and 0.6.0 are tagged but not yet on
  crates.io (which stops at 0.3.2). If crates.io is behind, depend on the tag directly until
  it catches up:
  `ferroly = { git = "https://github.com/nandlabs/ferroly", tag = "v0.6.0" }`.
- **Old crates.io versions are not patched.** In particular, 0.3.2 and earlier
  have known vulnerabilities that were fixed in later releases (see
  `CHANGELOG.md`). Upgrade rather than wait for a backport.

Please check that you are on the newest release, or on `main`, before reporting.

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
discussions, or pull requests.**

Instead, report them privately using GitHub's built-in
[**Report a vulnerability**](https://github.com/nandlabs/ferroly/security/advisories/new)
workflow (Security → Advisories → Report a vulnerability), or email the
maintainers at **security@nandlabs.io**.

Please include, as far as you can:

- The affected module / Cargo feature (e.g. `http`, `ws`, `genai`, `codec`).
- The affected version or git commit.
- A description of the vulnerability and its impact.
- Steps to reproduce, ideally a minimal proof-of-concept.
- Any suggested remediation.

## What to Expect

- **Acknowledgement** of your report within **3 business days**.
- An initial **assessment** and severity triage within **7 business days**.
- Coordinated disclosure: we will work with you on a fix and a disclosure
  timeline, and credit you in the advisory unless you prefer to remain anonymous.

## Scope

Because Ferroly deliberately hand-rolls functionality that is usually delegated
to third-party crates (its own HTTP/1.1 stack, WebSocket implementation, JSON /
XML / YAML codecs, SHA-1 handshake, etc.), the security surface lives largely in
this repository rather than in dependencies. Reports concerning any of these
in-house implementations are especially welcome.

TLS is provided by [`rustls`](https://github.com/rustls/rustls), with the `ring`
crypto provider by default (the `tls-ring` feature) or whichever rustls
`CryptoProvider` the application installs; vulnerabilities in the underlying TLS/crypto stack should be
reported upstream to those projects, though we appreciate a heads-up so we can
bump the pinned versions.

Thank you for helping keep Ferroly and its users safe.

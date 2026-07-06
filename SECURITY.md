# Security Policy

## Supported Versions

Ferroly is pre-1.0. Security fixes are applied to the latest released version on
[crates.io](https://crates.io/crates/ferroly) and the `main` branch. Please make
sure you are on the latest version before reporting.

| Version | Supported          |
| ------- | ------------------ |
| latest  | :white_check_mark: |
| older   | :x:                |

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

TLS is provided by [`rustls`](https://github.com/rustls/rustls) (with the `ring`
crypto provider); vulnerabilities in the underlying TLS/crypto stack should be
reported upstream to those projects, though we appreciate a heads-up so we can
bump the pinned versions.

Thank you for helping keep Ferroly and its users safe.

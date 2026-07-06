# Licensing

## Ferroly's license

Ferroly is dual-licensed under **either** of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

**at your option.** This is the same dual license used across the nandlabs
projects (including [golly](https://github.com/nandlabs/golly)). The SPDX
expression is:

```
Apache-2.0 OR MIT
```

You may use Ferroly under the terms of *either* license — you do not have to
comply with both. The MIT license is short and simple; the Apache-2.0 license
additionally includes an explicit patent grant.

## Contributions

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Ferroly by you, as defined in the Apache-2.0 license, shall be
dual licensed as above (Apache-2.0 OR MIT), without any additional terms or
conditions. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Third-party dependencies

Ferroly is intentionally dependency-minimal. Its entire runtime dependency tree
is `tokio` plus the TLS stack (`rustls` / `tokio-rustls` / `rustls-pki-types` /
`webpki-roots`); the only build-time dependencies are the proc-macro tooling
(`proc-macro2` / `syn` / `quote`) used by `ferroly-derive`.

**Every dependency is permissively licensed** — there is no copyleft (no GPL,
LGPL, AGPL, MPL, or EPL) anywhere in the tree. The complete set of licenses in
the resolved graph is:

| License | Notes |
|---|---|
| `MIT`, `Apache-2.0`, `MIT OR Apache-2.0` | the large majority of crates |
| `ISC` | `untrusted`, `rustls-webpki` |
| `BSD-3-Clause` | `subtle` |
| `Unicode-3.0` | data license, part of `unicode-ident` (build-time) |
| `CDLA-Permissive-2.0` | `webpki-roots` — a permissive *data* license for the Mozilla root certificates |
| `Apache-2.0 AND ISC` | `ring` — its bundled OpenSSL/BoringSSL-derived code |

All of the above are compatible with redistributing Ferroly under `Apache-2.0 OR
MIT`.

### Binary distribution / attribution

If you distribute Ferroly (or a binary built with it), the MIT, ISC, BSD-3-Clause,
and Apache-2.0 licenses require that you preserve the relevant copyright and
license notices. The recommended way to satisfy this is to generate a
third-party notices file, e.g.:

```sh
cargo install cargo-about
cargo about generate about.hbs > THIRD-PARTY-NOTICES.html
```

Dependency licenses are also enforced in CI via [`cargo-deny`](deny.toml).

### Notes for strict license policies

Two entries are permissive but sometimes flagged by strict corporate allowlists:

- **`webpki-roots` (`CDLA-Permissive-2.0`)** — a data license, not an OSI
  *software* license. If this is a problem for your policy, you can swap the
  Mozilla root bundle for the OS trust store via `rustls-platform-verifier`
  (`MIT OR Apache-2.0`).
- **`ring` (`Apache-2.0 AND ISC`)** — bundles crypto code derived from
  OpenSSL/BoringSSL. It can be replaced with `aws-lc-rs` if preferred.

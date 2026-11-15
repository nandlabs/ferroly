# Contributing to Ferroly

Thanks for your interest in contributing! This document covers how to build, test, and submit
changes, plus the project's dependency policy and licensing terms.

## Getting started

```sh
git clone https://github.com/nandlabs/ferroly
cd ferroly
cargo test -p ferroly --features full
```

## Reporting issues

Open issues through the
[issue chooser](https://github.com/nandlabs/ferroly/issues/new/choose), which
provides templates for each kind:

- **Bug reports** — steps to reproduce, expected vs. actual behavior.
- **Feature requests** — the use case and the proposed API.
- **Documentation** — what is missing, wrong, or unclear.
- **Questions** — usage help.

## Before you open a pull request

Please make sure the following all pass locally — CI runs the same checks:

```sh
# formatting (must be clean)
cargo fmt --all --check

# lints (warnings are errors)
cargo clippy --workspace --all-targets --features ferroly/full -- -D warnings

# tests (default features, full, and the derive crate)
cargo test -p ferroly
cargo test -p ferroly --features full
cargo test -p ferroly-derive

# the feature you touched must also lint and document cleanly on its own
cargo clippy -p ferroly --no-default-features --features <feature> --lib -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p ferroly --no-default-features --features <feature>

# dependency budget, and license / advisory policy (see below)
./.github/scripts/check-dependency-budget.sh
cargo deny check
```

CI runs the single-feature lint and rustdoc for **every** feature (the
`Feature isolation` job), because building only `full` hides a feature that forgets
a dependency edge, leaves dead code behind, or links to another feature's docs.
Gate cross-feature code with `#[cfg(feature = "…")]`, give integration tests a
`#![cfg(feature = "…")]` header, and declare `required-features` on examples.

The parsers that read untrusted input have `cargo-fuzz` targets under `fuzz/`
(run weekly in CI) and an always-on generative suite in
`crates/ferroly/tests/codec_fuzz.rs`. A new parser needs both.

Other CI jobs you can run locally: `cargo check -p ferroly --target
i686-unknown-linux-gnu --features full` (32-bit), `cargo test --manifest-path
ci/rt-consumer/Cargo.toml` (`ferroly::rt` in a crate without tokio), and the
`Interop` workflow's scripts — `interop/h2spec.sh` (needs an `h2spec` binary) and
`interop/grpc-go.sh` (needs Go), which check the HTTP/2 server against h2spec and
the gRPC stack against grpc-go in both directions. `interop/go-http.sh` (needs Go)
drives the HTTP/2 server with Go's `net/http` client.

## Versioning & releases (please read)

**Every pull request that changes the published crates (anything under `crates/`, or
the root `Cargo.toml`, whose `[workspace.package]` and `[workspace.dependencies]` go
into both published manifests) must carry a version bump** — CI enforces this via the
`version-guard` job, and a red check there is almost always one of the steps below.
PRs that touch only CI, docs outside the crates, or the lockfile are exempt: they
release nothing.

1. **Bump the crate version** in the root `Cargo.toml` under `[workspace.package]`.
   The crate is pre-1.0, so:
   - **patch** (`0.3.0` → `0.3.1`) for documentation, fixes, and internal changes;
   - **minor** (`0.3.0` → `0.4.0`) for new functionality or a breaking change.
2. **Update the README version badge** to the *same* version. `version-guard` fails
   if `Cargo.toml` and the badge disagree.
3. **Add a matching `## [<version>]` section to `CHANGELOG.md`** describing your
   change. `version-guard` fails if the version has no changelog entry
   (`## [Unreleased]` does not count).
4. **Do not create the git tag.** The `v<version>` tag is created automatically when
   your PR merges to `main`, on the commit the merge put there (the squash or merge
   commit, or the last commit of a rebase merge). A version that has *already* been
   tagged is rejected — that is the signal to bump again.

`version-guard` runs `.github/scripts/check-release-version.sh`, the same check that
tag-on-merge and the publish workflow run: both crates carry the version, ferroly's
`ferroly-derive` requirement in `[workspace.dependencies]` is that version, and the
changelog has its entry. Run it locally with `.github/scripts/check-release-version.sh
<version>`.

After bumping, run `cargo build --features full --locked` so `Cargo.lock` picks up the
new version, and commit the updated lockfile.

### Publishing to crates.io

Contributors don't publish anything: merging a version bump to `main` does it. The
rest of this section is for maintainers.

**What happens on a release.** `tag-on-merge.yml` creates the `v<version>` tag and
calls the `Publish to crates.io` workflow (`.github/workflows/publish.yml`) for it.
That workflow has no tag-push trigger: for a tag push GitHub would run the workflow
file from the tagged commit, which could be any commit with its own copy of the
checks. It only runs from `main` (a merge, or *Run workflow*), so its checks are
always main's. It runs:

1. **verify** (no credentials):
   - the run is from `main`, and the tag is `v<semver>`;
   - the tagged commit is on `main`, and passes `check-release-version.sh` (the copy
     on `main`, not the tagged tree's): both crates and ferroly's requirement on
     ferroly-derive carry the tag's version, and `CHANGELOG.md` has its entry;
   - it looks each crate up in the crates.io sparse index and runs `cargo publish
     --dry-run --locked` for those not yet published. When both are new this is one
     multi-package dry-run (`-p ferroly-derive -p ferroly`): cargo packages
     ferroly-derive into a local overlay registry and builds ferroly against it, so
     ferroly is verified fully even though its derive crate at that version is not
     on crates.io yet. (A plain `cargo publish --dry-run -p ferroly`, and even
     `cargo package --no-verify -p ferroly`, fail at that point: neither can resolve
     `ferroly-derive = "^<version>"`.);
   - the `crates-io` environment exists, has required reviewers, and deploys from
     the branch `main` only (see *Repository settings*). GitHub creates a missing
     environment on first use with no protection at all, so the run stops here
     instead.
2. **publish**, in the protected `crates-io` environment, so it waits for a
   reviewer's approval. It checks out the exact commit `verify` checked (by SHA) and
   first confirms the tag still points there; a tag moved in between stops the run
   with nothing uploaded. Then `cargo publish --no-verify -p ferroly-derive` (the
   build already ran in `verify`, without credentials), polls the sparse index
   (every 10 s, for up to 10 minutes) until it serves that version, and `cargo
   publish --no-verify -p ferroly`. A crate whose version is already in the index is
   skipped, so re-running the workflow is always safe; when both are already there
   the job (and its approval prompt) is skipped.
3. **drift**: the `crates.io drift` check below, for the tag just published.

**Publishing an existing tag** (a failed or never-approved run, or a release from
before this workflow existed): Actions → *Publish to crates.io* → *Run workflow*,
branch **`main`**, with the tag (for example `v0.5.0`) in the *tag* input. Any
`v<version>` tag on `main` works, including ones whose tree predates the workflow.

**Backlog: 0.4.0 and 0.5.0 are not on crates.io.** Both are tagged on `main`
(`v0.4.0`, `v0.5.0`), but crates.io stops at 0.3.2. Once the environment below is set
up, publish them with the dispatch input, oldest first so crates.io's newest version
ends up the newest release:

1. *Run workflow* on `main` with tag `v0.4.0` (optional; it keeps crates.io's version
   history complete); approve it.
2. *Run workflow* on `main` with tag `v0.5.0`; approve it.
3. Then any newer tag (`v0.6.0` once merged) the same way, unless its merge already
   published it.

Until then the daily drift check is red and names both tags.

By hand, from a clean checkout of the tag, with cargo 1.90 or newer (multi-package
`--dry-run`) and a token in `CARGO_REGISTRY_TOKEN`:

```sh
git checkout v0.5.0
cargo publish --dry-run --locked -p ferroly-derive -p ferroly
cargo publish --locked --no-verify -p ferroly-derive
# wait until https://index.crates.io/fe/rr/ferroly-derive lists "vers":"0.5.0"
cargo publish --locked --no-verify -p ferroly
```

**Drift guard.** `.github/workflows/crates-io-drift.yml` runs daily on `main`, by
hand, and after every publish run. It asks the crates.io API (User-Agent
`ferroly-ci`) for both crates:

- after a publish (or by hand with a *tag*): at that tag's version;
- daily (or by hand without a tag): at every `v*` tag, newest first, down to the
  newest one that is fully on crates.io, so an older unpublished release is reported
  too. It also fails when the version on `main` has no tag (tag-on-merge failed or
  never ran).

The daily run allows a 24-hour grace period for a tag that is still waiting for
approval; the run after a publish allows none. A failing daily run opens an issue
titled "crates.io drift: a release is not on crates.io" (or comments on the open one;
`crates-io-drift-alert.yml`), since GitHub mails a failed schedule only to whoever
last edited its cron line. Close the issue once the release is published. GitHub
also disables scheduled workflows after 60 days without repository activity;
re-enable it under Actions if that happens. The fix for a red drift check is to
publish the tags it names (above). For an untagged version on `main`, re-run the
failed `Tag on merge` run of the merge that set the version.

**Repository settings.** Do these before the first publish run; `verify` refuses to
continue until the environment is protected.

- Settings → Environments → **New environment** `crates-io`, then:
  - **Required reviewers**: the maintainers who may approve a release (and, ideally,
    *Prevent self-review*).
  - **Deployment branches and tags**: *Selected branches and tags*, with exactly one
    rule: the **branch** `main`. No tag rule: nothing publishes from a tag ref, and
    `verify` rejects an environment that allows one.
  - **Credentials**, one of:
    - *Token* (the default): the **environment** secret `CARGO_REGISTRY_TOKEN`,
      holding a crates.io API token (crates.io → Account Settings → API Tokens) with
      an expiry, the **`publish-update` scope only**, and crate scopes limited to
      **`ferroly` and `ferroly-derive`**. Never add it as a repository secret: the
      workflows pass no secrets between them, so only the approved `crates-io` job
      can read it.
    - *Trusted publishing* (no stored token): on crates.io, for **each** of the two
      crates, Settings → Trusted Publishing → add a GitHub publisher with owner
      `nandlabs`, repository `ferroly`, environment `crates-io`, and workflow
      `publish.yml`; then add a second one, identical but with workflow
      `tag-on-merge.yml` (crates.io checks the workflow that started the run, and
      for a merge that is tag-on-merge, which calls `publish.yml`). Then set the
      environment *variable* `CRATES_IO_TRUSTED_PUBLISHING` to `true`: the publish
      job exchanges a GitHub OIDC token for a short-lived crates.io token
      (`rust-lang/crates-io-auth-action`), which is revoked when the job ends. The
      `CARGO_REGISTRY_TOKEN` secret can then be deleted, and the crates.io token
      revoked.
- Merge methods: squash, merge commit and rebase all tag the right commit, so none
  needs disabling.

**Testing the release workflows.** `.github/tests/test_publish_workflows.py` runs
the shell in these workflows against throwaway git repositories and a mock crates.io
and GitHub API on 127.0.0.1 (no network, no credentials). CI's *Release workflow
tests* job runs it whenever something under `.github/` changes; locally:
`python3 -I .github/tests/test_publish_workflows.py .`

## Minimum supported Rust version (MSRV)

The MSRV is **Rust 1.75** (`rust-version` in the root `Cargo.toml`). CI's `MSRV (1.75)`
job builds every feature and runs the whole test suite on it, so tests, doc examples and
examples must stay within 1.75 as well — no newer std APIs (`File::lock`, `LazyLock`,
`Option::is_none_or`, …) or syntax.

The policy:

- **Ceiling: stable minus 4.** The MSRV is never newer than the stable release four
  versions back. Today it is held far lower than that.
- **Raised only in a minor release** (`0.x` → `0.(x+1)`), with a `CHANGELOG.md` entry —
  never in a patch release.
- **Raised immediately when a held-back dependency has an advisory.** Some
  dependencies are held at old versions because newer ones need newer Rust: `tokio`
  is held below 1.54 (1.54 needs Rust 1.85), and `zeroize`, `cc` and `find-msvc-tools`
  are held before edition 2024 (see the `ignore` list in `.github/dependabot.yml`). If
  one of them gets a security advisory (`cargo deny` checks weekly) whose fix needs a
  newer Rust, the fix wins: the MSRV is raised in the next release, patch or not.
- When the MSRV moves, update `rust-version`, the MSRV job's `toolchain:` and
  `RUSTUP_TOOLCHAIN`, the README badges, and drop the `dependabot.yml` holds it no
  longer needs.

## Dependency policy (please read)

Ferroly's defining goal is to be **self-contained**. This is enforced, not aspirational:

- **The only permitted external runtime dependencies** are `tokio` and the TLS stack
  (`rustls`, `tokio-rustls`, `rustls-pki-types`, `webpki-roots`).
- **One optional exception:** the `log` facade crate, enabled only by the off-by-default
  `log-bridge` feature. A bridge into the facade cannot exist without it, and it has no
  dependencies of its own (the budget script checks that).
- **Build-time** dependencies are limited to the proc-macro tooling (`proc-macro2`, `syn`,
  `quote`) used by `ferroly-derive`.
- **New external dependencies are not accepted.** If a feature needs functionality we don't
  have, implement it in-house (that's the whole point of the project). TLS is the one exception
  and it must stay isolated behind `ferroly::http`'s internal transport — no TLS types in any
  public API.

CI's `Dependency budget` job fails when either crate gains a direct dependency outside
this list (`.github/scripts/check-dependency-budget.sh`). PRs that add a dependency to
`Cargo.toml` will be asked to remove it. If you believe an
exception is genuinely warranted, open an issue to discuss it *before* writing code.

## Code style & conventions

- Format with `rustfmt` (the repo's `rustfmt.toml` settings) — CI checks this.
- Keep `clippy` clean at `-D warnings`. Note `rustfmt` does **not** reformat code inside
  doc comments, so run clippy before pushing.
- **Indent doc-comment examples with spaces, never tabs** — clippy's
  `tabs_in_doc_comments` is an error at `-D warnings`.
- Rustdoc examples rely on the implicit doctest `main`; don't wrap them in an explicit
  `# fn main() { … }` unless the example genuinely needs it.
- Public items are documented (`#![deny(missing_docs)]` is set on the modules).
- Errors use the in-house `#[derive(ferroly_derive::FerrolyError)]`, not `thiserror`.
- Encoding uses `ferroly::codec`'s `Encode`/`Decode`, not `serde`.
- Async trait methods return a manual `BoxFuture` (no `async-trait`).
- Prefer adding tests next to code (`#[cfg(test)] mod tests`) and end-to-end integration tests
  under `crates/ferroly/tests/` (gated with `#![cfg(feature = "...")]`).

## Documentation

Documentation is part of the source of truth and **must be kept in sync**. Each `docs/<module>.md`
page is a comprehensive developer guide, and they must stay that way:

- Update the relevant page in [`docs/`](docs/README.md) whenever you change a module's public
  API or behavior — every public type, method, feature flag, and optional item should be
  covered, with runnable examples for the main and advanced paths.
- Adding a **new module** requires a new `docs/<module>.md` page, an entry in the
  [`docs/README.md`](docs/README.md) module table and architecture diagram, a row in the
  feature table of the top-level [README](README.md), and an option in the "Affected /
  Target Module" dropdowns of the issue templates
  ([`bug_report.yml`](.github/ISSUE_TEMPLATE/bug_report.yml) and
  [`feature_request.yml`](.github/ISSUE_TEMPLATE/feature_request.yml)).
- Keep `///` doc comments accurate (CI builds docs with `-D warnings`, so broken intra-doc
  links fail the build).
- **Terminology:** the codec traits are `Encode` / `Decode` — never write "serialize" /
  "deserialize".
- **Attribution:** the port lineage is credited once in the top-level [README](README.md).
  Do **not** scatter "ports golly's X" / "(golly: …)" references through code comments or docs;
  describe what the code does on its own terms.

## Commits & pull requests

- Keep PRs focused; the [pull-request template](.github/PULL_REQUEST_TEMPLATE.md)
  prompts for the motivation and a summary of what changed — please fill it in.
- Use meaningful commit messages.
- Reference any related issue.
- Ensure the crate still builds with **default features** and with **`full`** (a feature that
  compiles only under `full` but breaks a smaller feature set is a bug).

## License

Ferroly is dual-licensed under **Apache-2.0 OR MIT** (see [LICENSING.md](LICENSING.md)).

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion
in Ferroly, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

In other words: **inbound = outbound.** By submitting a contribution you agree that it may be
distributed under both the MIT and the Apache-2.0 licenses, at the recipient's option. Do not
submit code you cannot license this way (e.g. copied from a copyleft-licensed project).

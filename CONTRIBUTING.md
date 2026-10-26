# Contributing to hive-board

Thanks for taking the time to help. Bug reports, ideas, documentation fixes and code are all
welcome. This guide says how to get a change from an idea to a merged pull request.

Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

**Security problems are not reported in issues.** Follow [SECURITY.md](SECURITY.md) and use
GitHub's private vulnerability reporting.

## Reporting a bug

[Open a bug report](https://github.com/jean1880/nuvek-hive-board/issues/new?template=bug_report.yml).
Search the [open and closed issues](https://github.com/jean1880/nuvek-hive-board/issues?q=is%3Aissue)
first, and try `master`, where fixes land before a release.

**Review everything you paste for secrets.** Logs, board posts and configuration can hold API
keys, tokens, hostnames and file paths. Replace anything sensitive with `****` before you post.

## Before you write code

**Open an issue first for anything bigger than a small fix**: a new route, a schema change, a
change of behaviour or a new dependency. A short discussion up front avoids a pull request that
cannot be merged as written. Typo fixes, small documentation fixes and obvious bug fixes can go
straight to a pull request.

The rules that most often decide a review:

- **Fail closed.** A missing token, an unparseable setting or an empty allow-list refuses; it
  never opens up.
- **Errors never expose** the database URL, a token or a host path, in a response or a log line.
- **Retention, not deletion.** The lifecycle sweep archives and compacts; only the DELETE routes
  remove data.
- **No `unwrap`, `expect` or `panic!` outside tests.**
- **Strings are truncated by character, never by byte.** Guard every slice with a multi-byte test.
- **Nothing names a deployment.** Hosts, paths and ports come from environment variables with
  `localhost` defaults.
- **Tests never touch real `$HOME` state.** Use a temporary directory.

## Setting up

You need Rust 1.90 or newer, Node 22 or newer (CI runs Node 24, which is the preferred development version), and a throwaway Postgres 16 for the database tests.

```bash
git clone https://github.com/jean1880/nuvek-hive-board.git
cd nuvek-hive-board
git config core.hooksPath .githooks   # gitleaks and rustfmt before each commit
```

The pre-commit hook needs [gitleaks](https://github.com/gitleaks/gitleaks) on your `PATH`.

## Testing

What CI runs, and what to run before you push:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
HIVE_TEST_DATABASE_URL=postgres://postgres:test@localhost:5432/postgres \
  cargo test --workspace --all-features
cargo deny check licenses bans sources
(cd frontend && npm ci && npm run build && npm run typecheck && npm test)
```

`HIVE_TEST_DATABASE_URL` is an admin URL to a **throwaway** server: every test creates its own
database and drops it afterwards. Never point it at a server holding real data. Without it, run the
tests that need no database:

```bash
cargo test --workspace --all-features --lib --bins --test default_trust_tests --test no_local_state
```

- **A bug fix** comes with a test that fails without the fix.
- **New logic** comes with unit tests beside it.
- **A change to a redaction pattern** comes with tests for what it must catch and for ordinary
  text it must leave alone.

## Commits

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):

```text
fix(jobs): refuse an owns entry equal to a configured root
feat(mcp): add board_get_thread
```

- **Types**: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `style`.
- **Subject**: says what is now true, in lower case, without a full stop.
- **Body**: explains why, when the subject alone doesn't. Mention a breaking change as
  `BREAKING CHANGE:` in the footer.
- **Sign off every commit** (`git commit -s`). The `Signed-off-by:` line certifies the
  [Developer Certificate of Origin](https://developercertificate.org/): that you wrote the change
  or otherwise have the right to submit it under the project's licence. Pull requests with
  unsigned commits are not merged.

Write prose in Canadian English (colour, behaviour, centre, licence as a noun). Code identifiers
keep their own spelling.

## Pull requests

1. Run the checks above and make sure they pass.
2. Open the pull request against `master`. Its title follows the Conventional Commits format.
3. Link the issue with `Fixes #123`, so it closes on merge.

Releases are cut by the maintainer. Leave the versions in `Cargo.toml` alone.

## AI-assisted contributions

Using AI tools in a contribution is fine. The responsibility for it stays with you: say so in the
pull request, understand every line, and run the checks yourself.

## Licence

hive-board is released under the [MIT Licence](LICENSE). By contributing, you agree that your
contribution is licensed under the same terms, and you certify the Developer Certificate of Origin
with your `Signed-off-by:` line.

# Releasing

Clients install from this repository's
[releases](https://github.com/l7aromeo/user-memory/releases): the one-line
installers download the latest one. A release is built on GitHub's runners
by `.github/workflows/release.yml`, and published only when the maintainer
runs it by hand. `scripts/client-release` builds, assembles and checks the
release in every job, and locally, where it is the release dry run; it never
publishes.

## Who does what

- **Whoever prepares the release** opens the release pull request (the
  version, the notes and the changelog, below), runs the dry run, and
  pushes the tag once the maintainer has approved it.
- **The maintainer** approves two things, and pushes nothing: the tag, by
  approving the release pull request and its commit, and the publish run,
  started by hand and approved in the `release` environment. Nothing
  reaches users without both.

## Prepare the release

On a branch, in one pull request:

1. Bump `version` in `Cargo.toml` (and `Cargo.lock`, with `cargo update
   --workspace`).
2. Write the notes for users in `release-notes/v<version>.md`: what to do
   on upgrade, what is new, what was fixed. They become the GitHub
   release's text, and a release without them is refused.
3. Make the changelog from every notes file, newest first, and commit it
   with them; it is never edited by hand:

   ```bash
   scripts/client-release --changelog > CHANGELOG.md
   ```

4. Run the release dry run, from a clean checkout of that commit:

   ```bash
   just client-release --out /tmp/user-memory-release
   ```

   It builds the macOS and Linux clients this machine can build from the
   committed tree (it refuses uncommitted changes), with a SHA-256 for
   each, and assembles the release into the folder: the binaries,
   `install.sh`, `uninstall.sh`, `install.ps1`, `uninstall.ps1`, the notes
   and `THIRD_PARTY_NOTICES.txt`, generated from the dependencies' licenses
   with [cargo-about](https://github.com/EmbarkStudios/cargo-about)
   (`cargo install cargo-about --locked --features cli`; `about.toml` lists
   the accepted licenses, and a dependency under any other stops the
   release). The Windows clients (`user-memory-<target>.exe`, for x64 and
   ARM64) build only on Windows, so a local run skips them and says so.

## The checks

Before anything is built, the script refuses:

- any committed file that contains your login name, your `git config
  user.email`, or a line of the optional private denylist
  `~/.config/user-memory/release-denylist` (one string per line, such as a
  host or an address; blank lines and `#` comments are ignored; never
  committed);
- a Markdown file (the docs, the README and the notes) that contains a home
  path, under `/Users` or `/home`, other than the docs' placeholder user
  `you`, or an HTML comment starting with `VERIFY`, left to check before
  a release;
- a `CHANGELOG.md` that is not the one `--changelog` makes.

It names each file and line, never the matched text. Then it runs `cargo
test --test docs`, which fails when a documented command or quoted message
disagrees with the code, and shows that test's output when it does. Each
built binary is refused if it holds the build machine's home folder.

## Build and publish on GitHub

1. **Build:** once the release pull request is merged and the tag
   approved, tag its commit on `main` and push the tag `v<version>`. First the run checks that the tag matches
   `Cargo.toml`'s version and that the Dockerfile's Rust is the current
   stable release (`scripts/check-rust-stable`), refusing an older one
   unless a `# Pinned below stable: <reason>` comment just above
   `RUST_IMAGE` says why, such as a minimum supported Rust version. Each
   target builds on its own runner (`--build-only`). Linux targets build in
   the Dockerfile's Rust image, so they need no newer glibc than today's
   releases; the log prints each runner's architecture and each Linux
   binary's glibc floor. The two Windows targets build on Windows runners in
   PowerShell, as the script would: only the client binary, with the
   checkout's and the runner's profile paths remapped out of it and checked
   absent (the dependencies' sources sit outside the profile, since the
   Windows C compilers get no path map), and a `.exe.sha256` in `shasum`'s format. Every build then runs
   its binary with the debug-only `debug-paths` command and fails unless
   clap refuses it, so a command meant for tests can never ship.
2. **Assemble:** the same run makes one release from those binaries
   (`--binaries`), with every check above, and uploads it as the `release`
   artifact. It stops there: a tag never publishes on its own.
3. **Publish:** when the maintainer is ready, the workflow runs by hand with
   the tag and `publish`:

   ```bash
   gh workflow run release.yml -f tag=v0.7.0 -f publish=true
   ```

   or use **Run workflow** on the workflow's page. It publishes exactly what
   the tag's run built and checked: it finds that run, downloads its
   artifact, checks it again (`--verify`: every installer, the notices, the
   notes and all six builds with their checksums, and that it was built from
   the tag's commit), then creates the release from the tag, with the notes
   as its text. It never rebuilds, and it refuses a tag that is not on
   `main`, a tag whose run did not succeed, and a release that already
   exists.

A push to a branch named `release-dry-run/<anything>`, or a manual run
without `publish`, does steps 1 and 2 on that branch, publishing nothing.

Only the publish job may write to the repository (`contents: write`), with
the workflow's own token, and it runs in the `release` environment, where
the maintainer can require an approval. The one secret is
`RELEASE_DENYLIST`, which holds the denylist's lines; on GitHub it must also
hold the maintainer's login name and git email, since the runner's own
differ.

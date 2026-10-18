//! The release script, scripts/client-release: the checks that keep this
//! machine and its owner out of the committed tree and the release, the
//! changelog made from the release notes, the build and assembly of a
//! release, and the check the publish run makes before it publishes. The
//! script never reaches GitHub: release.yml's publish job does that.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]
#![cfg(unix)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::tempdir;
#[path = "support/fake.rs"]
mod fake;

/// The login name and git email every run sees, so the checks never depend
/// on who runs the tests.
const LOGIN: &str = "releasetester";
const EMAIL: &str = "release-test@example.invalid";

const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// Every target a published release holds, in the script's order.
const TARGETS: [&str; 6] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// The installers and uninstallers every release carries, in its order.
const INSTALLERS: [&str; 4] = ["install.sh", "uninstall.sh", "install.ps1", "uninstall.ps1"];

fn repo_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn has_zsh() -> bool {
    Path::new("/bin/zsh").exists()
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(identity())
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out).1);
    text(&out).0.trim().to_owned()
}

/// The git identity of every run, set above any config file.
fn identity() -> [(&'static str, &'static str); 7] {
    [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "user.email"),
        ("GIT_CONFIG_VALUE_0", EMAIL),
        ("GIT_CONFIG_KEY_1", "user.name"),
        ("GIT_CONFIG_VALUE_1", "Release Test"),
    ]
}

/// Every file under `dir`, relative to it.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path.strip_prefix(dir).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

/// Writes `name`'s SHA-256 line beside it in `dir`, in shasum's format, as a
/// build does.
fn checksum(dir: &Path, name: &str) {
    let sum = Command::new("/bin/zsh")
        .args([
            "-c",
            "shasum -a 256 $1 2>/dev/null || sha256sum $1",
            "zsh",
            name,
        ])
        .current_dir(dir)
        .output()
        .unwrap();
    fs::write(dir.join(format!("{name}.sha256")), &sum.stdout).unwrap();
}

fn asset(target: &str) -> String {
    if target.ends_with("-pc-windows-msvc") {
        format!("user-memory-{target}.exe")
    } else {
        format!("user-memory-{target}")
    }
}

/// A committed source tree with the script, docs, release notes and the
/// changelog made from them, for running the build step.
fn source_tree(dir: &Path) -> PathBuf {
    let source = dir.join("source");
    fs::create_dir_all(source.join("scripts")).unwrap();
    fs::create_dir_all(source.join("docs/dev")).unwrap();
    fs::copy(
        repo_file("scripts/client-release"),
        source.join("scripts/client-release"),
    )
    .unwrap();
    for script in INSTALLERS {
        fs::copy(repo_file(script), source.join(script)).unwrap();
    }
    fs::write(
        source.join("Cargo.toml"),
        "[package]\nversion = \"9.9.9\"\n",
    )
    .unwrap();
    fs::write(source.join("README.md"), "# user-memory\n").unwrap();
    fs::write(source.join("docs/install.md"), "# Install\n").unwrap();
    fs::write(source.join("docs/dev/development.md"), "# Development\n").unwrap();
    fs::create_dir_all(source.join("release-notes")).unwrap();
    // 9.8.10 is newer than 9.8.9: the changelog sorts by number, not text.
    for version in ["9.9.9", "9.8.10", "9.8.9"] {
        fs::write(
            source.join(format!("release-notes/v{version}.md")),
            format!("## Upgrade\n\nRun the installer for {version}.\n\n```bash\n# a comment, not a heading\n```\n"),
        )
        .unwrap();
    }
    fs::write(source.join("CHANGELOG.md"), changelog(&source)).unwrap();
    // Builds write under target/, which the repository ignores.
    fs::write(source.join(".gitignore"), "/target\n").unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "init"]);
    source
}

/// What `--changelog` prints for the release notes of `source`.
fn changelog(source: &Path) -> String {
    let out = Command::new("/bin/zsh")
        .arg(source.join("scripts/client-release"))
        .arg("--changelog")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out).1);
    text(&out).0
}

/// `path` with the real gh out of reach, as every run here uses it: in
/// front of the system folders, where GitHub's Linux runners keep a real
/// gh, a fake one that logs any call and fails. Real cargo, which could
/// reach the network, must not be in those folders at all.
fn guarded(path: &str, home: &Path) -> String {
    for dir in SYSTEM_PATH.split(':') {
        assert!(
            !Path::new(dir).join("cargo").exists(),
            "{dir}/cargo would let a release test run the real cargo"
        );
    }
    let guard = home.join("no-real-gh");
    fs::create_dir_all(&guard).unwrap();
    fake::fake(&guard, "gh");
    format!("{}:{path}", guard.display())
}

/// Runs the script in `source`, with `home` as the sandbox, and checks it
/// never called gh.
fn run(source: &Path, home: &Path, path: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let out = Command::new("/bin/zsh")
        .arg(source.join("scripts/client-release"))
        .args(args)
        .current_dir(source)
        .env_clear()
        .env("PATH", guarded(path, home))
        .sandboxed(home)
        .env("USER", LOGIN)
        .env("TMPDIR", home)
        .envs(identity())
        .envs(envs.iter().copied())
        .output()
        .unwrap();
    assert!(
        !home.join("no-real-gh/gh.calls").exists(),
        "the script called gh"
    );
    out
}

/// A PATH with the fake toolchain (tests/fake/cli.rs) before the
/// system's, so the build step runs without a compiler: `cargo build` writes
/// a binary holding `$FAKE_BINARY_TEXT`, `cargo test` passes (or prints
/// `$FAKE_TEST_FAILURE` and fails), `cargo about` writes the notices, and the
/// host is aarch64-apple-darwin with that target installed.
fn fake_toolchain(dir: &Path) -> String {
    let bin = dir.join("toolchain");
    fs::create_dir_all(&bin).unwrap();
    for name in ["cargo", "cargo-about", "rustc", "rustup"] {
        fake::fake(&bin, name);
    }
    format!("{}:{SYSTEM_PATH}", bin.display())
}

/// Commits `content` as `file` in a source tree under `dir`, then runs the
/// build without cargo, which stops at the docs test when every check
/// before it passes. Returns whether it got there, and what it said.
fn build_with(dir: &Path, file: &str, content: &str) -> (bool, String) {
    let source = source_tree(dir);
    let path = source.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "plant"]);
    let out = run(
        &source,
        dir,
        SYSTEM_PATH,
        &["--out", dir.join("release").to_str().unwrap()],
        &[],
    );
    let stderr = text(&out).1;
    (stderr.contains("the docs test needs cargo"), stderr)
}

/// A tree with `content` in `file` is refused for `reason`, before the
/// release's notes are copied.
fn assert_refused(file: &str, content: &str, reason: &str) {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let (passed, stderr) = build_with(dir.path(), file, content);
    assert!(!passed, "{file} passed: {stderr}");
    assert!(stderr.contains(reason), "{stderr}");
    assert!(!dir.path().join("release/notes").exists(), "{stderr}");
}

// A clean tree passes every check; the build then copies the notes and
// stops at the docs test, which needs cargo.
#[test]
fn a_clean_tree_passes_every_check_before_the_docs_test() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let out = dir.path().join("release");
    let clean = run(
        &source,
        dir.path(),
        SYSTEM_PATH,
        &["--out", out.to_str().unwrap()],
        &[],
    );
    assert!(
        text(&clean).1.contains("the docs test needs cargo"),
        "{}",
        text(&clean).1
    );
    assert_eq!(files(&out), [".commit", ".tag", "notes/v9.9.9.md"]);
    assert_eq!(
        fs::read_to_string(out.join("notes/v9.9.9.md")).unwrap(),
        fs::read_to_string(source.join("release-notes/v9.9.9.md")).unwrap()
    );
}

#[test]
fn a_home_path_in_a_doc_is_refused() {
    assert_refused(
        "docs/install.md",
        "# Install\n\nSee /Users/x/notes.\n",
        "docs/install.md line 3: a home path",
    );
}

#[test]
fn the_readme_is_checked_too() {
    assert_refused(
        "README.md",
        "# user-memory\n\nSee /home/x/notes.\n",
        "README.md line 3: a home path",
    );
}

// The docs' placeholder user, as in a sample hook command, names no one.
#[test]
fn the_docs_placeholder_home_is_allowed() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let (passed, stderr) = build_with(
        dir.path(),
        "docs/install.md",
        "'/Users/you/.local/bin/user-memory' and /home/you\n",
    );
    assert!(passed, "{stderr}");
}

// Code and tests may hold home paths, as fixtures do; only the published
// text (Markdown) is checked for them.
#[test]
fn a_home_path_in_code_is_not_a_doc() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let (passed, stderr) = build_with(
        dir.path(),
        "tests/paths.rs",
        "const HOME: &str = \"/Users/x\";\n",
    );
    assert!(passed, "{stderr}");
}

// Everything in the tree is public: the login name is refused in any file.
#[test]
fn the_login_name_is_refused_anywhere_in_the_tree() {
    assert_refused(
        "src/where.rs",
        &format!("// built by {LOGIN}\n"),
        "src/where.rs line 1: the login name",
    );
}

#[test]
fn the_git_email_is_refused_anywhere_in_the_tree() {
    assert_refused(
        "tests/who.rs",
        &format!("// {}\n", EMAIL.to_uppercase()),
        "tests/who.rs line 1: the git user's email",
    );
}

/// Writes the denylist into `home` with `lines`.
fn denylist(home: &Path, lines: &str) {
    let dir = home.join(".config/user-memory");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("release-denylist"), lines).unwrap();
}

#[test]
fn a_denylist_entry_is_refused_anywhere_in_the_tree() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    denylist(dir.path(), "  memory.internal.example  \n");
    let (passed, stderr) = build_with(
        dir.path(),
        "compose.yaml",
        "host: Memory.Internal.Example\n",
    );
    assert!(!passed, "{stderr}");
    assert!(
        stderr.contains("compose.yaml line 1: an entry of "),
        "{stderr}"
    );
    assert!(
        !stderr.contains("memory.internal"),
        "names no entry: {stderr}"
    );
}

#[test]
fn a_denylist_with_blank_lines_and_comments_refuses_nothing_else() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    denylist(dir.path(), "# private hosts\n\n   \n");
    let (passed, stderr) = build_with(dir.path(), "docs/install.md", "# Install\n\nRun it.\n");
    assert!(passed, "{stderr}");
}

#[test]
fn a_note_left_to_verify_is_refused() {
    assert_refused(
        "docs/dev/development.md",
        "# Development\n\n<!-- VERIFY: the flag name -->\n",
        "docs/dev/development.md line 3: a note to verify before publishing",
    );
}

#[test]
fn release_notes_get_the_same_checks_as_the_docs() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let notes = source.join("release-notes/v9.9.9.md");
    let mut body = fs::read_to_string(&notes).unwrap();
    body.push_str("See /Users/x/notes.\n");
    fs::write(&notes, body).unwrap();
    fs::write(source.join("CHANGELOG.md"), changelog(&source)).unwrap();
    git(&source, &["commit", "-q", "-am", "plant"]);
    let out = run(
        &source,
        dir.path(),
        SYSTEM_PATH,
        &["--out", dir.path().join("release").to_str().unwrap()],
        &[],
    );
    let stderr = text(&out).1;
    assert!(
        stderr.contains("release-notes/v9.9.9.md line 8: a home path"),
        "{stderr}"
    );
}

#[test]
fn a_build_without_its_release_notes_is_refused() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    git(&source, &["rm", "-q", "release-notes/v9.9.9.md"]);
    fs::write(source.join("CHANGELOG.md"), changelog(&source)).unwrap();
    git(&source, &["commit", "-q", "-am", "drop the notes"]);
    let out = run(
        &source,
        dir.path(),
        SYSTEM_PATH,
        &["--out", dir.path().join("release").to_str().unwrap()],
        &[],
    );
    assert!(!out.status.success());
    assert!(
        text(&out).1.contains("release-notes/v9.9.9.md is missing"),
        "{}",
        text(&out).1
    );
}

/// The changelog is made from every release's notes, newest first by
/// version number, each under its tag, with the notes' headings one level
/// deeper so they sit under it, and code blocks left as they are.
#[test]
fn the_changelog_is_made_from_every_release_note() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let changelog = changelog(&source);
    let headings: Vec<&str> = changelog
        .lines()
        .filter(|line| line.starts_with('#'))
        .collect();
    assert_eq!(
        headings,
        [
            "# Changelog",
            "## v9.9.9",
            "### Upgrade",
            "# a comment, not a heading",
            "## v9.8.10",
            "### Upgrade",
            "# a comment, not a heading",
            "## v9.8.9",
            "### Upgrade",
            "# a comment, not a heading",
        ]
    );
    assert!(
        changelog.contains("Run the installer for 9.8.10."),
        "{changelog}"
    );
}

// The changelog is committed, never edited by hand: a release whose
// CHANGELOG.md is not the one the notes make is refused.
#[test]
fn a_changelog_out_of_step_with_the_notes_is_refused() {
    assert_refused(
        "CHANGELOG.md",
        "# Changelog\n",
        "CHANGELOG.md is not made from release-notes; run: scripts/client-release --changelog > CHANGELOG.md",
    );
}

// This repository's own changelog is the one its notes make.
#[test]
fn the_committed_changelog_is_made_from_the_release_notes() {
    if !has_zsh() {
        return;
    }
    assert!(
        changelog(&repo_file("")) == fs::read_to_string(repo_file("CHANGELOG.md")).unwrap(),
        "CHANGELOG.md is out of date; run: scripts/client-release --changelog > CHANGELOG.md"
    );
}

#[test]
fn build_only_compiles_the_binaries_and_nothing_else() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let path = fake_toolchain(dir.path());
    let out = dir.path().join("built");
    let built = run(
        &source,
        dir.path(),
        &path,
        &[
            "--build-only",
            "--target",
            "aarch64-apple-darwin",
            "--out",
            out.to_str().unwrap(),
        ],
        &[],
    );
    assert!(built.status.success(), "{}", text(&built).1);
    assert_eq!(
        files(&out),
        [
            ".commit",
            ".tag",
            "user-memory-aarch64-apple-darwin",
            "user-memory-aarch64-apple-darwin.sha256"
        ]
    );
}

#[test]
fn a_binary_that_names_the_home_folder_is_refused() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let path = fake_toolchain(dir.path());
    let home = dir.path().to_str().unwrap();
    let built = run(
        &source,
        dir.path(),
        &path,
        &[
            "--build-only",
            "--target",
            "aarch64-apple-darwin",
            "--out",
            dir.path().join("built").to_str().unwrap(),
        ],
        &[(
            "FAKE_BINARY_TEXT",
            &format!("panicked at {home}/src/main.rs"),
        )],
    );
    assert!(!built.status.success());
    assert!(
        text(&built).1.contains("names this machine's home folder"),
        "{}",
        text(&built).1
    );
}

// A local dry run builds, assembles and checks a release in its folder,
// and says that nothing was published.
#[test]
fn a_local_release_is_assembled_and_checked_and_never_published() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let path = fake_toolchain(dir.path());
    let out = dir.path().join("release");
    let made = run(
        &source,
        dir.path(),
        &path,
        &[
            "--target",
            "aarch64-apple-darwin",
            "--out",
            out.to_str().unwrap(),
        ],
        &[],
    );
    let (stdout, stderr) = text(&made);
    assert!(made.status.success(), "{stderr}");
    assert!(
        stdout.contains("Release v9.9.9 written to") && stdout.contains("nothing was published"),
        "{stdout}"
    );
    assert_eq!(
        files(&out),
        [
            ".commit",
            ".tag",
            "THIRD_PARTY_NOTICES.txt",
            "install.ps1",
            "install.sh",
            "notes/v9.9.9.md",
            "uninstall.ps1",
            "uninstall.sh",
            "user-memory-aarch64-apple-darwin",
            "user-memory-aarch64-apple-darwin.sha256",
        ]
    );
    for file in INSTALLERS {
        assert_eq!(
            fs::read(out.join(file)).unwrap(),
            fs::read(repo_file(file)).unwrap(),
            "{file}"
        );
    }
}

#[test]
fn a_release_assembles_from_binaries_built_elsewhere() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let path = fake_toolchain(dir.path());
    let built = dir.path().join("built");
    let target = ["--target", "aarch64-apple-darwin"];
    let only = run(
        &source,
        dir.path(),
        &path,
        &[
            &["--build-only", "--out", built.to_str().unwrap()][..],
            &target,
        ]
        .concat(),
        &[],
    );
    assert!(only.status.success(), "{}", text(&only).1);

    // No toolchain compiles here: only the binaries from --build-only.
    fs::create_dir_all(dir.path().join("no-rustc")).unwrap();
    for name in ["cargo", "cargo-about"] {
        fs::copy(
            dir.path().join("toolchain").join(name),
            dir.path().join("no-rustc").join(name),
        )
        .unwrap();
    }
    let assemble = |out: &Path| {
        run(
            &source,
            dir.path(),
            &format!("{}:{SYSTEM_PATH}", dir.path().join("no-rustc").display()),
            &[
                &[
                    "--binaries",
                    built.to_str().unwrap(),
                    "--out",
                    out.to_str().unwrap(),
                ][..],
                &target,
            ]
            .concat(),
            &[],
        )
    };
    let release = dir.path().join("release");
    let assembled = assemble(&release);
    assert!(assembled.status.success(), "{}", text(&assembled).1);
    assert_eq!(
        fs::read(release.join("user-memory-aarch64-apple-darwin")).unwrap(),
        fs::read(built.join("user-memory-aarch64-apple-darwin")).unwrap()
    );

    fs::write(built.join("user-memory-aarch64-apple-darwin"), "tampered").unwrap();
    let tampered = assemble(&dir.path().join("tampered"));
    assert!(!tampered.status.success());
    assert!(
        text(&tampered).1.contains("does not match its checksum"),
        "{}",
        text(&tampered).1
    );
}

#[test]
fn options_are_refused_where_they_make_no_sense() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    for (args, message) in [
        (&["--build-only"][..], "give --out DIR"),
        (
            &["--build-only", "--binaries", "x", "--out", "y"],
            "give one",
        ),
        (
            &["--out", "y", "--commit", "abc1234"],
            "--commit checks an assembled release; use it with --verify",
        ),
        (&["--verify", "x", "--out", "y"], "give it alone"),
        (
            &["--changelog", "--out", "y"],
            "--changelog takes no other option",
        ),
        (&["--no-build"], "unknown option --no-build"),
        (&["--repo", "o/n"], "unknown option --repo"),
    ] {
        let out = run(&source, dir.path(), SYSTEM_PATH, args, &[]);
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out).1.contains(message), "{args:?}: {}", text(&out).1);
    }
}

// MSVC builds only on Windows, which release.yml's Windows jobs do: the
// script refuses to build a Windows target itself, and says where it builds.
#[test]
fn windows_targets_are_built_only_on_windows() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let path = fake_toolchain(dir.path());
    for target in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
        let built = run(
            &source,
            dir.path(),
            &path,
            &[
                "--build-only",
                "--target",
                target,
                "--out",
                dir.path().join(target).to_str().unwrap(),
            ],
            &[],
        );
        assert!(!built.status.success(), "{target}");
        assert!(
            text(&built).1.contains(&format!(
                "{target} builds only on Windows: release.yml builds it on a Windows runner"
            )),
            "{target}: {}",
            text(&built).1
        );
    }
}

/// A release assembled from a build of every target, as the tag's run
/// makes it and the publish run downloads it, built from abc1234.
fn assembled(dir: &Path) -> PathBuf {
    let out = dir.join("assembled");
    fs::create_dir_all(out.join("notes")).unwrap();
    fs::write(out.join(".tag"), "v9.9.9\n").unwrap();
    fs::write(out.join(".commit"), "abc1234\n").unwrap();
    fs::write(
        out.join("notes/v9.9.9.md"),
        "## Upgrade\n\nRun the installer.\n",
    )
    .unwrap();
    for script in INSTALLERS {
        fs::copy(repo_file(script), out.join(script)).unwrap();
    }
    fs::write(out.join("THIRD_PARTY_NOTICES.txt"), "notices").unwrap();
    for target in TARGETS {
        let name = asset(target);
        fs::write(out.join(&name), format!("build {target}")).unwrap();
        checksum(&out, &name);
    }
    out
}

/// Runs the publish run's check on `release`, from a source tree in `dir`.
fn verify(dir: &Path, release: &Path, extra: &[&str]) -> Output {
    let source = source_tree(dir);
    run(
        &source,
        dir,
        SYSTEM_PATH,
        &[&["--verify", release.to_str().unwrap()][..], extra].concat(),
        &[],
    )
}

#[test]
fn a_whole_release_passes_the_publish_check() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let release = assembled(dir.path());
    let out = verify(dir.path(), &release, &["--commit", "abc1234ffffffff"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(
        text(&out).0.contains("Release v9.9.9 in"),
        "{}",
        text(&out).0
    );
}

// The publish run publishes the tag's own build: a release built from
// another commit is refused.
#[test]
fn a_release_built_from_another_commit_is_refused() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let release = assembled(dir.path());
    let other = "1234567890abcdef1234567890abcdef12345678";
    let out = verify(dir.path(), &release, &["--commit", other]);
    assert!(!out.status.success());
    assert!(
        text(&out)
            .1
            .contains(&format!("was built from abc1234, not {other}")),
        "{}",
        text(&out).1
    );
}

// Each file a release needs is one it cannot be published without: every
// installer and uninstaller, the notices, the notes, each target's build
// and its checksum.
#[test]
fn a_release_missing_any_file_is_refused() {
    if !has_zsh() {
        return;
    }
    let mut needed: Vec<(String, String)> = INSTALLERS
        .iter()
        .chain(&["THIRD_PARTY_NOTICES.txt"])
        .map(|file| ((*file).to_owned(), format!("has no {file}")))
        .collect();
    needed.push(("notes/v9.9.9.md".into(), "a release needs its notes".into()));
    for target in TARGETS {
        let name = asset(target);
        needed.push((name.clone(), format!("has no build for {target}")));
        needed.push((
            format!("{name}.sha256"),
            format!("has no build for {target}"),
        ));
    }
    for (file, reason) in needed {
        let dir = tempdir().unwrap();
        let release = assembled(dir.path());
        fs::remove_file(release.join(&file)).unwrap();
        let out = verify(dir.path(), &release, &[]);
        assert!(!out.status.success(), "{file}");
        assert!(text(&out).1.contains(&reason), "{file}: {}", text(&out).1);
    }
}

#[test]
fn a_binary_that_does_not_match_its_checksum_is_refused() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let release = assembled(dir.path());
    let name = asset("aarch64-pc-windows-msvc");
    fs::write(release.join(&name), "tampered").unwrap();
    let out = verify(dir.path(), &release, &[]);
    assert!(!out.status.success());
    assert!(
        text(&out)
            .1
            .contains(&format!("{name} does not match its checksum")),
        "{}",
        text(&out).1
    );
}

// The notes become the release's text, so the publish check reads them as
// the build read the tree.
#[test]
fn the_publish_check_reads_the_notes_again() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let release = assembled(dir.path());
    fs::write(
        release.join("notes/v9.9.9.md"),
        "## Upgrade\n\nSee /Users/x/notes.\n",
    )
    .unwrap();
    let out = verify(dir.path(), &release, &[]);
    assert!(!out.status.success());
    assert!(
        text(&out).1.contains("v9.9.9.md line 3: a home path"),
        "{}",
        text(&out).1
    );
}

/// A release that stops at the docs test says which doc disagrees: the
/// test's own output, hidden when it passes, is shown.
#[test]
fn a_failed_docs_test_shows_why() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    let path = fake_toolchain(dir.path());
    let failure = "docs/cli.md:12: no code prints \"Removed: everything\"";
    let out = run(
        &source,
        dir.path(),
        &path,
        &[
            "--target",
            "aarch64-apple-darwin",
            "--out",
            dir.path().join("release").to_str().unwrap(),
        ],
        &[("FAKE_TEST_FAILURE", failure)],
    );
    let (_, stderr) = text(&out);
    assert!(!out.status.success());
    assert!(stderr.contains(failure), "{stderr}");
    assert!(
        stderr.contains("the docs disagree with the code"),
        "{stderr}"
    );
}

// No run here can reach the real gh: the one found is the guard in the
// test's own folder, never the system's.
#[test]
fn no_release_test_can_reach_the_real_gh() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let found = Command::new("/bin/zsh")
        .args(["-c", "command -v gh"])
        .env_clear()
        .env("PATH", guarded(SYSTEM_PATH, dir.path()))
        .output()
        .unwrap();
    let found = String::from_utf8(found.stdout).unwrap();
    assert!(
        Path::new(found.trim()).starts_with(dir.path()),
        "gh resolves outside the test: {found}"
    );
}

/// The release workflow runs scripts/client-release in every job but the
/// last step: each option it passes must be one the script takes. Only the
/// user's manual publish run, in the `release` environment, reaches the
/// publish job, which alone may write to the repository, with the
/// workflow's own token; a tag push never does.
#[test]
fn the_release_workflow_publishes_the_checked_build_in_this_repository() {
    if !has_zsh() {
        return;
    }
    let workflow = fs::read_to_string(repo_file(".github/workflows/release.yml")).unwrap();
    let help = Command::new("/bin/zsh")
        .arg(repo_file("scripts/client-release"))
        .arg("--help")
        .output()
        .unwrap();
    let help = text(&help).0;
    let runs: Vec<&str> = workflow
        .lines()
        .filter_map(|line| {
            line.split_once("zsh scripts/client-release")
                .map(|(_, rest)| rest)
        })
        .collect();
    assert_eq!(
        runs.len(),
        3,
        "build, assemble and the publish check each run it"
    );
    for run in runs {
        for option in run.split_whitespace().filter(|word| word.starts_with("--")) {
            assert!(help.contains(option), "the script has no {option}");
        }
    }

    let (before_publish, publish) = workflow.split_once("\n  publish:").expect("a publish job");
    assert!(publish.contains("environment: release"), "{publish}");
    // A tag push builds and stops: only the user's manual run publishes,
    // and it publishes the tag's own build, never a new one.
    let gate = publish
        .lines()
        .find_map(|line| line.trim().strip_prefix("if: "))
        .expect("the publish job is gated");
    assert_eq!(
        gate,
        "github.event_name == 'workflow_dispatch' && inputs.publish"
    );
    assert!(publish.contains("gh run download"), "{publish}");
    let check = publish
        .find("--verify \"$RUNNER_TEMP/release\" --commit \"$COMMIT\"")
        .expect("the publish job checks the download again");
    let create = publish
        .find("gh release create \"$TAG\" --repo \"$GITHUB_REPOSITORY\" --verify-tag")
        .expect("the publish job creates the release in this repository");
    assert!(check < create, "the check comes before the release");
    assert!(
        publish.contains("--notes-file \"notes/$TAG.md\""),
        "{publish}"
    );
    assert!(
        publish.contains("GH_TOKEN: ${{ github.token }}"),
        "{publish}"
    );
    for builds in ["--build-only", "--binaries", "upload-artifact"] {
        assert!(
            !publish.contains(builds),
            "the publish job never builds: {builds}"
        );
    }
    // Only the publish job may write; every other job reads.
    assert!(
        publish.contains("permissions:\n      contents: write\n"),
        "{publish}"
    );
    assert!(
        before_publish.contains("\npermissions:\n  contents: read\n"),
        "the workflow reads by default"
    );
    assert!(!before_publish.contains("contents: write"));
    for (at, _) in workflow.match_indices("secrets.") {
        assert!(
            workflow[at..].starts_with("secrets.RELEASE_DENYLIST"),
            "the only secret is the denylist: {}",
            workflow[at..].lines().next().unwrap()
        );
    }
    let version = workflow
        .split("\n  version:")
        .nth(1)
        .expect("a version job");
    assert!(
        version
            .lines()
            .take(3)
            .any(|line| line.trim() == "if: ${{ !inputs.publish }}"),
        "a publishing run builds nothing: {version}"
    );
}

// The release workflow assembles every target, the Windows builds among
// them, from the binaries its build jobs made: each user-memory-<target>.exe
// with its .exe.sha256 (the names install.ps1 downloads) and every
// installer, macOS and Linux and Windows. What it assembles is whole: the
// publish run's check passes on it. A Windows build that does not match its
// checksum stops the assembly.
#[test]
fn a_release_assembles_every_target_with_the_windows_builds() {
    if !has_zsh() {
        return;
    }
    let dir = tempdir().unwrap();
    let source = source_tree(dir.path());
    // No compiler here: only cargo for the docs test and cargo-about.
    fake_toolchain(dir.path());
    let bin = dir.path().join("no-rustc");
    fs::create_dir_all(&bin).unwrap();
    for name in ["cargo", "cargo-about"] {
        fs::copy(dir.path().join("toolchain").join(name), bin.join(name)).unwrap();
    }
    let path = format!("{}:{SYSTEM_PATH}", bin.display());
    let built = dir.path().join("built");
    fs::create_dir_all(&built).unwrap();
    for target in TARGETS {
        let name = asset(target);
        fs::write(built.join(&name), format!("build {target}")).unwrap();
        checksum(&built, &name);
    }
    let assemble = |out: &Path| {
        run(
            &source,
            dir.path(),
            &path,
            &[
                "--binaries",
                built.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
            &[],
        )
    };
    let release = dir.path().join("release");
    let assembled = assemble(&release);
    assert!(assembled.status.success(), "{}", text(&assembled).1);
    for file in INSTALLERS {
        assert_eq!(
            fs::read(release.join(file)).unwrap(),
            fs::read(repo_file(file)).unwrap(),
            "{file}"
        );
    }
    for target in TARGETS {
        let name = asset(target);
        assert_eq!(
            fs::read(release.join(&name)).unwrap(),
            fs::read(built.join(&name)).unwrap(),
            "{name}"
        );
        assert!(release.join(format!("{name}.sha256")).is_file(), "{name}");
    }
    assert!(
        release
            .join("user-memory-x86_64-pc-windows-msvc.exe")
            .is_file()
    );
    assert!(
        release
            .join("user-memory-aarch64-pc-windows-msvc.exe.sha256")
            .is_file()
    );
    let checked = run(
        &source,
        dir.path(),
        SYSTEM_PATH,
        &["--verify", release.to_str().unwrap()],
        &[],
    );
    assert!(checked.status.success(), "{}", text(&checked).1);

    fs::write(
        built.join("user-memory-aarch64-pc-windows-msvc.exe"),
        "tampered",
    )
    .unwrap();
    let tampered = assemble(&dir.path().join("tampered"));
    assert!(!tampered.status.success());
    assert!(
        text(&tampered)
            .1
            .contains("user-memory-aarch64-pc-windows-msvc.exe does not match its checksum"),
        "{}",
        text(&tampered).1
    );
}

/// The script of the publish job's step `name`, as release.yml runs it.
fn publish_job_step(name: &str) -> String {
    let workflow = fs::read_to_string(repo_file(".github/workflows/release.yml")).unwrap();
    let docs = yaml_rust2::YamlLoader::load_from_str(&workflow).unwrap();
    docs[0]["jobs"]["publish"]["steps"]
        .as_vec()
        .unwrap()
        .iter()
        .find(|step| step["name"].as_str() == Some(name))
        .and_then(|step| step["run"].as_str())
        .unwrap_or_else(|| panic!("the publish job has no step {name:?}"))
        .to_owned()
}

/// The publish job's step that creates the release.
fn publish_step() -> String {
    publish_job_step("Publish the release")
}

/// Runs the publish step as GitHub does (bash, stopping at the first
/// failure) in an assembled release, with a gh that logs its calls and
/// says whether the release already exists. Returns the run and the calls.
fn run_publish_step(dir: &Path, release_exists: bool) -> (Output, String) {
    let release = dir.join("release");
    fs::rename(assembled(dir), &release).unwrap();
    let calls = dir.join("gh.calls");
    let gh = format!(
        "gh() {{ printf '%s\\n' \"$*\" >> '{}'; if [ \"$1 $2\" = 'release view' ]; then return {}; fi; }}\n",
        calls.display(),
        if release_exists { 0 } else { 1 }
    );
    let out = Command::new("bash")
        .args(["--noprofile", "--norc", "-eo", "pipefail", "-c"])
        .arg(gh + &publish_step())
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .env("TAG", "v9.9.9")
        .env("GITHUB_REPOSITORY", "l7aromeo/user-memory")
        .env("RUNNER_TEMP", release.parent().unwrap())
        .current_dir(dir)
        .output()
        .unwrap();
    (out, fs::read_to_string(&calls).unwrap_or_default())
}

// A release that already exists is never replaced or added to: the step
// stops before it creates anything.
#[test]
fn the_publish_step_refuses_a_release_that_exists() {
    let dir = tempdir().unwrap();
    let (out, calls) = run_publish_step(dir.path(), true);
    assert!(!out.status.success());
    assert!(
        text(&out)
            .0
            .contains("::error::l7aromeo/user-memory already has the release v9.9.9"),
        "{}",
        text(&out).0
    );
    assert!(!calls.contains("release create"), "{calls}");
}

// Otherwise it creates the release from the existing tag, in this
// repository, with the notes as its text and every asset of the assembled
// release, the Windows builds among them.
#[test]
fn the_publish_step_creates_the_release_from_the_tag_with_every_asset() {
    let dir = tempdir().unwrap();
    let (out, calls) = run_publish_step(dir.path(), false);
    assert!(out.status.success(), "{}", text(&out).1);
    let create = calls
        .lines()
        .find(|line| line.starts_with("release create"))
        .expect("the release is created");
    let mut expected = [
        "release create v9.9.9 --repo l7aromeo/user-memory --verify-tag --title v9.9.9",
        "--notes-file notes/v9.9.9.md",
    ]
    .join(" ");
    for file in INSTALLERS.iter().chain(&["THIRD_PARTY_NOTICES.txt"]) {
        expected.push(' ');
        expected.push_str(file);
    }
    let mut assets: Vec<String> = TARGETS
        .iter()
        .flat_map(|target| [asset(target), format!("{}.sha256", asset(target))])
        .collect();
    assets.sort();
    for name in assets {
        expected.push(' ');
        expected.push_str(&name);
    }
    assert_eq!(create, expected);
}

/// A checkout as the publish job has it: `main` with v9.9.9 tagged on it
/// and `origin/main` pointing there, and v9.9.8 tagged on a commit that
/// never reached main.
fn tagged_checkout(dir: &Path) -> PathBuf {
    let repo = dir.join("checkout");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "release"]);
    git(&repo, &["tag", "v9.9.9"]);
    git(&repo, &["update-ref", "refs/remotes/origin/main", "main"]);
    git(&repo, &["switch", "-q", "-c", "side"]);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "never merged"],
    );
    git(&repo, &["tag", "v9.9.8"]);
    git(&repo, &["switch", "-q", "main"]);
    repo
}

/// What one run of the "Find the tag's build" step did.
struct Found {
    out: Output,
    calls: String,
    output: String,
    commit: String,
}

/// Runs the publish job's "Find the tag's build" step as GitHub does (bash,
/// stopping at the first failure) for `tag`, with a gh that logs its calls,
/// answers the runs query with `run` (nothing when empty, as gh's --jq
/// prints for no match) and downloads an artifact whose .tag is `built`.
fn find_the_build(tag: &str, run: &str, built: &str) -> Found {
    let dir = tempdir().unwrap();
    let repo = tagged_checkout(dir.path());
    let calls = dir.path().join("gh.calls");
    let output = dir.path().join("github-output");
    let gh = format!(
        r#"gh() {{
  printf '%s\n' "$*" >> '{calls}'
  case "$1 $2" in
    "api "*) if [ -n '{run}' ]; then printf '%s\n' '{run}'; fi ;;
    "run download") mkdir -p "$7" && printf '%s\n' '{built}' > "$7/.tag" ;;
    *) return 1 ;;
  esac
}}
"#,
        calls = calls.display()
    );
    let out = Command::new("bash")
        .args(["--noprofile", "--norc", "-eo", "pipefail", "-c"])
        .arg(gh + &publish_job_step("Find the tag's build"))
        .env_clear()
        .env("PATH", SYSTEM_PATH)
        .envs(identity())
        .env("TAG", tag)
        .env("GITHUB_REPOSITORY", "l7aromeo/user-memory")
        .env("RUNNER_TEMP", dir.path())
        .env("GITHUB_OUTPUT", &output)
        .current_dir(&repo)
        .output()
        .unwrap();
    Found {
        out,
        calls: fs::read_to_string(&calls).unwrap_or_default(),
        output: fs::read_to_string(&output).unwrap_or_default(),
        commit: git(&repo, &["rev-parse", "v9.9.9^{commit}"]),
    }
}

/// The step stopped for `reason`, and handed the publish step no commit.
fn assert_not_found(found: &Found, reason: &str) {
    assert!(!found.out.status.success(), "{}", text(&found.out).0);
    assert!(
        text(&found.out).0.contains(reason),
        "{}",
        text(&found.out).0
    );
    assert!(!found.output.contains("commit="), "{}", found.output);
}

// The tag on main, with a successful run of its own commit whose artifact
// says it built that tag: the step downloads that run's release and hands
// the commit on, which the publish check then requires the release to be
// built from.
#[test]
fn the_tags_own_checked_build_is_found() {
    let found = find_the_build("v9.9.9", "4242", "v9.9.9");
    assert!(found.out.status.success(), "{}", text(&found.out).0);
    assert_eq!(found.output, format!("commit={}\n", found.commit));
    let query = found
        .calls
        .lines()
        .find(|line| line.starts_with("api "))
        .expect("the step asks for the tag's runs");
    for part in [
        "repos/l7aromeo/user-memory/actions/workflows/release.yml/runs?",
        "event=push&branch=v9.9.9&status=success",
        &format!("select(.head_sha == \"{}\")", found.commit),
    ] {
        assert!(query.contains(part), "{part} not in {query}");
    }
    assert!(
        found
            .calls
            .contains("run download 4242 --name release --dir "),
        "{}",
        found.calls
    );
}

// A tag on a commit that never reached main is not published, and nothing
// is asked of GitHub for it.
#[test]
fn a_tag_not_on_main_is_refused() {
    let found = find_the_build("v9.9.8", "4242", "v9.9.8");
    assert_not_found(&found, "::error::v9.9.8 (");
    assert_not_found(&found, ") is not on main");
    assert!(found.calls.is_empty(), "{}", found.calls);
}

// Without a successful run of this workflow for the tag's commit there is
// nothing checked to publish: the step never downloads an artifact.
#[test]
fn a_tag_without_a_successful_run_is_refused() {
    let found = find_the_build("v9.9.9", "", "v9.9.9");
    assert_not_found(
        &found,
        "::error::no successful run of this workflow built v9.9.9",
    );
    assert!(!found.calls.contains("run download"), "{}", found.calls);
}

// An artifact that says it was built for another tag is not published.
#[test]
fn an_artifact_built_for_another_tag_is_refused() {
    let found = find_the_build("v9.9.9", "4242", "v9.9.7");
    assert_not_found(&found, "::error::run 4242 built v9.9.7, not v9.9.9");
}

#[test]
fn a_tag_that_does_not_exist_is_refused() {
    let found = find_the_build("v9.9.6", "4242", "v9.9.6");
    assert_not_found(&found, "::error::there is no tag v9.9.6");
    assert!(found.calls.is_empty(), "{}", found.calls);
}

#[test]
fn only_a_version_tag_is_published() {
    let found = find_the_build("main", "4242", "main");
    assert_not_found(
        &found,
        "::error::publish needs tag, a version tag such as v0.5.0, not 'main'",
    );
    assert!(found.calls.is_empty(), "{}", found.calls);
}

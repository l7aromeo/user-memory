//! The fake command-line programs (tests/fake/cli.rs) answer as the tests
//! that arrange them expect.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/fake.rs"]
mod fake;
#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;

use std::{fs, path::Path, process::Command};

/// What `program --version` prints and how it exits, run with `home` as its
/// state folder.
fn version(program: &Path, home: &Path) -> (String, Option<i32>) {
    let out = Command::new(program)
        .arg("--version")
        .sandboxed(home)
        .output()
        .unwrap();
    (String::from_utf8(out.stdout).unwrap(), out.status.code())
}

// Each agent fake prints a version setup accepts unless a test gives it
// another in `<name>.version`, which it prints as written; a `<name>.exit`
// still fails it first.
#[test]
fn the_agent_fakes_print_their_version() {
    for (name, default) in [
        ("claude", "2.1.200 (Claude Code)\n"),
        ("codex", "codex-cli 0.130.0\n"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let cli = home.path().join("cli");
        fs::create_dir(&cli).unwrap();
        let program = fake::fake(&cli, name);
        assert_eq!(
            version(&program, home.path()),
            (default.to_owned(), Some(0)),
            "{name}"
        );
        fs::write(
            home.path().join(format!("{name}.version")),
            "2.1.138 (Claude Code)",
        )
        .unwrap();
        assert_eq!(
            version(&program, home.path()),
            ("2.1.138 (Claude Code)\n".to_owned(), Some(0)),
            "{name}"
        );
        fs::write(cli.join(format!("{name}.exit")), "3").unwrap();
        assert_eq!(
            version(&program, home.path()),
            (String::new(), Some(3)),
            "{name}"
        );
    }
}

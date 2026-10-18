//! The production image builds from a narrow context (.dockerignore allows a few
//! paths, the Dockerfile copies them). Every file the release binary embeds with
//! `include_str!` must be inside that context, or `docker compose build` fails.

#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

use std::{
    fs,
    path::{Component, Path},
};

/// Repository-relative files embedded outside `#[cfg(test)]` code.
fn embedded_files() -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![Path::new("src").to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            // Test modules sit at the end of each file; only code before them ships.
            let shipped = text.split("#[cfg(test)]").next().unwrap();
            for part in shipped.split("include_str!(\"").skip(1) {
                let relative = part.split('"').next().unwrap();
                let resolved = path.parent().unwrap().join(relative);
                // By component, not by '/': on Windows the folder part
                // comes back with '\'. The Dockerfile names folders with '/'.
                let mut parts: Vec<String> = Vec::new();
                for component in resolved.components() {
                    match component {
                        Component::ParentDir => {
                            parts.pop();
                        }
                        Component::Normal(piece) => {
                            parts.push(piece.to_string_lossy().into_owned())
                        }
                        _ => {}
                    }
                }
                files.push(parts.join("/"));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn every_embedded_file_is_in_the_docker_build_context() {
    let files = embedded_files();
    assert!(
        files.iter().any(|f| f.starts_with("skills/")),
        "expected the skill to be embedded: {files:?}"
    );
    let dockerfile = fs::read_to_string("Dockerfile").unwrap();
    let ignore = fs::read_to_string(".dockerignore").unwrap();
    for file in files {
        let top = file.split('/').next().unwrap();
        assert!(
            dockerfile.contains(&format!("COPY {top} ./{top}")),
            "Dockerfile does not copy {top}/ (needed for {file})"
        );
        assert!(
            ignore.lines().any(|l| l == format!("!{top}/**")),
            ".dockerignore does not re-include {top}/** (needed for {file})"
        );
    }
}

// A binary whose source the context leaves out cannot be built there, and
// cargo builds every binary unless told one: the image builds only the server.
#[test]
fn the_image_builds_only_the_server_binary() {
    let dockerfile = fs::read_to_string("Dockerfile").unwrap();
    let builds: Vec<&str> = dockerfile
        .lines()
        .filter(|line| line.contains("cargo build"))
        .collect();
    assert_eq!(
        builds,
        ["RUN cargo build --release --locked --bin user-memory"],
        "{dockerfile}"
    );
    let manifest = fs::read_to_string("Cargo.toml").unwrap();
    assert!(
        manifest.contains("path = \"tests/fake/cli.rs\""),
        "the fake CLI moved; check it is still outside the image's build"
    );
}

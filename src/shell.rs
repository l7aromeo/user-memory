//! Text a shell runs: the hook commands setup writes and the commands
//! user-memory prints for the user to run, for a POSIX shell or, on
//! Windows, PowerShell.

use std::path::Path;

/// `path` as one word for a POSIX shell: single-quoted, with each single
/// quote in it closed, escaped and reopened. Always quoted, so a hook
/// command keeps one form whatever the path.
pub fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

/// The characters PowerShell reads as a single quote: the ASCII one and
/// the typographic ones (U+2018 to U+201B).
const POWERSHELL_QUOTES: [char; 5] = ['\'', '\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'];

/// `path` as one word for PowerShell: single-quoted, so nothing in it is
/// expanded, with each quote character in it doubled, which PowerShell
/// reads as that character.
pub fn powershell_quote(path: &Path) -> String {
    let mut quoted = String::from("'");
    for c in path.to_string_lossy().chars() {
        if POWERSHELL_QUOTES.contains(&c) {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_one_shell_word() {
        assert_eq!(quote(Path::new("/a/b-c_d.e")), "'/a/b-c_d.e'");
        assert_eq!(quote(Path::new("/a b/it's")), r#"'/a b/it'"'"'s'"#);
    }

    #[test]
    fn a_path_is_one_powershell_word() {
        assert_eq!(
            powershell_quote(Path::new(r"C:\a b\$x [1]")),
            r"'C:\a b\$x [1]'"
        );
        assert_eq!(
            powershell_quote(Path::new("C:\\it's \u{2018}a\u{2019} \u{201a}b\u{201b}")),
            "'C:\\it''s \u{2018}\u{2018}a\u{2019}\u{2019} \u{201a}\u{201a}b\u{201b}\u{201b}'"
        );
    }
}

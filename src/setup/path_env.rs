//! The user PATH on Windows: setup adds the bin folder, so a new terminal
//! runs `user-memory`, and uninstall removes the entry setup added. The
//! rules for the entry list are pure functions, tested on every platform;
//! reading and writing the registry is Windows-only.

use std::{ffi::OsString, path::Path};

/// An environment the rules read: a variable's value by name.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;
/// Whether two paths name one folder.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
type Same<'a> = &'a dyn Fn(&Path, &Path) -> bool;

/// `text` with each `%NAME%` replaced by that variable, as Windows expands
/// a REG_EXPAND_SZ value; an unknown or unterminated one stays as written.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
fn expand(text: &str, env: Env) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        match env(&after[..end]).filter(|_| end > 0) {
            Some(value) => {
                out.push_str(&value.to_string_lossy());
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether the PATH entry `entry` names `dir`: compared after expanding it,
/// without the quotes or trailing separator Windows also accepts.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
fn names(entry: &str, dir: &Path, env: Env, same: Same) -> bool {
    let expanded = expand(entry.trim(), env);
    let entry = expanded
        .trim_matches('"')
        .trim_end_matches(['\\', '/'])
        .trim();
    !entry.is_empty() && same(Path::new(entry), dir)
}

/// The PATH value `value` with `dir` appended at the end, so the user's own
/// entries win, as `~/.local/bin` comes after them on Unix; `None` when
/// an entry already names it.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
pub(super) fn with_entry(value: &str, dir: &Path, env: Env, same: Same) -> Option<String> {
    if value.split(';').any(|entry| names(entry, dir, env, same)) {
        return None;
    }
    let dir = dir.display();
    Some(if value.is_empty() {
        dir.to_string()
    } else if value.ends_with(';') {
        format!("{value}{dir}")
    } else {
        format!("{value};{dir}")
    })
}

/// The PATH value `value` without the entries naming `dir`, every other
/// entry as it was; `None` when none names it.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
pub(super) fn without_entry(value: &str, dir: &Path, env: Env, same: Same) -> Option<String> {
    let entries: Vec<&str> = value.split(';').collect();
    let kept: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| !names(entry, dir, env, same))
        .collect();
    (kept.len() != entries.len()).then(|| kept.join(";"))
}

/// The key under HKEY_CURRENT_USER that holds the user's environment.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry reads it on Windows; tests run it everywhere"
    )
)]
const KEY: &str = "Environment";

/// A test-only variable naming another key under HKEY_CURRENT_USER to edit
/// instead of [`KEY`], so a test run never changes the user's real PATH.
/// Only debug builds, which the tests run, read it.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry reads it on Windows; tests run it everywhere"
    )
)]
const KEY_OVERRIDE: &str = "USER_MEMORY_ENVIRONMENT_KEY";

/// The key under HKEY_CURRENT_USER that setup edits: [`KEY`] in a release
/// build whatever the environment says; in a debug build the key `given`
/// names, when one is.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
fn key_name(debug: bool, given: Option<String>) -> String {
    given
        .filter(|name| debug && !name.is_empty())
        .unwrap_or_else(|| KEY.to_owned())
}

/// Whether a change to the key `name` is announced to running programs:
/// only [`KEY`], whose PATH they read (as Windows names keys, in any case).
/// The tests' key changes nothing anyone listens to, so a test run never
/// broadcasts to the desktop.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the registry calls it on Windows; tests run it everywhere"
    )
)]
fn announced(name: &str) -> bool {
    name.eq_ignore_ascii_case(KEY)
}

/// The user PATH in the registry, `HKEY_CURRENT_USER\Environment\Path`.
#[cfg(windows)]
mod registry {
    use super::{KEY_OVERRIDE, announced, key_name, with_entry, without_entry};
    use crate::{paths, platform::windows::broadcast_environment_change};
    use std::{env, path::Path};
    use windows_registry::{CURRENT_USER, Key, Type};

    const VALUE: &str = "Path";

    /// The key setup edits, [`key_name`]'s, read from the environment only
    /// in a debug build; and whether a change to it is [`announced`].
    fn key() -> windows_registry::Result<(Key, bool)> {
        let given = cfg!(debug_assertions)
            .then(|| env::var(KEY_OVERRIDE).ok())
            .flatten();
        let name = key_name(cfg!(debug_assertions), given);
        Ok((CURRENT_USER.create(&name)?, announced(&name)))
    }

    /// The error a missing value reads as: HRESULT_FROM_WIN32
    /// (ERROR_FILE_NOT_FOUND).
    const NOT_FOUND: i32 = 0x8007_0002_u32 as i32;

    /// The value and whether it is REG_EXPAND_SZ; a user without a PATH
    /// value gets one of that type, as Windows writes it.
    fn read(key: &Key) -> anyhow::Result<(String, bool)> {
        match key.get_type(VALUE) {
            Ok(Type::String) => Ok((key.get_string(VALUE)?, false)),
            Ok(Type::ExpandString) => Ok((key.get_string(VALUE)?, true)),
            Ok(other) => anyhow::bail!("the user PATH has an unexpected registry type ({other:?})"),
            Err(err) if err.code().0 == NOT_FOUND => Ok((String::new(), true)),
            Err(err) => Err(err.into()),
        }
    }

    /// Writes `value` with its type kept, then, when `announce`, tells
    /// running programs, so a new terminal sees it.
    fn write(key: &Key, value: &str, expand: bool, announce: bool) -> anyhow::Result<()> {
        if expand {
            key.set_expand_string(VALUE, value)?;
        } else {
            key.set_string(VALUE, value)?;
        }
        if announce {
            broadcast_environment_change();
        }
        Ok(())
    }

    /// Adds `dir` to the user PATH; whether it was missing.
    pub(in crate::setup) fn add(dir: &Path) -> anyhow::Result<bool> {
        let (key, announce) = key()?;
        let (value, expand) = read(&key)?;
        let Some(value) = with_entry(&value, dir, &|name| env::var_os(name), &paths::same) else {
            return Ok(false);
        };
        write(&key, &value, expand, announce)?;
        Ok(true)
    }

    /// Removes `dir` from the user PATH; whether it was there.
    pub(in crate::setup) fn remove(dir: &Path) -> anyhow::Result<bool> {
        let (key, announce) = key()?;
        let (value, expand) = read(&key)?;
        let Some(value) = without_entry(&value, dir, &|name| env::var_os(name), &paths::same)
        else {
            return Ok(false);
        };
        write(&key, &value, expand, announce)?;
        Ok(true)
    }
}

#[cfg(windows)]
pub(super) use registry::{add, remove};

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = r"C:\Users\dev\AppData\Local\Programs\user-memory\bin";

    fn env(name: &str) -> Option<OsString> {
        match name {
            "LOCALAPPDATA" => Some(r"C:\Users\dev\AppData\Local".into()),
            "USERPROFILE" => Some(r"C:\Users\dev".into()),
            _ => None,
        }
    }

    /// Windows compares paths ignoring case; the rules take that as given.
    fn same(a: &Path, b: &Path) -> bool {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }

    /// A release binary edits the user's environment whatever the
    /// environment says; only a debug build, as the tests run, takes the
    /// tests' key.
    #[test]
    fn only_a_debug_build_edits_another_key() {
        let tests = Some(r"Software\user-memory-test-1".to_owned());
        assert_eq!(key_name(false, tests.clone()), "Environment");
        assert_eq!(key_name(false, None), "Environment");
        assert_eq!(key_name(true, tests), r"Software\user-memory-test-1");
        assert_eq!(key_name(true, Some(String::new())), "Environment");
        assert_eq!(key_name(true, None), "Environment");
        assert_eq!(KEY_OVERRIDE, "USER_MEMORY_ENVIRONMENT_KEY");
    }

    /// Only a change to the user's environment is broadcast: never one to
    /// the key a test run edits instead.
    #[test]
    fn only_the_users_environment_is_announced() {
        let tests = Some(r"Software\user-memory-test-1".to_owned());
        assert!(announced(&key_name(false, tests.clone())));
        assert!(!announced(&key_name(true, tests)));
        assert!(announced(&key_name(true, None)));
        assert!(announced("ENVIRONMENT"));
    }

    #[test]
    fn variables_expand_as_windows_expands_them() {
        assert_eq!(
            expand(r"%LOCALAPPDATA%\Programs", &env),
            r"C:\Users\dev\AppData\Local\Programs"
        );
        assert_eq!(expand("%NOPE%;%USERPROFILE%", &env), r"%NOPE%;C:\Users\dev");
        assert_eq!(expand("100% sure", &env), "100% sure");
        assert_eq!(expand("%%", &env), "%%");
    }

    #[test]
    fn the_bin_folder_is_appended_once() {
        let bin = Path::new(BIN);
        assert_eq!(with_entry("", bin, &env, &same).as_deref(), Some(BIN));
        assert_eq!(
            with_entry(r"C:\tools", bin, &env, &same),
            Some(format!(r"C:\tools;{BIN}"))
        );
        assert_eq!(
            with_entry(r"C:\tools;", bin, &env, &same),
            Some(format!(r"C:\tools;{BIN}"))
        );
        // Already there: as written, through a variable, in another case,
        // quoted or with a trailing separator.
        for present in [
            format!(r"C:\tools;{BIN}"),
            r"%LOCALAPPDATA%\Programs\user-memory\bin;C:\tools".to_owned(),
            BIN.to_uppercase(),
            format!("\"{BIN}\""),
            format!(r"{BIN}\"),
        ] {
            assert_eq!(with_entry(&present, bin, &env, &same), None, "{present}");
        }
    }

    #[test]
    fn only_the_entries_naming_the_bin_folder_are_removed() {
        let bin = Path::new(BIN);
        assert_eq!(
            without_entry(&format!(r"C:\tools;{BIN};;D:\x"), bin, &env, &same).as_deref(),
            Some(r"C:\tools;;D:\x")
        );
        assert_eq!(
            without_entry(
                r"%LOCALAPPDATA%\Programs\user-memory\bin;C:\tools",
                bin,
                &env,
                &same
            )
            .as_deref(),
            Some(r"C:\tools")
        );
        assert_eq!(without_entry(BIN, bin, &env, &same).as_deref(), Some(""));
        assert_eq!(without_entry(r"C:\tools", bin, &env, &same), None);
    }
}

//! Files and folders only this user can read: the store, its backups and
//! exports, session snapshots, tokens and receipts.
//!
//! On Unix that is mode 0600 for a file and 0700 for a folder. On Windows it
//! is a protected access list with one entry, for the current user
//! ([`crate::platform::windows`]); a folder's entry is inherited, so what is
//! created inside starts private. Callers never handle mode bits or access
//! lists themselves.
//!
//! On Windows a new file carries its folder's inherited access list from
//! creation until [`create_file`] restricts it, before it returns: callers
//! write their secret only after that.

#[cfg(windows)]
use crate::platform::windows;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
};

/// Creates `dir` and any missing parents; each folder it creates is
/// private, and existing ones are left as they are. Succeeds when `dir`
/// already exists.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(windows)]
    {
        let missing: Vec<&Path> = dir
            .ancestors()
            .take_while(|folder| !folder.as_os_str().is_empty() && !folder.exists())
            .collect();
        fs::create_dir_all(dir)?;
        missing
            .into_iter()
            .rev()
            .try_for_each(|folder| windows::restrict(folder, true))
    }
}

/// Opens `path` with `options`; a file it creates is private before this
/// returns. An existing file keeps its permissions on Unix.
pub fn create_file(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).open(path)
    }
    #[cfg(windows)]
    {
        let file = options.open(path)?;
        windows::restrict(path, false)?;
        Ok(file)
    }
}

/// Makes the existing file or folder `path` private, such as a file
/// written elsewhere and renamed into place.
pub fn restrict(path: &Path) -> io::Result<()> {
    let folder = fs::metadata(path)?.is_dir();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if folder { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    windows::restrict(path, folder)
}

/// Whether only this user can read or change `path`: no group or other
/// permission bits on Unix; on Windows, a protected access list whose one
/// entry allows the current user.
pub fn is_private(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    windows::is_private(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_folders_and_files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("a/b");
        create_dir(&folder).unwrap();
        create_dir(&folder).unwrap();
        for created in [dir.path().join("a"), folder.clone()] {
            assert!(is_private(&created).unwrap(), "{}", created.display());
        }
        let file = folder.join("secret");
        create_file(OpenOptions::new().write(true).create_new(true), &file).unwrap();
        assert!(is_private(&file).unwrap());
        assert!(
            create_file(OpenOptions::new().write(true).create_new(true), &file).is_err(),
            "the caller's options hold"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_modes_are_0700_and_0600_and_existing_folders_keep_theirs() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        create_dir(&dir.path().join("a/b")).unwrap();
        assert_eq!(mode(dir.path()), 0o755);
        assert_eq!(mode(&dir.path().join("a")), 0o700);
        assert_eq!(mode(&dir.path().join("a/b")), 0o700);
        let file = dir.path().join("f");
        create_file(OpenOptions::new().write(true).create_new(true), &file).unwrap();
        assert_eq!(mode(&file), 0o600);
        // An existing file opened again keeps its mode.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        create_file(OpenOptions::new().write(true), &file).unwrap();
        assert_eq!(mode(&file), 0o640);
        assert!(!is_private(&file).unwrap());
        restrict(&file).unwrap();
        assert_eq!(mode(&file), 0o600);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o750)).unwrap();
        restrict(dir.path()).unwrap();
        assert_eq!(mode(dir.path()), 0o700);
        for (bits, private) in [(0o600, true), (0o400, true), (0o604, false), (0o620, false)] {
            fs::set_permissions(&file, fs::Permissions::from_mode(bits)).unwrap();
            assert_eq!(is_private(&file).unwrap(), private, "{bits:o}");
        }
    }

    #[cfg(windows)]
    mod on_windows {
        use super::*;
        use std::process::Command;

        /// Grants the built-in Users group read access to `path`, by SID so
        /// it holds in any display language.
        fn widen(path: &Path) {
            let status = Command::new("icacls")
                .arg(path)
                .args(["/grant", "*S-1-5-32-545:R"])
                .output()
                .unwrap();
            assert!(status.status.success(), "{status:?}");
        }

        #[test]
        fn a_file_created_inside_a_private_folder_is_private() {
            let dir = tempfile::tempdir().unwrap();
            let folder = dir.path().join("private");
            create_dir(&folder).unwrap();
            let inside = folder.join("plain");
            fs::write(&inside, "x").unwrap();
            assert!(is_private(&inside).unwrap());
        }

        #[test]
        fn restrict_makes_a_wide_file_private_and_widening_is_seen() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("wide");
            fs::write(&file, "x").unwrap();
            widen(&file);
            assert!(!is_private(&file).unwrap());
            restrict(&file).unwrap();
            assert!(is_private(&file).unwrap());
            widen(&file);
            assert!(!is_private(&file).unwrap());
        }

        #[test]
        fn the_temporary_folder_is_not_private() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("plain");
            fs::write(&file, "x").unwrap();
            assert!(!is_private(&file).unwrap());
        }
    }
}

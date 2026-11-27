//! The Windows system calls behind private files ([`crate::private`]), the
//! PATH change notice, the console check, the file id that tells `serve`
//! its binary was replaced and the rename that waits out a passing hold:
//! the only `unsafe` code in user-memory.
//!
//! A private file or folder has a protected access list (DACL) with one
//! entry: full access for the user this process runs as. "Protected" drops
//! the entries a file inherits from its folder (SYSTEM, Administrators), so
//! the result matches 0600 and 0700 on Unix. A folder's entry is inherited
//! by what is created inside it. As root can on Unix, an administrator can
//! still take ownership of the file and change its access list.
//!
//! Every call that hands back memory (the access list, the security
//! descriptor) is freed by a guard on drop, and every handle is closed the
//! same way, so no early return leaks one.

#![allow(unsafe_code, reason = "the one module that calls Windows directly")]
#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    ffi::c_void,
    io,
    path::Path,
    ptr,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_SUCCESS, HANDLE,
            HLOCAL, LPARAM, LocalFree, WIN32_ERROR, WPARAM,
        },
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{
                EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT,
                SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
                TRUSTEE_IS_USER, TRUSTEE_W,
            },
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetTokenInformation, NO_INHERITANCE, OBJECT_INHERIT_ACE,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ALL_ACCESS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        },
        System::{
            Console::GetConsoleWindow,
            SystemServices::ACCESS_ALLOWED_ACE_TYPE,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
        UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        },
    },
    core::{HSTRING, PWSTR, w},
};

/// How long the PATH change notice waits, in milliseconds, for each window
/// and for all of them together: a hung or slow window must not hold up
/// setup. Five seconds, as Windows' own environment variables dialog waits
/// for each window.
const NOTICE_WAIT_MS: u32 = 5000;

/// Gives `path` a protected access list allowing only the current user;
/// a `folder`'s entry is inherited by files and folders created inside it.
pub fn restrict(path: &Path, folder: bool) -> io::Result<()> {
    let user = User::current()?;
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS.0,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if folder {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            NO_INHERITANCE
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            // With TRUSTEE_IS_SID the name field carries the SID.
            ptstrName: PWSTR(user.sid().0.cast()),
        },
    };
    let mut acl: *mut ACL = ptr::null_mut();
    // SAFETY: one entry, whose SID `user` keeps alive through the call; no
    // old list to merge; `acl` is a valid out-pointer, which receives a
    // list the system allocated and `Local` frees.
    check(unsafe { SetEntriesInAclW(Some(&[entry]), None, &mut acl) })?;
    let acl = Local(acl.cast());
    let name = HSTRING::from(path);
    // SAFETY: `name` is a NUL-terminated wide string and `acl` a valid
    // access list, both alive through the call; no owner, group or audit
    // list is passed, so only the access list changes.
    check(unsafe {
        SetNamedSecurityInfoW(
            &name,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(acl.0.cast()),
            None,
        )
    })
}

/// Whether `path`'s access list has exactly one entry, allowing the current
/// user. A file inside a private folder qualifies through the entry it
/// inherits; a missing (null) list, which allows everyone, does not.
pub fn is_private(path: &Path) -> io::Result<bool> {
    let user = User::current()?;
    let name = HSTRING::from(path);
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `name` is a NUL-terminated wide string alive through the
    // call; `dacl` and `descriptor` are valid out-pointers. The system
    // allocates the descriptor, which `Local` frees, and points `dacl`
    // inside it.
    check(unsafe {
        GetNamedSecurityInfoW(
            &name,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    })?;
    let _descriptor = Local(descriptor.0);
    if dacl.is_null() {
        return Ok(false);
    }
    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` points into the descriptor, alive until `_descriptor`
    // drops; `size` is the structure AclSizeInformation fills, and its
    // length is passed with it.
    unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(io::Error::other)?,
            AclSizeInformation,
        )
    }?;
    if size.AceCount != 1 {
        return Ok(false);
    }
    let mut ace: *mut c_void = ptr::null_mut();
    // SAFETY: index 0 exists, as the list holds one entry; `ace` is a valid
    // out-pointer, which receives a pointer into the list.
    unsafe { GetAce(dacl, 0, &mut ace) }?;
    let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
    // SAFETY: every entry starts with its header, which says its type.
    let kind = unsafe { (*ace).Header.AceType };
    if u32::from(kind) != ACCESS_ALLOWED_ACE_TYPE {
        return Ok(false);
    }
    // SAFETY: an access-allowed entry holds its SID from `SidStart` on,
    // inside the live list; `user` keeps the other SID alive.
    let sid = PSID(unsafe { &raw mut (*ace).SidStart }.cast());
    // SAFETY: both SIDs are valid for the call; EqualSid fails when they
    // differ, without reading past either.
    Ok(unsafe { EqualSid(sid, user.sid()) }.is_ok())
}

/// Tells running programs that the user's environment changed, so a new
/// terminal sees the PATH setup wrote; best effort, as a window that does
/// not answer in time is skipped. The broadcast waits on each window in
/// turn, so it runs on a thread of its own, and setup goes on after
/// [`NOTICE_WAIT_MS`] in all; the thread ends with the process. The
/// asynchronous send functions are no way round it: they refuse a system
/// message whose parameter is a pointer, as this one's is.
pub fn broadcast_environment_change() {
    let (done, finished) = mpsc::channel();
    let notice = thread::Builder::new().spawn(move || {
        // SAFETY: the message's parameter points to a static NUL-terminated
        // wide string, which outlives the call; no result is read back.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                WPARAM(0),
                LPARAM(w!("Environment").as_ptr() as isize),
                SMTO_ABORTIFHUNG,
                NOTICE_WAIT_MS,
                None,
            )
        };
        let _ = done.send(());
    });
    if notice.is_ok() {
        let _ = finished.recv_timeout(Duration::from_millis(NOTICE_WAIT_MS.into()));
    }
}

/// The file id of `path`: its volume's serial number and its file index,
/// which a file keeps through a rename and a replacement never shares. A
/// running program's file opens too: the handle asks only to read its
/// attributes and shares everything.
pub fn file_id(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is `file`'s, open for the whole call, and `info`
    // is a valid out-pointer the call fills.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

/// The first pause before a rename refused for a sharing violation is
/// tried again: a program that opened the file for a moment is usually done
/// within a few milliseconds.
const RENAME_PAUSE: Duration = Duration::from_millis(10);

/// The longest pause between two tries, as each pause doubles: short
/// enough that a rename follows soon after the holder lets go.
const RENAME_PAUSE_MAX: Duration = Duration::from_millis(250);

/// Renames `from` to `to`, trying again for up to `wait` while another
/// program has either file open without sharing delete (a sharing
/// violation): a program may open a new or newly started executable for a
/// moment. Any other error, access denied among them, is returned at once.
pub fn rename(from: &Path, to: &Path, wait: Duration) -> io::Result<()> {
    retry_shared(wait, || std::fs::rename(from, to))
}

/// Deletes `path`, trying again for up to `wait` while another program has
/// it open without sharing delete, as [`rename`] does.
pub fn remove_file(path: &Path, wait: Duration) -> io::Result<()> {
    retry_shared(wait, || std::fs::remove_file(path))
}

/// Whether Windows refused to open a file that is there: a name being
/// deleted, from the moment a delete marks the file until the handle that
/// marked it closes (access denied), or a file another program opened
/// without sharing (a sharing violation). Both pass once that handle closes.
pub fn refused(err: &io::Error) -> bool {
    [ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION]
        .iter()
        .any(|code| err.raw_os_error() == i32::try_from(code.0).ok())
}

/// Runs `step` until it ends in anything but a sharing violation, or `wait`
/// has passed, pausing longer between tries as [`RENAME_PAUSE`] and
/// [`RENAME_PAUSE_MAX`] say.
pub(crate) fn retry_shared(
    wait: Duration,
    mut step: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let deadline = Instant::now() + wait;
    let mut pause = RENAME_PAUSE;
    loop {
        let err = match step() {
            Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32) => err,
            done => return done,
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(err);
        }
        thread::sleep(pause.min(left));
        pause = (pause * 2).min(RENAME_PAUSE_MAX);
    }
}

/// Whether this process's console has a window someone can type in, so a
/// question asked on CONIN$ can be answered. A hidden console, as a program
/// started with CREATE_NO_WINDOW has (an agent's shell starts its commands
/// so), has none, though its CONIN$ opens and a read from it waits for
/// ever. A pseudoconsole (ConPTY) has one, so Windows Terminal, VS Code and
/// OpenSSH sessions are still asked.
pub fn console_window() -> bool {
    // SAFETY: no arguments and no pointers; it returns the console's window
    // or null.
    !unsafe { GetConsoleWindow() }.is_invalid()
}

/// The user this process runs as, held as the token information that
/// contains their SID.
struct User {
    /// The TOKEN_USER structure and the SID it points to; `u64` cells keep
    /// it aligned for the structure.
    buffer: Vec<u64>,
}

impl User {
    fn current() -> io::Result<Self> {
        let mut token = HANDLE::default();
        // SAFETY: GetCurrentProcess's pseudo-handle needs no closing;
        // `token` is a valid out-pointer, closed by `Token`.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }?;
        let token = Token(token);
        let mut length = 0;
        // SAFETY: without a buffer the call only reports the length it
        // needs, into `length`; it fails for the missing buffer.
        let sized = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
        if let Err(err) = sized
            && length == 0
        {
            return Err(err.into());
        }
        let cells = (length as usize).div_ceil(size_of::<u64>());
        let mut buffer = vec![0u64; cells];
        // SAFETY: `buffer` holds at least `length` bytes, aligned for
        // TOKEN_USER, and stays alive through the call.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                length,
                &mut length,
            )
        }?;
        Ok(Self { buffer })
    }

    fn sid(&self) -> PSID {
        // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of
        // `buffer`, whose SID pointer points inside `buffer`, which `self`
        // owns and never changes.
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// A token handle, closed on drop.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcessToken and is closed
        // once, here. A failed close leaves nothing to undo.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Memory the system allocated for a call's result, freed on drop.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: the pointer came from the system's local heap (or is
        // null, which LocalFree ignores) and is freed once, here.
        unsafe { LocalFree(Some(HLOCAL(self.0))) };
    }
}

/// `Ok` for ERROR_SUCCESS, else the error the code names.
fn check(code: WIN32_ERROR) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code.0 as i32))
    }
}

/// Marks `path` for deletion through a handle it keeps open, as a delete
/// does between marking a file and closing it: until the returned file is
/// dropped, every open of the name is refused, and then the name is gone.
#[cfg(test)]
pub fn pending_delete(path: &Path) -> std::fs::File {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(DELETE.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .open(path)
        .expect("the test's file is there");
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>())
        .expect("a one-byte structure's size fits a u32");
    // SAFETY: the handle is `file`'s, open with delete access for the whole
    // call, and `info` is a FILE_DISPOSITION_INFO of `size` bytes, which the
    // call only reads.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            ptr::from_ref(&info).cast::<c_void>(),
            size,
        )
    }
    .expect("a file this test made can be marked for deletion");
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use windows::Win32::{
        Foundation::{HWND, LRESULT},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_WNDPROC, GetMessageW, MSG,
            SetWindowLongPtrW, WINDOW_EX_STYLE, WS_OVERLAPPED,
        },
    };

    /// How long each test window takes to handle the change notice: less
    /// than the wait for each window, so the broadcast waits for it in full.
    const SLOW: Duration = Duration::from_secs(3);
    /// How many such windows: together they take longer than the total
    /// limit plus [`ROOM`], so only that limit ends the broadcast in time.
    const SLOW_WINDOWS: u32 = 3;
    /// What the broadcast may take past its limit: starting its thread and
    /// a loaded machine's scheduling.
    const ROOM: Duration = Duration::from_secs(2);

    /// A window procedure, as Windows calls it.
    type WindowProcedure = extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

    extern "system" fn slow(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if message == WM_SETTINGCHANGE {
            thread::sleep(SLOW);
            return LRESULT(0);
        }
        // SAFETY: the arguments are the ones Windows passed for `window`.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// Hidden top-level windows, each [`SLOW`] to answer the notice, on a
    /// thread that serves their messages until the test process ends;
    /// returns once they exist. Any later broadcast in the same process
    /// waits on them too, which nextest's process per test avoids.
    fn slow_windows() {
        let (ready, created) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..SLOW_WINDOWS {
                // SAFETY: a system class; static names; no parent, menu,
                // module or creation data.
                let window = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("STATIC"),
                        w!(""),
                        WS_OVERLAPPED,
                        0,
                        0,
                        0,
                        0,
                        None,
                        None,
                        None,
                        None,
                    )
                }
                .unwrap();
                let procedure: WindowProcedure = slow;
                // SAFETY: the window is this thread's own and alive; `slow`
                // is a window procedure for the rest of the process.
                unsafe { SetWindowLongPtrW(window, GWLP_WNDPROC, procedure as usize as isize) };
            }
            ready.send(()).unwrap();
            let mut message = MSG::default();
            // SAFETY: `message` is a valid out-pointer for this thread's
            // queue, and is dispatched as read.
            while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
                // SAFETY: as above.
                unsafe { DispatchMessageW(&message) };
            }
        });
        created.recv().unwrap();
    }

    /// Windows that are slow but not hung each hold the broadcast up to
    /// the wait for each window; the total limit still ends it in time.
    #[test]
    fn the_environment_broadcast_ends_within_its_limit() {
        slow_windows();
        let start = Instant::now();
        broadcast_environment_change();
        let took = start.elapsed();
        let limit = Duration::from_millis(NOTICE_WAIT_MS.into());
        // A broadcast that never reached the slow windows (another desktop,
        // a window station without them) would pass the limit for nothing.
        assert!(
            took >= SLOW,
            "the broadcast took {took:?}; it never waited on a slow window"
        );
        assert!(
            took < limit + ROOM,
            "the broadcast took {took:?}, past its {limit:?} limit"
        );
    }

    // An open of a name being deleted, or of a file opened without
    // sharing, is refused, and `refused` names both; once the other handle
    // closes, the name opens again or is gone.
    #[test]
    fn an_open_refused_while_another_handle_holds_the_name_is_named() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mark");
        std::fs::write(&path, "named").unwrap();
        let held = pending_delete(&path);
        let refused_open = std::fs::read(&path).unwrap_err();
        assert!(refused(&refused_open), "{refused_open}");
        drop(held);
        assert_eq!(
            std::fs::read(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::write(&path, "named").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let shared = std::fs::read(&path).unwrap_err();
        assert!(refused(&shared), "{shared}");
        drop(held);
        assert_eq!(std::fs::read(&path).unwrap(), b"named");
        let missing = std::fs::read(dir.path().join("none")).unwrap_err();
        assert!(!refused(&missing), "{missing}");
    }
}

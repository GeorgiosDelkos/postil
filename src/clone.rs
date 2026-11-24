//! Copy-on-write clones of whole folders, which give a thread's worktree the engineer's build
//! folders without copying them (docs/design.md, "Worktrees").
//!
//! The crate's one use of `unsafe`: a call to `clonefile(2)`, which clones a folder and
//! everything in it in one system call. Measured on 2026-11-24 with a 4.4 GB, 193,000-file
//! `target/`: 3 s, against 34 s for `cp -c -R`, which clones file by file, and no disk used
//! until a block changes. The man page discourages cloning folders this way and points to
//! `copyfile(3)`, which walks the tree as `cp` does; the caller falls back to `cp -c -R` when
//! this call fails for another reason than a volume that cannot clone, and skips the folder
//! when it cannot.
#![cfg_attr(
    target_os = "macos",
    expect(
        unsafe_code,
        reason = "one FFI call to clonefile(2); the owner allowed this module on 2026-11-24"
    )
)]

use std::io;
use std::path::Path;

/// Clones the folder `from` to `to`, which must not exist, with every file, folder and
/// symlink in it. Both must be on one APFS volume. A symlink inside the folder is cloned as a
/// link, not followed, cross-volume links included (tested 2026-11-24 on macOS 27.2).
///
/// # Errors
///
/// The system's error: `EXDEV` across volumes, `EEXIST` when `to` exists, `ENOTSUP` on a file
/// system without clones, and `Unsupported` off macOS.
#[cfg(target_os = "macos")]
pub(crate) fn clone_folder(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        /// `int clonefile(const char *src, const char *dst, uint32_t flags);`, from
        /// `<sys/clonefile.h>` in libSystem, which every macOS program links.
        fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
    }
    /// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`: a symlink at `src` is cloned as a link.
    /// The caller passes folders only; this keeps a link that replaced one since from being
    /// followed out of the repository.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: `from` and `to` are `CString`s that live until this statement ends, so both
    // pointers are valid, NUL-terminated strings for the whole call; `clonefile` only reads
    // them and keeps neither. The flags are a documented value.
    let status = unsafe { clonefile(from.as_ptr(), to.as_ptr(), CLONE_NOFOLLOW) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Off macOS there is no `clonefile`: the caller skips the folder, as on a volume without
/// clones.
#[cfg(not(target_os = "macos"))]
pub(crate) fn clone_folder(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::testing::TempDir;

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(miri, ignore)] // clonefile is an FFI call, which miri cannot run
    fn a_folder_is_cloned_whole_with_its_links_kept_as_links() {
        let dir = TempDir::new("clone-folder");
        let from = dir.path().join("target");
        fs::create_dir_all(from.join("debug/deps")).unwrap();
        fs::write(from.join("debug/deps/lib.rlib"), "built\n").unwrap();
        symlink("../outside.txt", from.join("link")).unwrap();
        let to = dir.path().join("clone");

        clone_folder(&from, &to).unwrap();

        assert_eq!(
            fs::read_to_string(to.join("debug/deps/lib.rlib")).unwrap(),
            "built\n"
        );
        assert_eq!(
            fs::read_link(to.join("link")).unwrap(),
            Path::new("../outside.txt")
        );
        // A clone is a copy: writing it leaves the original as it was.
        fs::write(to.join("debug/deps/lib.rlib"), "rebuilt\n").unwrap();
        assert_eq!(
            fs::read_to_string(from.join("debug/deps/lib.rlib")).unwrap(),
            "built\n"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(miri, ignore)] // clonefile is an FFI call, which miri cannot run
    fn a_clone_onto_an_existing_path_fails_and_changes_nothing() {
        let dir = TempDir::new("clone-exists");
        let from = dir.path().join("target");
        fs::create_dir_all(&from).unwrap();
        let to = dir.path().join("clone");
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("kept"), "x").unwrap();

        let error = clone_folder(&from, &to).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        assert_eq!(fs::read_to_string(to.join("kept")).unwrap(), "x");
    }
}

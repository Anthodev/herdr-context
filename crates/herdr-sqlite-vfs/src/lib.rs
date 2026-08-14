use std::ffi::{CStr, c_char, c_int};
use std::io;
use std::sync::LazyLock;

use libsqlite3_sys as ffi;

pub const NAME: &str = "herdr-confined-unix";

static REGISTERED: LazyLock<Result<(), &'static str>> = LazyLock::new(register_inner);

pub fn register() -> io::Result<()> {
    match *REGISTERED {
        Ok(()) => Ok(()),
        Err(message) => Err(io::Error::other(message)),
    }
}

fn register_inner() -> Result<(), &'static str> {
    // SAFETY: SQLite owns the base VFS for the process lifetime. The copied function table keeps
    // its immutable callbacks and app data, while the leaked wrapper supplies a static name and
    // remains registered until process exit.
    unsafe {
        let base = ffi::sqlite3_vfs_find(c"unix".as_ptr());
        if base.is_null() {
            return Err("bundled Unix SQLite VFS is unavailable");
        }
        let mut wrapper = std::ptr::read(base);
        wrapper.pNext = std::ptr::null_mut();
        wrapper.zName = c"herdr-confined-unix".as_ptr();
        wrapper.xFullPathname = Some(descriptor_full_pathname);
        let wrapper = Box::into_raw(Box::new(wrapper));
        if ffi::sqlite3_vfs_register(wrapper, 0) != ffi::SQLITE_OK {
            drop(Box::from_raw(wrapper));
            return Err("confined SQLite VFS registration failed");
        }
    }
    Ok(())
}

unsafe extern "C" fn descriptor_full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    input: *const c_char,
    output_len: c_int,
    output: *mut c_char,
) -> c_int {
    if input.is_null() || output.is_null() || output_len <= 0 {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: SQLite provides a valid NUL-terminated input for xFullPathname.
    let input = unsafe { CStr::from_ptr(input) };
    if !is_descriptor_path(input.to_bytes()) {
        return ffi::SQLITE_CANTOPEN;
    }
    let input = input.to_bytes_with_nul();
    let Ok(output_len) = usize::try_from(output_len) else {
        return ffi::SQLITE_CANTOPEN;
    };
    if input.len() > output_len {
        return ffi::SQLITE_CANTOPEN;
    }
    // SAFETY: the length check proves the SQLite-owned output buffer is large enough, and
    // xFullPathname's input and output buffers do not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(input.as_ptr().cast(), output, input.len());
    }
    ffi::SQLITE_OK
}

fn is_descriptor_path(path: &[u8]) -> bool {
    let remainder = path
        .strip_prefix(b"/proc/self/fd/")
        .or_else(|| path.strip_prefix(b"/dev/fd/"));
    let Some((descriptor, name)) = remainder.and_then(split_once_slash) else {
        return false;
    };
    !descriptor.is_empty()
        && descriptor.iter().all(u8::is_ascii_digit)
        && !name.is_empty()
        && !name.contains(&b'/')
        && !matches!(name, b"." | b"..")
}

fn split_once_slash(value: &[u8]) -> Option<(&[u8], &[u8])> {
    let index = value.iter().position(|byte| *byte == b'/')?;
    Some((&value[..index], &value[index + 1..]))
}

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, c_char};

    use super::{descriptor_full_pathname, is_descriptor_path};

    #[test]
    fn accepts_only_direct_descriptor_children() {
        assert!(is_descriptor_path(b"/proc/self/fd/12/opencode.db"));
        assert!(is_descriptor_path(b"/dev/fd/3/opencode.db-wal"));
        assert!(!is_descriptor_path(b"/tmp/opencode.db"));
        assert!(!is_descriptor_path(b"/proc/self/fd/x/opencode.db"));
        assert!(!is_descriptor_path(b"/proc/self/fd/12/../opencode.db"));
    }

    #[test]
    fn full_pathname_preserves_the_descriptor_bridge() {
        let input = c"/proc/self/fd/12/opencode.db";
        let mut output = [0 as c_char; 128];
        // SAFETY: the test provides valid non-overlapping C buffers with the declared capacity.
        let result = unsafe {
            descriptor_full_pathname(
                std::ptr::null_mut(),
                input.as_ptr(),
                output.len().try_into().expect("output length"),
                output.as_mut_ptr(),
            )
        };
        assert_eq!(result, libsqlite3_sys::SQLITE_OK);
        // SAFETY: a successful callback writes the input including its NUL terminator.
        assert_eq!(unsafe { CStr::from_ptr(output.as_ptr()) }, input);
    }
}

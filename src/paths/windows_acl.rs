use crate::{Error, Result};
use std::{
    mem::size_of,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    ptr,
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::FILE_ALL_ACCESS,
    System::Threading::*,
};

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Allocation(*mut std::ffi::c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// An explicit, protected current-user DACL; reject filesystems that cannot enforce it.
pub(super) fn protect(path: &Path, directory: bool) -> Result<()> {
    // Do not follow a junction or symbolic link when tightening a data path.
    if std::fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0 {
        return Err(Error::Io(std::io::Error::other(
            "Private data path must not be a reparse point",
        )));
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if name[..name.len() - 1].contains(&0) {
        return Err(Error::Io(std::io::Error::other(
            "Invalid private data path",
        )));
    }
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        let token = Token(token);
        let mut size = 0;
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut size);
        if size == 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.0,
            TokenUser,
            data.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        let sid = (*(data.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        let entry = EXPLICIT_ACCESS_W {
            // File-specific rights apply to both this directory and its children.
            // Generic rights cause Windows to split effective/inherit-only ACEs.
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if directory {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                NO_INHERITANCE
            },
            Trustee: TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.cast(),
                ..Default::default()
            },
        };
        let mut acl = ptr::null_mut();
        let error = SetEntriesInAclW(1, &entry, ptr::null(), &mut acl);
        if error != ERROR_SUCCESS {
            return Err(Error::Io(std::io::Error::from_raw_os_error(error as i32)));
        }
        let _allocation = Allocation(acl.cast());
        let error = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        );
        if error != ERROR_SUCCESS {
            return Err(Error::Io(std::io::Error::from_raw_os_error(error as i32)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synthetic_private_directory_has_a_protected_dacl() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("한글 synthetic");
        crate::paths::ensure_private_dir(&path).unwrap();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let mut descriptor = ptr::null_mut();
            let mut dacl = ptr::null_mut();
            assert_eq!(
                GetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut dacl,
                    ptr::null_mut(),
                    &mut descriptor
                ),
                ERROR_SUCCESS
            );
            let _allocation = Allocation(descriptor);
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                0
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            assert!(!dacl.is_null());
            assert_eq!((*dacl).AceCount, 1);
        }
        std::fs::write(path.join("synthetic.txt"), "synthetic").unwrap();
    }
}

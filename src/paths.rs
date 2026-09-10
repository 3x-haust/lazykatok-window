use crate::{Error, Result};
use std::path::PathBuf;

#[cfg(windows)]
mod windows_acl;

pub fn default_data_dir() -> Result<PathBuf> {
    #[cfg(not(target_os = "macos"))]
    {
        Ok(dirs::data_local_dir()
            .ok_or(Error::HomeDirUnavailable)?
            .join("katok"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().ok_or(Error::HomeDirUnavailable)?;
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("katok"))
    }
}

pub fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(Error::Io)?;
    #[cfg(windows)]
    windows_acl::protect(path, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).map_err(Error::Io)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).map_err(Error::Io)?;
    }
    Ok(())
}

pub(crate) fn ensure_private_file(path: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    windows_acl::protect(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

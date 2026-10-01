//! The local identity, and whether the app serves its control socket.

mod control;
mod identity;

use std::path::PathBuf;

use directories::ProjectDirs;

pub use control::{COMMAND_LINE_ACCESS, CommandLineAccess};
pub use identity::{IDENTITY, IdentityError, LocalIdentity, StoredIdentity};

/// Return the platform-specific directory used for Ferry configuration.
pub fn default_config_dir() -> Option<PathBuf> {
    ProjectDirs::from("dev", "fanchao", "Ferry")
        .map(|directories| directories.config_dir().to_path_buf())
}

pub(crate) fn is_valid_device_id(device_id: &str) -> bool {
    (32..=38).contains(&device_id.len())
        && device_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub(crate) fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }

    Ok(())
}

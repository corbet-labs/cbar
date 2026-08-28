mod client;
pub mod commands;
pub mod responses;
mod server;

use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use tracing::warn;

pub use commands::*;
pub use responses::Response;

#[derive(Debug)]
pub struct Ipc {
    path: PathBuf,
}

impl Ipc {
    /// Creates a new IPC instance.
    /// This can be used as both a server and client.
    pub fn new() -> Result<Self> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| eyre!("XDG_RUNTIME_DIR is required for secure IPC"))?;

        let metadata = fs::metadata(&runtime_dir)
            .wrap_err_with(|| format!("Unable to inspect {}", runtime_dir.display()))?;
        if !metadata.is_dir() {
            return Err(eyre!(
                "XDG_RUNTIME_DIR is not a directory: {}",
                runtime_dir.display()
            ));
        }

        let ipc_socket_file = runtime_dir.join("ironbar-ipc.sock");

        if format!("{}", ipc_socket_file.display()).len() > 100 {
            warn!(
                "The IPC socket file's absolute path exceeds 100 bytes, the socket may fail to create."
            );
        }

        Ok(Self {
            path: ipc_socket_file,
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }
}

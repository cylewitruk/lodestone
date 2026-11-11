//! The daemon's listeners: its Unix socket, which a lock file keeps to one daemon, and TCP on
//! `127.0.0.1` when a port is configured.

#[cfg(unix)]
use std::fs::{self, File};
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::PathBuf;

use anyhow::Context;
#[cfg(unix)]
use anyhow::bail;
use tokio::net::TcpListener;

use crate::config::Config;

pub struct Listeners {
    #[cfg(unix)]
    pub unix: (tokio::net::UnixListener, Socket),
    pub tcp: Option<(TcpListener, SocketAddr)>,
}

impl Listeners {
    /// Binds every listener `config` asks for. Fails when another daemon owns the socket (or,
    /// without one, the port).
    pub async fn bind(config: &Config) -> anyhow::Result<Self> {
        #[cfg(unix)]
        let unix = Socket::bind(config.socket_path(), config.lock_path())?;
        let tcp = match config.tcp_port() {
            Some(port) => {
                let addr = SocketAddr::from(([127, 0, 0, 1], port));
                let l = TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("cannot listen on {addr}"))?;
                Some((l, addr))
            }
            None => None,
        };
        Ok(Self {
            #[cfg(unix)]
            unix,
            tcp,
        })
    }

    pub fn describe(&self) -> String {
        let mut at = Vec::new();
        #[cfg(unix)]
        at.push(format!("unix:{}", self.unix.1.path.display()));
        if let Some((_, addr)) = &self.tcp {
            at.push(format!("http://{addr}"));
        }
        at.join(" and ")
    }
}

/// The daemon's socket file, removed when dropped, and the lock that makes this daemon its owner.
#[cfg(unix)]
pub struct Socket {
    path: PathBuf,
    _lock: File,
}

#[cfg(unix)]
impl Socket {
    fn bind(path: PathBuf, lock_path: PathBuf) -> anyhow::Result<(tokio::net::UnixListener, Self)> {
        use std::fs::{OpenOptions, Permissions, TryLockError};
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};

        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!(
                "another lodestone daemon is listening on {}",
                path.display()
            ),
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", lock_path.display()));
            }
        }
        // Left behind by a daemon that did not exit cleanly.
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket()) {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("cannot listen on {}", path.display()))?;
        fs::set_permissions(&path, Permissions::from_mode(0o600))?;
        Ok((listener, Self { path, _lock: lock }))
    }
}

#[cfg(unix)]
impl Drop for Socket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

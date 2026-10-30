//! One Robin process. A second launch connects to the Unix socket and exits.
//! `robin --select <file>` sends that path to the running instance.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub struct Lock {
    listener: UnixListener,
    path: PathBuf,
}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let listener = UnixListener::bind(path).map_err(|e| Error::io(path, e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| Error::io(path, e))?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn poll_select(&self) -> Result<Option<PathBuf>> {
        match self.listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = String::new();
                stream
                    .read_to_string(&mut buf)
                    .map_err(|e| Error::io(&self.path, e))?;
                let line = buf.lines().next().unwrap_or("").trim();
                if let Some(path) = line.strip_prefix("select ") {
                    Ok(Some(PathBuf::from(path)))
                } else {
                    Ok(None)
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(Error::io(&self.path, err)),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn runtime_socket() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir());
    base.join("robin.sock")
}

pub fn send_select(socket: &Path, file: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket).map_err(|e| Error::io(socket, e))?;
    write!(stream, "select {}\n", file.display()).map_err(|e| Error::io(socket, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_process_selects_a_file_in_the_first() {
        let path = std::env::temp_dir().join(format!("robin-sock-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let lock = Lock::acquire(&path).unwrap();
        assert!(lock.poll_select().unwrap().is_none());
        send_select(&path, Path::new("/home/sky/Pictures/dune-sunset-0412.jpg")).unwrap();
        let selected = lock.poll_select().unwrap().unwrap();
        assert_eq!(
            selected,
            PathBuf::from("/home/sky/Pictures/dune-sunset-0412.jpg")
        );
        drop(lock);
        assert!(!path.exists());
    }
}

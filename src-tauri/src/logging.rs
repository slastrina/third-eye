//! Always-on logging (2026-09-03 review item 1). Every incident until now
//! was diagnosed by writing a probe, because the installed app logged to a
//! stderr nobody could see. The log now lands in
//! `~/Library/Logs/Third Eye/third-eye.log` (size-rotated, one `.1` kept)
//! for every launch; stderr stays on in debug builds; `THIRD_EYE_LOG_FILE`
//! still overrides the path; `RUST_LOG` overrides the level.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Rotate when the live file would pass this size; the previous file is
/// kept as `<name>.1` (so a diagnosis always has the last ~10 MB).
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// `~/Library/Logs/Third Eye` — Console.app's user-log location.
pub fn log_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        Path::new(&home)
            .join("Library")
            .join("Logs")
            .join("Third Eye"),
    )
}

pub fn default_log_path() -> Option<PathBuf> {
    log_dir().map(|d| d.join("third-eye.log"))
}

/// An append-only file that rotates itself at `max_bytes`.
pub struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
}

impl RotatingFile {
    pub fn open(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            file,
            written,
            max_bytes,
        })
    }

    /// The sibling the previous log rotates to.
    pub fn rotated_path(path: &Path) -> PathBuf {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".1");
        path.with_file_name(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        let _ = self.file.flush();
        let _ = std::fs::rename(&self.path, Self::rotated_path(&self.path));
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// The env_logger target: the rotating file, plus stderr when wanted. A
/// file write failure never fails the log call (stderr still gets it).
pub struct Tee {
    file: Option<RotatingFile>,
    stderr: bool,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(file) = &mut self.file {
            let _ = file.write_all(buf);
        }
        if self.stderr {
            let _ = io::stderr().write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
        if self.stderr {
            let _ = io::stderr().flush();
        }
        Ok(())
    }
}

/// Install the global logger. Returns the log file path in use (None when
/// no file could be opened — logging then goes to stderr only).
pub fn init() -> Option<PathBuf> {
    let path = std::env::var_os("THIRD_EYE_LOG_FILE")
        .map(PathBuf::from)
        .or_else(default_log_path);
    let file = path
        .as_ref()
        .and_then(|p| match RotatingFile::open(p.clone(), MAX_LOG_BYTES) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!(
                    "third-eye: cannot open log file {}: {e} — logging to stderr",
                    p.display()
                );
                None
            }
        });
    let opened = file.is_some();
    let tee = Tee {
        file,
        // Debug builds (and a file-less fallback) keep the terminal trail.
        stderr: cfg!(debug_assertions) || !opened,
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
        .target(env_logger::Target::Pipe(Box::new(tee)))
        .init();
    if opened {
        path
    } else {
        None
    }
}

/// The log file's path for Settings → Status.
#[tauri::command]
pub fn log_path() -> Option<String> {
    std::env::var_os("THIRD_EYE_LOG_FILE")
        .map(PathBuf::from)
        .or_else(default_log_path)
        .map(|p| p.display().to_string())
}

/// Reveal the log file in Finder.
#[tauri::command]
pub fn reveal_log() -> Result<(), String> {
    let path = log_path().ok_or_else(|| "no log path".to_string())?;
    std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(&path)
        .status()
        .map_err(|e| format!("could not run open: {e}"))
        .and_then(|s| {
            if s.success() {
                Ok(())
            } else {
                Err(format!("open exited {s}"))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_the_cap_keeping_one_previous_file() {
        let dir = std::env::temp_dir().join(format!("te-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("t.log");
        let mut f = RotatingFile::open(path.clone(), 100).unwrap();
        f.write_all(&[b'a'; 60]).unwrap();
        f.write_all(&[b'b'; 60]).unwrap(); // 120 > 100 → rotate BEFORE this write
        f.flush().unwrap();
        let rotated = RotatingFile::rotated_path(&path);
        assert_eq!(std::fs::read(&rotated).unwrap(), vec![b'a'; 60]);
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'b'; 60]);
        // A second rotation replaces the previous .1 (only one kept).
        f.write_all(&[b'c'; 60]).unwrap();
        f.flush().unwrap();
        assert_eq!(std::fs::read(&rotated).unwrap(), vec![b'b'; 60]);
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'c'; 60]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopening_counts_the_existing_size() {
        let dir = std::env::temp_dir().join(format!("te-log2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("t.log");
        RotatingFile::open(path.clone(), 100)
            .unwrap()
            .write_all(&[b'x'; 90])
            .unwrap();
        let mut again = RotatingFile::open(path.clone(), 100).unwrap();
        assert_eq!(again.written, 90);
        again.write_all(&[b'y'; 20]).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            vec![b'y'; 20],
            "rotated on reopen+overflow"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_path_is_under_library_logs() {
        let p = default_log_path().unwrap();
        assert!(p.ends_with("Library/Logs/Third Eye/third-eye.log"), "{p:?}");
    }
}

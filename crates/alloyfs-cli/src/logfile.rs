//! Log files for the commands that run for hours: `~/.alloyfs/logs/<name>.log`.
//!
//! A Windows service instance has NO console. `service run` spawns its child
//! with a zeroed `STARTUPINFOW` and `CREATE_NO_WINDOW`, so the child's stderr
//! goes to a handle that does not exist — and the supervisor's own
//! crash-loop diagnostics ("the instance exited immediately; backing off")
//! were written into the same void. Someone whose drive is missing had
//! nothing to read, anywhere. Long-running commands therefore tee their
//! tracing output to a file as well as stderr, and `alloyfs logs` reads it
//! back.
//!
//! Deliberately not `tracing-appender`: its value is the non-blocking
//! background writer, and a filesystem daemon that is already spending
//! milliseconds per wire round trip does not need a thread to defer a log
//! write to. This is a mutex, an append handle, and a size check.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Rotate once the live file passes this. One previous generation is kept
/// (`<name>.log.1`), so a name costs at most twice this on disk.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// `~/.alloyfs/logs`.
pub fn dir() -> PathBuf {
    crate::config::app_dir().join("logs")
}

pub fn path_for(name: &str) -> PathBuf {
    dir().join(format!("{name}.log"))
}

/// The name this process should log under, if any: the service id when the
/// supervisor passed one down, otherwise the caller's choice.
///
/// `service run <id>` sets this for the child it spawns, so a serviced
/// mount's output lands in the same file as the supervisor's — which is
/// what someone reading "why does my drive keep vanishing" needs to see
/// side by side.
pub fn name_override() -> Option<String> {
    std::env::var("ALLOYFS_LOG_NAME")
        .ok()
        .map(|s| sanitize(&s))
        .filter(|s| !s.is_empty())
}

/// Keep a log name to one path component, on both platforms.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

struct Inner {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
}

impl Inner {
    fn ensure(&mut self) -> io::Result<&mut std::fs::File> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            // Append mode, and the size is read back rather than assumed:
            // a restarted service continues its own file instead of
            // rotating it away on the first write.
            self.written = file.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("just opened"))
    }

    /// Hand the live file to `.log.1` and start a new one.
    ///
    /// Two processes can share one name (a service supervisor and its
    /// child), so both could decide to rotate at once. The loser's rename
    /// finds nothing to move and its next write re-creates the file — a
    /// doubled rotation, never a lost handle or a wedged writer.
    fn rotate(&mut self) {
        self.file = None;
        let previous = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::rename(&self.path, &previous);
        self.written = 0;
    }
}

/// A `MakeWriter` over one rotating file.
#[derive(Clone)]
pub struct FileLog(Arc<Mutex<Inner>>);

/// Open (lazily — nothing is created until the first line is written).
pub fn open(name: &str) -> FileLog {
    FileLog(Arc::new(Mutex::new(Inner {
        path: path_for(&sanitize(name)),
        file: None,
        written: 0,
    })))
}

pub struct Handle(Arc<Mutex<Inner>>);

impl Write for Handle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A poisoned lock must not silence logging — the data it guards is
        // a file handle and a byte count, and both are fine to reuse.
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.written >= MAX_BYTES {
            inner.rotate();
        }
        let written = {
            // Errors here are swallowed on purpose: a full disk or a
            // read-only home must not turn every log line into an error
            // the caller has to handle, and stderr still has the message.
            let Ok(file) = inner.ensure() else {
                return Ok(buf.len());
            };
            match file.write(buf) {
                Ok(n) => n,
                Err(_) => return Ok(buf.len()),
            }
        };
        inner.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = inner.file.as_mut() {
            let _ = file.flush();
        }
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileLog {
    type Writer = Handle;

    fn make_writer(&'a self) -> Self::Writer {
        Handle(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_one_generation_and_never_loses_the_writer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("t.log");
        let log = FileLog(Arc::new(Mutex::new(Inner {
            path: path.clone(),
            file: None,
            written: 0,
        })));
        let mut w = Handle(log.0.clone());

        // Enough to cross MAX_BYTES twice over.
        let line = vec![b'x'; 64 * 1024];
        for _ in 0..(MAX_BYTES / line.len() as u64 * 2 + 8) {
            w.write_all(&line).unwrap();
        }
        w.flush().unwrap();

        assert!(path.exists(), "the live file survives rotation");
        assert!(
            path.with_extension("log.1").exists(),
            "the previous generation is kept"
        );
        assert!(
            std::fs::metadata(&path).unwrap().len() <= MAX_BYTES + line.len() as u64,
            "the live file is bounded by the rotation size"
        );
        // And it is still writable afterwards — the failure that matters is
        // a rotation that leaves the writer holding a dead handle.
        w.write_all(b"after\n").unwrap();
        w.flush().unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("after\n"));
    }

    #[test]
    fn names_stay_one_path_component() {
        assert_eq!(sanitize("webdav"), "webdav");
        assert_eq!(sanitize("a b/c"), "a-b-c");
        // Dots are legal in a filename and are kept; what cannot survive is
        // anything that would make the name more than one component. A
        // leading run of them goes too, so no name starts a traversal.
        assert_eq!(sanitize("../../etc/passwd"), "-..-etc-passwd");
        for hostile in ["../../etc/passwd", "a\\b", "C:evil", "x/y"] {
            let safe = sanitize(hostile);
            assert!(
                !safe.contains(['/', '\\', ':']) && !safe.starts_with('.'),
                "{hostile:?} sanitized to {safe:?}"
            );
        }
    }
}

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_file_name(format!(
        ".aubridge-auth-{}-{sequence}.tmp",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

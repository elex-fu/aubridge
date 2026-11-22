use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_validated(path, bytes, || Ok(()))
}

/// Validate the destination after the complete replacement has been written and synced.
pub(crate) fn replace_validated(
    path: &Path,
    bytes: &[u8],
    validate: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
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
    let result = written
        .and_then(|()| validate())
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_commit_checks_complete_staged_bytes_and_preserves_external_edit() {
        let dir =
            std::env::temp_dir().join(format!("aubridge-validated-commit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let original = b"original-config";
        let proposed = b"complete-new-config";
        let external = b"external-config";
        fs::write(&path, original).unwrap();
        let result = replace_validated(&path, proposed, || {
            let staged = fs::read_dir(&dir)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .filter(|file| {
                    file.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".aubridge-auth-")
                })
                .collect::<Vec<_>>();
            assert_eq!(staged.len(), 1);
            assert_eq!(fs::read(&staged[0])?, proposed);
            assert_eq!(fs::read(&path)?, original);
            fs::write(&path, external)?;
            if fs::read(&path)? != original {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "destination changed",
                ));
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), external);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "failed validation must remove the staged file"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

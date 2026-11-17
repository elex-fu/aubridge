//! stash —— 写回前的完整备份(§7:"与 CLI 共享登录"开启时先备份,
//! mtime + hash;UI 一键还原即从此处取回)。engineering-constraints
//! B-7:`~/.aubridge/stash/`。
//!
//! 备份是开关开启时的一次性快照(不是每次写回都覆盖——写回本身是
//! 字段级合并,备份的职责是留"开启前的原样"供还原)。

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::chatgpt::FileGuard;

/// 备份元信息(可展示、可校验)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StashMeta {
    /// 备份时刻源文件的 mtime(unix 秒)。
    pub mtime: u64,
    /// 备份字节内容的 sha256(校验还原完整性)。
    pub sha256: String,
    /// 备份时刻(unix 秒)。
    pub backed_up_at: u64,
}

/// 备份文件名约定(固定名,一个凭据文件一份;重复开启 = 覆盖式重备,
/// 只保留开启前最近一次原样)。
const NAME: &str = "codex-auth.json";
const META_NAME: &str = "codex-auth.json.meta";

/// 把 `src` 完整备份进 `stash_dir`,返回元信息。src 缺失 = Err(调用方
/// 决定:开关开启失败,而不是静默"备了个空")。
pub fn backup(src: &Path, stash_dir: &Path) -> io::Result<StashMeta> {
    let bytes = std::fs::read(src)?;
    let mtime = std::fs::metadata(src)?
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::fs::create_dir_all(stash_dir)?;
    crate::private_file::replace(&stash_dir.join(NAME), &bytes)?;
    let meta = StashMeta {
        mtime,
        sha256: sha256_hex(&bytes),
        backed_up_at: unix_now(),
    };
    crate::private_file::replace(
        &stash_dir.join(META_NAME),
        &serde_json::to_vec_pretty(&meta)?,
    )?;
    Ok(meta)
}

/// 读备份元信息;无备份 = None。
pub fn meta(stash_dir: &Path) -> Option<StashMeta> {
    let text = std::fs::read_to_string(stash_dir.join(META_NAME)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 一键还原:stash 里的备份原子写回 `dst`(temp + rename)。无备份
/// = Err("无备份")。还原前校验备份 sha256，再与刷新写回互斥。
pub fn restore(stash_dir: &Path, dst: &Path) -> io::Result<()> {
    let bytes = verified_backup(stash_dir)?;
    let _guard = FileGuard::acquire(&dst.with_extension("json.lock"), Duration::from_secs(5))
        .map_err(io::Error::other)?;
    crate::private_file::replace(dst, &bytes)
}

pub fn verify(stash_dir: &Path) -> io::Result<()> {
    verified_backup(stash_dir).map(|_| ())
}

fn verified_backup(stash_dir: &Path) -> io::Result<Vec<u8>> {
    let bytes = std::fs::read(stash_dir.join(NAME))?;
    let Some(m) = meta(stash_dir) else {
        return Err(io::Error::other("无备份可还原"));
    };
    if sha256_hex(&bytes) != m.sha256 {
        return Err(io::Error::other("备份内容校验失败(sha256 不符)"));
    }
    Ok(bytes)
}

#[cfg(unix)]
pub fn protect_existing_backup(stash_dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let path = stash_dir.join(NAME);
    let entry = match std::fs::symlink_metadata(&path) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !entry.is_file() || entry.nlink() != 1 {
        return Err(io::Error::other("备份不是独立的普通文件"));
    }
    if entry.mode() & 0o077 == 0 {
        return Ok(());
    }
    let file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (entry.dev(), entry.ino()) {
        return Err(io::Error::other("备份文件已变化"));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// sha256 hex:stash 完整性校验 + §7 凭据指纹既定口径(设计文档
/// 561 行)。sha2(RustCrypto,纯 Rust 零传递依赖)——§5 清单外
/// 新依赖,变更记录 25 留评审记录。
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 测试/展示用:stash 目录里备份文件名(固定约定,不外泄路径拼接细节)。
#[cfg(test)]
pub(crate) fn backup_file(stash_dir: &Path) -> std::path::PathBuf {
    stash_dir.join(NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aubridge-stash-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn backup_and_restore_roundtrip() {
        let dir = tmp("rt");
        let src = dir.join("auth.json");
        std::fs::write(&src, r#"{"tokens":{"access_token":"at"}}"#).unwrap();
        let stash = dir.join("stash");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&stash).unwrap();
            std::fs::write(backup_file(&stash), "old backup").unwrap();
            std::fs::set_permissions(backup_file(&stash), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }

        let m = backup(&src, &stash).unwrap();
        assert_eq!(m.sha256.len(), 64);
        assert_eq!(meta(&stash), Some(m));

        // 写回改坏了原文件 → 还原回原字节。
        std::fs::write(&src, "corrupted").unwrap();
        restore(&stash, &src).unwrap();
        assert_eq!(
            std::fs::read_to_string(&src).unwrap(),
            r#"{"tokens":{"access_token":"at"}}"#
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [backup_file(&stash), stash.join(META_NAME), src] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[test]
    fn backup_missing_source_errors() {
        let dir = tmp("missing-src");
        assert!(backup(&dir.join("nope.json"), &dir.join("stash")).is_err());
    }

    #[test]
    fn restore_without_backup_errors() {
        let dir = tmp("no-backup");
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("auth.json");
        std::fs::write(&dst, "x").unwrap();
        assert!(restore(&dir, &dst).is_err());
        // 未被破坏。
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "x");
    }

    #[test]
    fn restore_respects_credential_file_lock() {
        let dir = tmp("restore-lock");
        let auth = dir.join("auth.json");
        let stash = dir.join("stash");
        std::fs::write(&auth, "original").unwrap();
        backup(&auth, &stash).unwrap();
        std::fs::write(&auth, "current").unwrap();

        let guard =
            FileGuard::acquire(&auth.with_extension("json.lock"), Duration::from_secs(1)).unwrap();
        let error = restore(&stash, &auth).expect_err("restore must respect the writeback lock");
        assert!(error.to_string().contains("file lock busy"));
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), "current");

        drop(guard);
        restore(&stash, &auth).unwrap();
        assert_eq!(std::fs::read_to_string(&auth).unwrap(), "original");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restore_detects_corrupted_backup() {
        let dir = tmp("corrupt");
        let src = dir.join("auth.json");
        std::fs::write(&src, "original").unwrap();
        let stash = dir.join("stash");
        backup(&src, &stash).unwrap();
        // 篡改备份本体(meta 仍是原 hash)→ 校验失败拒绝还原。
        std::fs::write(backup_file(&stash), "tampered").unwrap();
        assert!(restore(&stash, &src).is_err());
    }
}

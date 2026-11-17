//! credentials —— 凭据存取(设计文档 §7,M2 第一版:key 通道)。
//!
//! 纪律:service 命名 `aubridge/<provider-id>`(engineering-constraints
//! C);key 读写经 trait 抽象,keyring 为生产实现、内存实现可测;
//! OAuth 导入(M2):oauth 模块,登录读取只读。

use std::collections::BTreeMap;
use std::sync::Mutex;

/// key 存取抽象。Send + Sync:网关把 store 挂进跨线程共享状态
/// (变更记录 32-①)。
pub trait KeyStore: Send + Sync {
    fn set(&self, provider: &str, key: &str) -> Result<(), String>;
    fn get(&self, provider: &str) -> Option<String>;
    fn delete(&self, provider: &str) -> Result<(), String>;
}

/// 生产实现:OS keyring(service = `aubridge`,account = provider-id)。
pub struct OsKeyring;

fn entry(provider: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new("aubridge", provider).map_err(|e| e.to_string())
}

impl KeyStore for OsKeyring {
    fn set(&self, provider: &str, key: &str) -> Result<(), String> {
        entry(provider)?
            .set_password(key)
            .map_err(|e| e.to_string())
    }
    fn get(&self, provider: &str) -> Option<String> {
        entry(provider).ok()?.get_password().ok()
    }
    fn delete(&self, provider: &str) -> Result<(), String> {
        let entry = entry(provider)?;
        match entry.delete_credential() {
            Ok(()) => {
                // The macOS backend does not surface SecKeychainItemDelete's
                // return status. Confirm that the entry actually disappeared.
                #[cfg(target_os = "macos")]
                match entry.get_secret() {
                    Err(keyring::Error::NoEntry) => {}
                    Err(error) => return Err(error.to_string()),
                    Ok(_) => return Err("钥匙串条目仍然存在".into()),
                }
                Ok(())
            }
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// 测试/内存实现(与 OsKeyring 同签名;Mutex 满足跨线程共享)。
#[derive(Default)]
pub struct MemoryStore(pub Mutex<BTreeMap<String, String>>);

impl KeyStore for MemoryStore {
    fn set(&self, provider: &str, key: &str) -> Result<(), String> {
        self.0
            .lock()
            .expect("memory store lock")
            .insert(provider.into(), key.into());
        Ok(())
    }
    fn get(&self, provider: &str) -> Option<String> {
        self.0
            .lock()
            .expect("memory store lock")
            .get(provider)
            .cloned()
    }
    fn delete(&self, provider: &str) -> Result<(), String> {
        self.0.lock().expect("memory store lock").remove(provider);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryStore::default();
        store.set("zhipu", "zk-123").unwrap();
        assert_eq!(store.get("zhipu").as_deref(), Some("zk-123"));
        assert_eq!(store.get("openrouter"), None);
        store.delete("zhipu").unwrap();
        assert_eq!(store.get("zhipu"), None);
    }
}

pub mod ccswitch;
pub mod chatgpt;
pub mod oauth;
pub mod opencode;
pub mod planquota;
mod private_file;
pub mod refresh;
pub mod settings;
pub mod stash;
pub mod usage;

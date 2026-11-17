//! manifest —— 源声明模型与校验(设计文档 §7/§9)。
//!
//! 插件化 = 数据合同:内置 preset 与第三方插件走同一 schema、同一
//! 装载路径(`schema::Manifest` + `schema::validate`)。本 crate 永远
//! 零 IO、零重依赖(serde 之外),`state::ProviderState` 是 router 的
//! 纯函数输入快照。

pub mod schema;
pub mod state;

pub use schema::ProviderKind;
pub use schema::{AuthKind, Manifest, Permissions, ProviderDecl, QuotaDecl, QuotaSource};
pub use state::ProviderState;

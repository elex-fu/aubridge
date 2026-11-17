//! router —— 免费优先候选循环,产品的心脏(设计文档 §8)。
//!
//! 纯函数 crate:输入 `ProviderState` 快照 + 时刻,输出执行计划;
//! 无锁无 IO(crate-layout §2)。gateway 负责 IO 并回写状态转移结果。
//! 错误分类矩阵(§8.1)与 rest 四桶状态转移也在此,均为纯函数。

pub mod affinity;
pub mod breaker;
pub mod classify;
pub mod group;
pub mod guard;
pub mod plan;
pub mod rest;

pub use breaker::{record_failure, record_success};

pub use classify::{Bucket, FailureDecision, UpstreamSignal};
pub use group::{candidates_group, candidates_named_group, parse_members, GroupRule};
pub use plan::{candidates, candidates_auto};
pub use rest::{apply_failure, apply_sample_reset};

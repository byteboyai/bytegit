//! bytegit:ByteBoy 系产品共用的本地 Git 底层。
//!
//! 全部同步;公开 API 不暴露 `git2` 类型。设计见 dozer 仓库
//! `docs/superpowers/specs/2026-10-02-bytegit-design.md`。

mod change;
mod content;
mod error;
mod history;
mod id;
mod info;
mod repo;
mod stats;
mod status;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
#[cfg(feature = "watch")]
mod watch;
mod write;

pub use change::ChangeKind;
pub use content::{Content, ContentLimits, ContentPair};
pub use error::{GitError, GitErrorKind};
pub use history::{CommitSummary, FileChange, LogOptions, Patch, Signature};
pub use id::{BlobId, CommitId};
pub use info::{HeadInfo, Remote};
pub use repo::Repo;
pub use status::{FileState, StatusEntry, StatusOptions};
#[cfg(feature = "watch")]
pub use watch::{GitChange, IgnoreRules, WatchHandle, WatchOptions, watch};
pub use write::{CloneOptions, clone, git_available, init};

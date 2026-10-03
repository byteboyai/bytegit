//! bytegit:ByteBoy 系产品共用的本地 Git 底层。
//!
//! 全部同步;公开 API 不暴露 `git2` 类型。设计见 dozer 仓库
//! `docs/superpowers/specs/2026-10-02-bytegit-design.md`。

mod change;
mod error;
mod id;
mod info;
mod repo;
mod status;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

pub use change::ChangeKind;
pub use error::{GitError, GitErrorKind};
pub use id::{BlobId, CommitId};
pub use info::{HeadInfo, Remote};
pub use repo::Repo;
pub use status::{FileState, StatusEntry, StatusOptions};

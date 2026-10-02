use std::fmt;

/// 稳定的错误分类,调用方按它分支;展示用 [`GitError::message`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitErrorKind {
    /// 路径不在任何 git 仓库内。
    NotARepo,
    /// 引用(分支/提交/路径)不存在。
    RefNotFound,
    /// 仓库还没有任何提交。
    NoCommits,
    /// 需要 `git` 可执行文件但机器上没有(仅写操作的命令行实现会用到)。
    GitBinaryUnavailable,
    Io,
    /// 其余后端错误。
    Backend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError {
    kind: GitErrorKind,
    message: String,
}

impl GitError {
    pub fn new(kind: GitErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> GitErrorKind {
        self.kind
    }

    /// 适合直接展示给用户的文本。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GitError {}

impl From<git2::Error> for GitError {
    fn from(e: git2::Error) -> Self {
        let kind = match (e.code(), e.class()) {
            (git2::ErrorCode::NotFound, git2::ErrorClass::Repository) => GitErrorKind::NotARepo,
            (git2::ErrorCode::UnbornBranch, _) => GitErrorKind::NoCommits,
            (git2::ErrorCode::NotFound, _) => GitErrorKind::RefNotFound,
            _ => GitErrorKind::Backend,
        };
        Self::new(kind, e.message())
    }
}

impl From<std::io::Error> for GitError {
    fn from(e: std::io::Error) -> Self {
        Self::new(GitErrorKind::Io, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_maps_to_io_kind() {
        let e: GitError = std::io::Error::other("boom").into();
        assert_eq!(e.kind(), GitErrorKind::Io);
        assert_eq!(e.message(), "boom");
        assert_eq!(e.to_string(), "boom");
    }

    #[test]
    fn git2_not_found_in_repository_class_is_not_a_repo() {
        let e = git2::Error::new(
            git2::ErrorCode::NotFound,
            git2::ErrorClass::Repository,
            "could not find repository",
        );
        assert_eq!(GitError::from(e).kind(), GitErrorKind::NotARepo);
    }

    #[test]
    fn git2_not_found_elsewhere_is_ref_not_found() {
        let e = git2::Error::new(
            git2::ErrorCode::NotFound,
            git2::ErrorClass::Reference,
            "no such ref",
        );
        assert_eq!(GitError::from(e).kind(), GitErrorKind::RefNotFound);
    }

    #[test]
    fn git2_unborn_branch_is_no_commits() {
        let e = git2::Error::new(
            git2::ErrorCode::UnbornBranch,
            git2::ErrorClass::Reference,
            "unborn",
        );
        assert_eq!(GitError::from(e).kind(), GitErrorKind::NoCommits);
    }
}

use std::path::{Path, PathBuf};

use crate::{GitError, GitErrorKind};

/// 打开的仓库句柄。所有操作都是它的方法;`git2::Repository` 不对外暴露。
///
/// `Repo` 是 `Send` 不是 `Sync`:跨线程时各线程自己 `discover`。
pub struct Repo {
    inner: git2::Repository,
    root: PathBuf,
}

impl Repo {
    /// 从 `path` 向上查找仓库,所以项目位于仓库子目录时也能打开。
    ///
    /// 返回的 [`Repo::root`] 是 libgit2 解析后的真实路径(macOS 上 `/var/...`
    /// 会变成 `/private/var/...`),调用方与自己持有的项目路径比较前要先规范化。
    pub fn discover(path: &Path) -> Result<Self, GitError> {
        // libgit2 对不存在的路径报 NotFound,会被误归为 RefNotFound;这里先挡掉。
        if !path.exists() {
            return Err(GitError::new(
                GitErrorKind::Io,
                format!("路径不存在: {}", path.display()),
            ));
        }
        let inner = git2::Repository::discover(path)?;
        let dir = match inner.workdir() {
            Some(dir) => dir,
            // bare 仓库没有工作区,退而用 git 目录本身。
            None => inner.path(),
        };
        // libgit2 返回的目录带尾部分隔符,统一去掉。
        let root: PathBuf = dir.components().collect();
        Ok(Self { inner, root })
    }

    /// 工作区根目录(bare 仓库为 git 目录)。
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_bare(&self) -> bool {
        self.inner.is_bare()
    }

    /// 仓库里还没有任何引用(没有分支、标签等),即还没有提交。
    ///
    /// HEAD 指向尚未诞生的分支**不等于**没有提交:`git checkout --orphan` 之后
    /// HEAD 未诞生,但其他分支上有提交,此时返回 `false`。
    ///
    /// 不直接用 `git2::Repository::is_empty`:它对"空"的判断依赖用户全局配置里的
    /// `init.defaultBranch`,同一个空仓库在不同机器上结果可能不同。
    pub fn is_empty(&self) -> Result<bool, GitError> {
        match self.inner.head() {
            Ok(_) => Ok(false),
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
                Ok(self.inner.references()?.next().is_none())
            }
            Err(e) => Err(GitError::from(e)),
        }
    }

    pub(crate) fn raw(&self) -> &git2::Repository {
        &self.inner
    }
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo").field("root", &self.root).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    fn same(a: &Path, b: &Path) -> bool {
        a.canonicalize().unwrap() == b.canonicalize().unwrap()
    }

    #[test]
    fn repo_is_send() {
        fn is_send<T: Send>() {}
        is_send::<Repo>();
    }

    #[test]
    fn discover_finds_repo_at_root() {
        let t = TempRepo::new();
        let repo = Repo::discover(t.path()).unwrap();
        assert!(same(repo.root(), t.path()));
        assert!(!repo.is_bare());
    }

    #[test]
    fn discover_works_from_a_subdirectory() {
        let t = TempRepo::new();
        t.write_untracked("a/b/c.txt", "x");
        let repo = Repo::discover(&t.path().join("a/b")).unwrap();
        assert!(same(repo.root(), t.path()));
    }

    #[test]
    fn discover_outside_any_repo_is_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let err = Repo::discover(dir.path()).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NotARepo);
    }

    #[test]
    fn fresh_repo_is_empty_until_first_commit() {
        let t = TempRepo::new();
        assert!(Repo::discover(t.path()).unwrap().is_empty().unwrap());
        t.commit_file("a.txt", "hi", "first");
        assert!(!Repo::discover(t.path()).unwrap().is_empty().unwrap());
    }

    #[test]
    fn root_has_no_trailing_separator() {
        let t = TempRepo::new();
        let repo = Repo::discover(t.path()).unwrap();
        assert!(!repo.root().to_string_lossy().ends_with('/'));
        assert_eq!(repo.root().file_name(), t.path().file_name());
    }

    #[test]
    fn discover_nonexistent_path_is_io_not_ref_not_found() {
        let t = TempRepo::new();
        let err = Repo::discover(&t.path().join("nope/deeper")).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::Io);
    }

    #[test]
    fn discover_from_a_file_path_finds_the_repo() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "x", "one");
        let repo = Repo::discover(&t.path().join("a.txt")).unwrap();
        assert!(same(repo.root(), t.path()));
    }

    #[test]
    fn discover_from_inside_dot_git_returns_the_workdir() {
        let t = TempRepo::new();
        let repo = Repo::discover(&t.path().join(".git")).unwrap();
        assert!(same(repo.root(), t.path()));
        assert!(!repo.is_bare());
    }

    #[test]
    fn bare_repo_reports_bare_and_uses_git_dir_as_root() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(dir.path()).unwrap();
        let repo = Repo::discover(dir.path()).unwrap();
        assert!(repo.is_bare());
        assert!(same(repo.root(), dir.path()));
        assert!(repo.is_empty().unwrap());
    }

    #[test]
    fn empty_repo_with_default_branch_name_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        assert!(Repo::discover(dir.path()).unwrap().is_empty().unwrap());
    }

    #[test]
    fn detached_head_is_not_empty() {
        let t = TempRepo::new();
        let id = t.commit_file("a.txt", "x", "one");
        t.raw_repo().set_head_detached(id.oid()).unwrap();
        assert!(!Repo::discover(t.path()).unwrap().is_empty().unwrap());
    }

    #[test]
    fn unborn_head_with_commits_on_other_branches_is_not_empty() {
        // `git checkout --orphan` 之后的状态:HEAD 指向尚未诞生的分支,但仓库里有提交。
        let t = TempRepo::new();
        t.commit_file("a.txt", "x", "one");
        t.raw_repo().set_head("refs/heads/orphan").unwrap();
        assert!(!Repo::discover(t.path()).unwrap().is_empty().unwrap());
    }
}

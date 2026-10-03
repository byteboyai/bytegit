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

    /// 只认仓库根:`path` 必须正好是某个仓库的工作区根(bare 仓库为它的 git 目录)才打开,
    /// 比较前两边都先规范化(符号链接、尾部分隔符都不影响)。`path` 在仓库**子目录**里时返回
    /// `GitErrorKind::NotARepo`——与 [`Repo::discover`] 的向上查找相反。
    ///
    /// 这是 Dozer 早期按"项目路径就是仓库根"写的那批查询(分支、是否有改动、文件状态)的语义,
    /// 保留它为一个**显式**的构造函数,而不是悄悄改成向上查找(规格 §8 O9:是否统一是产品决策)。
    pub fn open_exact(path: &Path) -> Result<Self, GitError> {
        let repo = Self::discover(path)?;
        if same_dir(repo.root(), path) {
            Ok(repo)
        } else {
            Err(GitError::new(
                GitErrorKind::NotARepo,
                format!("不是 git 仓库的根目录: {}", path.display()),
            ))
        }
    }

    /// 从**目录** `dir` 向上查找它所属的、有工作区的仓库。`dir` 不是目录(含是个文件)返回
    /// `GitErrorKind::Io`,仓库是 bare 的返回 `GitErrorKind::NotARepo`——bare 仓库没有可
    /// 供"看状态"的工作区。([`Repo::discover`] 对文件路径与 bare 仓库都会成功。)
    pub fn discover_workdir(dir: &Path) -> Result<Self, GitError> {
        if !dir.is_dir() {
            return Err(GitError::new(
                GitErrorKind::Io,
                format!("不是目录: {}", dir.display()),
            ));
        }
        let repo = Self::discover(dir)?;
        if repo.is_bare() {
            return Err(GitError::new(
                GitErrorKind::NotARepo,
                format!("裸仓库没有工作区: {}", dir.display()),
            ));
        }
        Ok(repo)
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

/// 两个目录是同一个目录(规范化后相等);任一规范化失败按不同处理。
fn same_dir(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
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

    // ---- open_exact / discover_workdir ----

    #[test]
    fn open_exact_opens_the_repo_root() {
        let t = TempRepo::new();
        let repo = Repo::open_exact(t.path()).unwrap();
        assert!(same(repo.root(), t.path()));
    }

    #[test]
    fn open_exact_ignores_a_trailing_separator_and_dot_components() {
        let t = TempRepo::new();
        let with_dot = t.path().join(".");
        assert!(Repo::open_exact(&with_dot).is_ok());
        let mut with_slash = t.path().as_os_str().to_os_string();
        with_slash.push("/");
        assert!(Repo::open_exact(Path::new(&with_slash)).is_ok());
    }

    #[test]
    fn open_exact_rejects_a_subdirectory_of_a_repo() {
        let t = TempRepo::new();
        t.write_untracked("sub/a.txt", "x");
        let err = Repo::open_exact(&t.path().join("sub")).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NotARepo);
        assert!(
            err.message().starts_with("不是 git 仓库的根目录: "),
            "{}",
            err.message()
        );
        // 对比:`discover` 向上查找,同一个路径能打开。
        assert!(Repo::discover(&t.path().join("sub")).is_ok());
    }

    #[test]
    fn open_exact_outside_any_repo_and_for_a_missing_path_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Repo::open_exact(dir.path()).unwrap_err().kind(),
            GitErrorKind::NotARepo
        );
        assert_eq!(
            Repo::open_exact(&dir.path().join("nope"))
                .unwrap_err()
                .kind(),
            GitErrorKind::Io
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_exact_accepts_a_symlink_to_the_repo_root() {
        let t = TempRepo::new();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(t.path(), &link).unwrap();
        assert!(Repo::open_exact(&link).is_ok());
    }

    #[test]
    fn open_exact_accepts_a_bare_repo_directory() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(dir.path()).unwrap();
        let repo = Repo::open_exact(dir.path()).unwrap();
        assert!(repo.is_bare());
    }

    #[test]
    fn discover_workdir_finds_the_repo_from_a_directory_and_from_its_subdirectory() {
        let t = TempRepo::new();
        t.write_untracked("sub/a.txt", "x");
        assert!(same(
            Repo::discover_workdir(t.path()).unwrap().root(),
            t.path()
        ));
        assert!(same(
            Repo::discover_workdir(&t.path().join("sub"))
                .unwrap()
                .root(),
            t.path()
        ));
    }

    #[test]
    fn discover_workdir_rejects_a_file_path() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "x", "one");
        let err = Repo::discover_workdir(&t.path().join("a.txt")).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::Io);
    }

    #[test]
    fn discover_workdir_rejects_a_bare_repo() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(dir.path()).unwrap();
        let err = Repo::discover_workdir(dir.path()).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NotARepo);
    }

    #[test]
    fn discover_workdir_outside_any_repo_and_for_a_missing_dir_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Repo::discover_workdir(dir.path()).unwrap_err().kind(),
            GitErrorKind::NotARepo
        );
        assert_eq!(
            Repo::discover_workdir(&dir.path().join("nope"))
                .unwrap_err()
                .kind(),
            GitErrorKind::Io
        );
    }
}

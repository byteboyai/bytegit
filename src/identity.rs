//! 工作区身份:一个仓库的"工作区根 + 私有 gitdir + 共享 common dir"。
//!
//! 监听、订阅与查询调度都以 [`WorkspaceKey`] 为键。它不是仓库根的同义词:
//! 同一个仓库的不同 linked worktree 共享 `common_dir`(refs/objects),但各有自己的
//! `git_dir`(HEAD/index),因此是**不同**的工作区;同一个工作区的不同子目录则得到同一个键。
//!
//! 路径一律由 libgit2 已解析的目录规范化(canonicalize)后保存,调用方拿项目输入路径
//! 做比较前不需要自己处理符号链接或尾部分隔符。公开 API 不暴露 `git2` 类型。

use std::path::{Path, PathBuf};

use crate::{GitError, GitErrorKind, Repo};

/// 一个仓库解析出的身份。bare 仓库没有工作区根。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RepoIdentity {
    workdir_root: Option<PathBuf>,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl RepoIdentity {
    /// 工作区根;bare 仓库为 `None`。
    pub fn workdir_root(&self) -> Option<&Path> {
        self.workdir_root.as_deref()
    }

    /// 当前工作区私有的 git 目录(linked worktree 为 `<common>/worktrees/<名>`)。
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// 共享 refs/objects 的 common 目录。普通仓库与裸仓库通常等于 [`Self::git_dir`]。
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    pub fn is_bare(&self) -> bool {
        self.workdir_root.is_none()
    }

    /// 工作区订阅与查询调度使用的键。bare 仓库没有工作区,返回
    /// [`GitErrorKind::NotARepo`]——历史等只读 API 的 bare 兼容性不受影响。
    pub fn workspace_key(&self) -> Result<WorkspaceKey, GitError> {
        let Some(root) = &self.workdir_root else {
            return Err(GitError::new(
                GitErrorKind::NotARepo,
                format!("裸仓库没有工作区订阅身份: {}", self.git_dir.display()),
            ));
        };
        Ok(WorkspaceKey {
            workdir_root: root.clone(),
            git_dir: self.git_dir.clone(),
        })
    }
}

/// 可哈希的工作区键:同一个工作区的任何子目录得到同一个值;同 `common_dir` 下的
/// 不同 linked worktree 得到不同值(它们的 `git_dir` 不同)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub struct WorkspaceKey {
    workdir_root: PathBuf,
    git_dir: PathBuf,
}

impl WorkspaceKey {
    pub fn workdir_root(&self) -> &Path {
        &self.workdir_root
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

impl RepoIdentity {
    /// 测试用:直接构造一个身份,避免分类/聚合测试依赖真实仓库。
    #[cfg(all(test, feature = "watch"))]
    pub(crate) fn for_test(
        workdir_root: Option<PathBuf>,
        git_dir: PathBuf,
        common_dir: PathBuf,
    ) -> Self {
        Self {
            workdir_root,
            git_dir,
            common_dir,
        }
    }
}

impl Repo {
    /// 解析当前打开仓库的工作区身份。路径来自 libgit2 已解析的 gitdir/common_dir,
    /// 不靠手工读取项目下的 `.git` 文件,所以 linked worktree / 子模块的引用文件形式
    /// 也能得到正确结果。
    pub fn identity(&self) -> Result<RepoIdentity, GitError> {
        let raw = self.raw();
        let workdir_root = raw.workdir().map(normalize);
        let git_dir = normalize(raw.path());
        let common_dir = normalize(raw.commondir());
        Ok(RepoIdentity {
            workdir_root,
            git_dir,
            common_dir,
        })
    }
}

/// canonicalize 后去掉尾部分隔符;路径不存在等失败时退回按分量拼接的纯词法结果
/// (不跟随符号链接),保证身份构造不会因为短暂的删除而 panic。
fn normalize(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(p) => p,
        Err(_) => path.components().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;
    use std::process::Command;

    fn command_ok(mut cmd: Command, what: &str) -> std::process::Output {
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("{what} 无法执行: {e}"));
        assert!(
            out.status.success(),
            "{what} 失败: status={:?}\nstdout={}\nstderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// 用真实 `git worktree add` 建一个 linked worktree,返回它的目录。调用方必须
    /// 持有配套的 [`tempfile::TempDir`],否则目录会在测试中途被删除。
    fn add_worktree(main: &TempRepo, name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().canonicalize().unwrap().join(name);
        let mut cmd = Command::new("git");
        cmd.args(["worktree", "add", "--detach"])
            .arg(&wt)
            .current_dir(main.path());
        command_ok(cmd, "git worktree add");
        (dir, wt)
    }

    #[test]
    fn root_and_a_subdirectory_share_one_workspace_key() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "x", "one");
        t.write_untracked("sub/dir/b.txt", "y");

        let from_root = Repo::discover(t.path()).unwrap().identity().unwrap();
        let from_sub = Repo::discover_workdir(&t.path().join("sub/dir"))
            .unwrap()
            .identity()
            .unwrap();

        assert_eq!(from_root, from_sub);
        assert_eq!(
            from_root.workspace_key().unwrap(),
            from_sub.workspace_key().unwrap()
        );
        assert_eq!(
            from_root.workdir_root().unwrap(),
            from_sub.workdir_root().unwrap()
        );
    }

    #[test]
    fn a_normal_repo_has_git_dir_equal_to_common_dir() {
        let t = TempRepo::new();
        let id = t.open().identity().unwrap();
        assert_eq!(id.git_dir(), id.common_dir());
        assert!(!id.is_bare());
        assert_eq!(id.git_dir(), t.path().canonicalize().unwrap().join(".git"));
    }

    #[test]
    fn linked_worktrees_share_common_dir_but_have_distinct_keys() {
        let main = TempRepo::new();
        main.commit_file("a.txt", "1\n", "init");
        let main_id = main.open().identity().unwrap();

        let (_wt_dir, wt) = add_worktree(&main, "wt");
        let wt_repo = Repo::discover_workdir(&wt).unwrap();
        let wt_id = wt_repo.identity().unwrap();

        assert_eq!(wt_id.common_dir(), main_id.common_dir());
        assert_ne!(wt_id.git_dir(), main_id.git_dir());
        assert_ne!(
            wt_id.workspace_key().unwrap(),
            main_id.workspace_key().unwrap()
        );
        // worktree 身份必须指向 worktree 自己的目录,而不是主工作区。
        assert_eq!(
            wt_id.workdir_root().unwrap(),
            wt.canonicalize().unwrap().as_path()
        );
        assert!(wt_id.git_dir().starts_with(main_id.common_dir()));
    }

    #[test]
    fn a_submodule_has_its_own_git_dir_and_key() {
        let super_repo = TempRepo::new();
        super_repo.commit_file("a.txt", "1\n", "init");
        let sub = TempRepo::new();
        sub.commit_file("b.txt", "1\n", "sub init");

        let mut cmd = Command::new("git");
        cmd.args(["-c", "protocol.file.allow=always", "submodule", "add"])
            .arg(sub.path())
            .arg("sub")
            .current_dir(super_repo.path());
        command_ok(cmd, "git submodule add");

        let super_id = super_repo.open().identity().unwrap();
        let sub_id = Repo::discover_workdir(&super_repo.path().join("sub"))
            .unwrap()
            .identity()
            .unwrap();

        assert_eq!(
            sub_id.workdir_root().unwrap(),
            super_repo
                .path()
                .canonicalize()
                .unwrap()
                .join("sub")
                .as_path()
        );
        assert_eq!(sub_id.git_dir(), sub_id.common_dir());
        assert!(
            sub_id.git_dir().ends_with(Path::new(".git/modules/sub")),
            "{:?}",
            sub_id.git_dir()
        );
        assert_ne!(
            sub_id.workspace_key().unwrap(),
            super_id.workspace_key().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_alias_resolves_to_the_same_identity() {
        let t = TempRepo::new();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(t.path(), &link).unwrap();

        let direct = Repo::discover(t.path()).unwrap().identity().unwrap();
        let via_link = Repo::discover(&link).unwrap().identity().unwrap();

        assert_eq!(direct, via_link);
        assert_eq!(
            via_link.workdir_root().unwrap(),
            t.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn bare_identity_has_no_workdir_and_cannot_be_subscribed() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(dir.path()).unwrap();
        let id = Repo::discover(dir.path()).unwrap().identity().unwrap();

        assert!(id.is_bare());
        assert!(id.workdir_root().is_none());
        assert_eq!(id.git_dir(), id.common_dir());
        let err = id.workspace_key().unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NotARepo);
    }

    #[test]
    fn identity_paths_are_normalized_without_a_trailing_separator() {
        let t = TempRepo::new();
        let id = t.open().identity().unwrap();
        for p in [id.workdir_root().unwrap(), id.git_dir(), id.common_dir()] {
            assert!(
                !p.to_string_lossy().ends_with('/'),
                "路径不应带尾部分隔符: {}",
                p.display()
            );
        }
    }
}

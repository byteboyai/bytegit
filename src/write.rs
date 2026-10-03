//! 写操作:`init`、`clone`、`Repo::checkout_branch`,以及 `git_available`。
//!
//! # 实现选择(评估结论,见规格 §4.6)
//!
//! `init` 用 `git2`。初始分支与命令行一致地遵循 `init.defaultBranch`(libgit2 自己会读全局
//! 配置,没配置为 `master`);与命令行的差异只有示例 hook 文件等无关紧要的内容。
//!
//! `clone`、`checkout_branch` **仍调用 `git` 可执行文件**,调用方看不到命令行细节:
//!
//! - libgit2 在本构建里没有 https/ssh 传输(`git2` 不开 `https`/`ssh` feature,避免引入
//!   OpenSSL/libssh2),网络 URL 直接报 `unsupported URL protocol`;即使开了,也读不到
//!   `~/.ssh/config`、`core.sshCommand`、代理与系统凭据助手的全部配置。
//! - libgit2 的 checkout 在"能不能切、切完什么状态"上与命令行一致,但**不执行
//!   `post-checkout` hook、不执行外部 smudge/clean 过滤器**(Git LFS 会留下指针文件而不是
//!   真实内容),冲突时也只说"N 个冲突"而不列文件、不给处理建议。
//!
//! `checkout_branch_runs_the_post_checkout_hook`、`checkout_branch_applies_external_filters`
//! 两条测试把这个理由钉住:谁想换成 `git2`,先让它们过。

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::Command;

use crate::{GitError, GitErrorKind, Repo};

/// [`clone`] 的选项。目前没有可选项(鉴权完全委托系统已配置的 SSH agent/凭据助手,
/// 不接收 token),留着这个类型是为了以后加分支、深度等选项不破坏调用方。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CloneOptions {}

/// 机器上有没有可用的 `git` 可执行文件(只看能不能跑起来,不解析版本)。
/// `clone`/`checkout_branch` 依赖它;URL 签出表单在提交前用它给出安装提示。
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// 在已存在的目录 `path` 里新建仓库。仓库已存在时无害(不改 HEAD 与提交)。
/// 初始分支遵循 `init.defaultBranch`(没配置为 `master`)。`path` 不存在时报错且**不创建**
/// 目录(迁移前命令行在不存在的工作目录里根本跑不起来;libgit2 自己会 `mkdir -p`)。
pub fn init(path: &Path) -> Result<Repo, GitError> {
    if !path.is_dir() {
        return Err(GitError::new(
            GitErrorKind::Io,
            format!("目录不存在: {}", path.display()),
        ));
    }
    git2::Repository::init(path)?;
    Repo::discover(path)
}

/// `git clone -- <url> <dest>`。鉴权完全委托系统已配置的 SSH agent/凭据助手。
/// `dest` 应当还不存在(调用方先校验)。失败时 `message()` 是 git 的 stderr 原文;
/// 机器上没有 `git` 返回 `GitErrorKind::GitBinaryUnavailable`。
///
/// `url` 可能来自用户粘贴或第三方 API,两者都不可信:用 `--` 结束选项解析,防止以 `-`
/// 开头的伪造 URL 被 git 当成命令行选项(同 CVE-2017-1000117 那一类问题)。
pub fn clone(url: &str, dest: &Path, _opts: CloneOptions) -> Result<Repo, GitError> {
    run_git(
        "git",
        None,
        &[
            OsStr::new("clone"),
            OsStr::new("--"),
            OsStr::new(url),
            dest.as_os_str(),
        ],
    )?;
    Repo::discover(dest)
}

impl Repo {
    /// 切换到本地分支 `name`(`git checkout <name>`)。工作区有会被覆盖的改动、或有未跟踪
    /// 文件挡路时失败,工作区保持原样;不冲突的改动(含已暂存的)会带到新分支。
    /// 失败时 `message()` 是 git 的 stderr 原文(含受影响的文件与处理建议),可直接展示。
    ///
    /// 从仓库工作区根运行,所以 `Repo` 是从子目录打开的也切整个仓库。
    pub fn checkout_branch(&self, name: &str) -> Result<(), GitError> {
        run_git(
            "git",
            Some(self.root()),
            &[OsStr::new("checkout"), OsStr::new(name)],
        )
    }
}

/// 跑 `program args...`,成功返回 `Ok`;失败带 stderr 原文。`program` 可替换,只为了测试
/// "找不到 git" 的分支。
fn run_git(program: &str, cwd: Option<&Path>, args: &[&OsStr]) -> Result<(), GitError> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().map_err(spawn_error)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(GitError::new(
            GitErrorKind::Backend,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

fn spawn_error(e: io::Error) -> GitError {
    let kind = if e.kind() == io::ErrorKind::NotFound {
        GitErrorKind::GitBinaryUnavailable
    } else {
        GitErrorKind::Io
    };
    GitError::new(kind, format!("无法运行 git: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    fn read(t: &TempRepo, rel: &str) -> String {
        std::fs::read_to_string(t.path().join(rel)).unwrap()
    }

    /// main:a.txt("a1")、b.txt("b1");feature:a.txt("a2")、多一个 c.txt。当前在 main。
    fn two_branches() -> TempRepo {
        let t = TempRepo::new();
        t.commit_file("a.txt", "a1\n", "base");
        t.commit_file("b.txt", "b1\n", "add b");
        t.branch("feature").checkout("feature");
        t.commit_file("a.txt", "a2\n", "feature changes a");
        t.commit_file("c.txt", "c1\n", "feature adds c");
        t.checkout("main");
        t
    }

    fn head_branch(t: &TempRepo) -> Option<String> {
        t.open().head().unwrap().branch
    }

    // ---- init ----

    #[test]
    fn init_creates_a_repository_with_an_unborn_head() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init(dir.path()).unwrap();
        assert!(dir.path().join(".git").is_dir());
        assert!(repo.is_empty().unwrap());
        assert!(!repo.head().unwrap().has_commits());
    }

    #[test]
    fn init_names_the_initial_branch_after_the_users_default() {
        // 不假设本机配置:预期值用同一份用户配置算出来(没配置为 master)。
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let expected = git2::Config::open_default()
            .and_then(|cfg| cfg.get_string("init.defaultbranch"))
            .ok()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "master".to_string());
        let head = std::fs::read_to_string(dir.path().join(".git/HEAD")).unwrap();
        assert_eq!(head.trim(), format!("ref: refs/heads/{expected}"));
    }

    #[test]
    fn init_on_an_existing_repo_is_harmless() {
        let t = TempRepo::new();
        let id = t.commit_file("a.txt", "x\n", "one");
        let repo = init(t.path()).unwrap();
        let head = repo.head().unwrap();
        assert_eq!(head.branch.as_deref(), Some("main"));
        assert_eq!(head.commit, Some(id));
    }

    #[test]
    fn init_inside_a_repo_subdirectory_creates_a_nested_repo() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "x\n", "one");
        let sub = t.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let nested = init(&sub).unwrap();
        assert!(
            sub.join(".git").is_dir(),
            "在子目录里 init 是新建嵌套仓库,不是复用上层"
        );
        assert_eq!(nested.root(), sub.canonicalize().unwrap());
    }

    #[test]
    fn init_in_a_missing_directory_is_an_error_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let err = init(&missing).expect_err("目录不存在应报错");
        assert_eq!(err.kind(), GitErrorKind::Io);
        assert!(!missing.exists());
    }

    #[test]
    fn init_on_a_file_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("afile");
        std::fs::write(&file, "x").unwrap();
        assert!(init(&file).is_err());
    }

    // ---- git_available / clone ----

    #[test]
    fn git_available_detects_system_git() {
        // 这几组测试都要求机器上有真 git(`clone`/`checkout_branch` 本来就是命令行实现)。
        assert!(git_available());
    }

    #[test]
    fn clone_copies_a_local_source_repo_and_returns_it() {
        let src = TempRepo::new();
        src.commit_file("a.txt", "one\n", "c1");
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("cloned");
        let repo = clone(
            &src.path().to_string_lossy(),
            &dest,
            CloneOptions::default(),
        )
        .unwrap();
        assert!(dest.join(".git").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(repo.head().unwrap().branch.as_deref(), Some("main"));
    }

    #[test]
    fn clone_fails_on_a_missing_source_with_gits_stderr() {
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("cloned");
        let missing = dest_parent.path().join("does-not-exist");
        let err = clone(&missing.to_string_lossy(), &dest, CloneOptions::default())
            .expect_err("源不存在应失败");
        assert_eq!(err.kind(), GitErrorKind::Backend);
        assert!(!err.message().is_empty());
    }

    #[test]
    fn clone_treats_a_dash_prefixed_url_as_a_path_not_an_option() {
        // 回归测试:确保 `--` 结束选项解析这道防线还在——若被误删,git 会把这个"URL"解析成
        // `--upload-pack` 选项而不是报"找不到仓库"(同 CVE-2017-1000117 那一类问题)。
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("cloned");
        let marker = dest_parent.path().join("pwned");
        let url = format!("--upload-pack=touch {}", marker.display());
        let err =
            clone(&url, &dest, CloneOptions::default()).expect_err("伪造的选项应被当成路径而失败");
        assert!(
            !marker.exists(),
            "伪造的 --upload-pack 参数不应该被当成选项执行"
        );
        assert!(!dest.exists());
        // 有 `--` 时 git 把它当成一个(不存在的)仓库路径并在报错里原样点名;
        // 没有 `--` 时它会被当成 `--upload-pack` 选项,报错里只会提到 `dest`。
        assert!(err.message().contains("--upload-pack"), "{}", err.message());
    }

    #[test]
    fn a_missing_git_binary_is_reported_as_binary_unavailable() {
        let err = run_git(
            "definitely-not-a-real-git-binary-for-bytegit-tests",
            None,
            &[OsStr::new("--version")],
        )
        .unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::GitBinaryUnavailable);
        assert!(
            err.message().starts_with("无法运行 git: "),
            "{}",
            err.message()
        );
    }

    // ---- checkout_branch ----

    #[test]
    fn checkout_branch_switches_head_and_the_worktree() {
        let t = two_branches();
        t.open().checkout_branch("feature").unwrap();
        assert_eq!(head_branch(&t).as_deref(), Some("feature"));
        assert_eq!(read(&t, "a.txt"), "a2\n");
        assert_eq!(read(&t, "c.txt"), "c1\n");
    }

    #[test]
    fn checking_out_the_current_branch_is_fine() {
        let t = two_branches();
        t.open().checkout_branch("main").unwrap();
        assert_eq!(head_branch(&t).as_deref(), Some("main"));
    }

    #[test]
    fn an_unstaged_edit_to_a_file_the_branches_agree_on_is_carried_over() {
        let t = two_branches();
        t.write_untracked("b.txt", "b-local\n");
        t.open().checkout_branch("feature").unwrap();
        assert_eq!(read(&t, "b.txt"), "b-local\n");
    }

    #[test]
    fn a_staged_new_file_is_carried_over() {
        let t = two_branches();
        t.write_untracked("d.txt", "d\n").stage("d.txt");
        t.open().checkout_branch("feature").unwrap();
        assert_eq!(read(&t, "d.txt"), "d\n");
    }

    #[test]
    fn a_conflicting_edit_blocks_the_switch_and_leaves_the_worktree_alone() {
        let t = two_branches();
        t.write_untracked("a.txt", "a-local\n");
        let err = t.open().checkout_branch("feature").unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::Backend);
        // git 的 stderr 原文会点名受影响的文件(UI 把它原样展示给用户)。
        assert!(err.message().contains("a.txt"), "{}", err.message());
        assert_eq!(head_branch(&t).as_deref(), Some("main"));
        assert_eq!(read(&t, "a.txt"), "a-local\n");
    }

    #[test]
    fn an_untracked_file_in_the_way_blocks_the_switch() {
        let t = two_branches();
        t.write_untracked("c.txt", "mine\n");
        let err = t.open().checkout_branch("feature").unwrap_err();
        assert!(err.message().contains("c.txt"), "{}", err.message());
        assert_eq!(head_branch(&t).as_deref(), Some("main"));
        assert_eq!(read(&t, "c.txt"), "mine\n");
    }

    #[test]
    fn checking_out_a_missing_branch_is_an_error() {
        let t = two_branches();
        let err = t.open().checkout_branch("nope").unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::Backend);
        assert!(!err.message().is_empty());
        assert_eq!(head_branch(&t).as_deref(), Some("main"));
    }

    #[test]
    fn a_repo_opened_from_a_subdirectory_still_switches_the_whole_repo() {
        let t = two_branches();
        let sub = t.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let repo = Repo::discover(&sub).unwrap();
        repo.checkout_branch("feature").unwrap();
        assert_eq!(head_branch(&t).as_deref(), Some("feature"));
        assert_eq!(read(&t, "a.txt"), "a2\n");
    }

    /// 这条与下一条是"为什么 `checkout_branch` 不能换成 `git2`"的理由,见模块文档。
    #[cfg(unix)]
    #[test]
    fn checkout_branch_runs_the_post_checkout_hook() {
        use std::os::unix::fs::PermissionsExt;
        let t = two_branches();
        let hook = t.path().join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        t.open().checkout_branch("feature").unwrap();
        assert!(t.path().join("hook-ran").exists());
    }

    #[cfg(unix)]
    #[test]
    fn checkout_branch_applies_external_filters() {
        // Git LFS 就是靠外部 smudge/clean 过滤器工作的:libgit2 不执行它们,
        // 换成 `git2` 会让 LFS 文件以指针文本落在工作区。
        let t = two_branches();
        std::fs::write(t.path().join(".git/info/attributes"), "a.txt filter=up\n").unwrap();
        let mut cfg = t.raw_repo().config().unwrap();
        cfg.set_str("filter.up.smudge", "tr a-z A-Z").unwrap();
        cfg.set_str("filter.up.clean", "cat").unwrap();
        t.open().checkout_branch("feature").unwrap();
        assert_eq!(read(&t, "a.txt"), "A2\n");
    }
}

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
    ///
    /// `name` 以 `-` 开头会被 git 当成选项,直接拒绝;末尾的 `--` 让 git 只把 `name` 当引用,
    /// 不会在分支名与同名文件并存时报歧义。
    pub fn checkout_branch(&self, name: &str) -> Result<(), GitError> {
        if name.starts_with('-') {
            return Err(GitError::new(
                GitErrorKind::Backend,
                format!("无效的分支名: {name}"),
            ));
        }
        run_git(
            "git",
            Some(self.root()),
            &[OsStr::new("checkout"), OsStr::new(name), OsStr::new("--")],
        )
    }
}

/// 可取消写操作的结果。`Cancelled`/`TimedOut` 是可解释结果,不是后端错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationOutcome {
    Completed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationReport {
    pub outcome: OperationOutcome,
    /// 最后一段 stderr(默认最多 256KiB)。
    pub stderr: String,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationProgress {
    StdoutChunk(usize),
    StderrChunk(usize),
}

#[derive(Clone)]
pub struct OperationOptions {
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// `None` = 不设总超时(默认)。
    pub timeout: Option<std::time::Duration>,
    /// stderr 保留的最后字节数;`0` 使用默认 256KiB。
    pub stderr_tail_bytes: usize,
}

impl Default for OperationOptions {
    fn default() -> Self {
        Self {
            cancel: None,
            timeout: None,
            stderr_tail_bytes: 256 * 1024,
        }
    }
}

impl std::fmt::Debug for OperationOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationOptions")
            .field("cancel", &self.cancel.is_some())
            .field("timeout", &self.timeout)
            .field("stderr_tail_bytes", &self.stderr_tail_bytes)
            .finish()
    }
}

/// 可取消操作的完成值。成功时同时返回 stderr/截断信息(可能有警告)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation<T> {
    Completed { value: T, report: OperationReport },
    Cancelled(OperationReport),
    TimedOut(OperationReport),
}

impl<T> Operation<T> {
    pub fn outcome(&self) -> OperationOutcome {
        match self {
            Self::Completed { .. } => OperationOutcome::Completed,
            Self::Cancelled(_) => OperationOutcome::Cancelled,
            Self::TimedOut(_) => OperationOutcome::TimedOut,
        }
    }
}

/// `git clone` 的可取消版本。`progress` 在等待线程上被调用,事件有界丢弃,
/// 不会阻塞 stdout/stderr 排空。
pub fn clone_with_options(
    url: &str,
    dest: &Path,
    options: &OperationOptions,
    progress: Option<&mut dyn FnMut(OperationProgress)>,
) -> Result<Operation<Repo>, GitError> {
    let report = run_cancellable(
        "git",
        None,
        &[
            OsStr::new("clone"),
            OsStr::new("--"),
            OsStr::new(url),
            dest.as_os_str(),
        ],
        options,
        progress,
    )?;
    match report.outcome {
        OperationOutcome::Completed => {
            let repo = Repo::discover(dest)?;
            Ok(Operation::Completed {
                value: repo,
                report,
            })
        }
        OperationOutcome::Cancelled => Ok(Operation::Cancelled(report)),
        OperationOutcome::TimedOut => Ok(Operation::TimedOut(report)),
    }
}

/// 可取消 `git checkout <name>`;旧 [`Repo::checkout_branch`] 的兼容包装会忽略取消。
pub fn checkout_branch_with_options(
    repo: &Repo,
    name: &str,
    options: &OperationOptions,
    progress: Option<&mut dyn FnMut(OperationProgress)>,
) -> Result<Operation<()>, GitError> {
    if name.starts_with('-') {
        return Err(GitError::new(
            GitErrorKind::Backend,
            format!("无效的分支名: {name}"),
        ));
    }
    let report = run_cancellable(
        "git",
        Some(repo.root()),
        &[OsStr::new("checkout"), OsStr::new(name), OsStr::new("--")],
        options,
        progress,
    )?;
    match report.outcome {
        OperationOutcome::Completed => Ok(Operation::Completed { value: (), report }),
        OperationOutcome::Cancelled => Ok(Operation::Cancelled(report)),
        OperationOutcome::TimedOut => Ok(Operation::TimedOut(report)),
    }
}

fn run_cancellable(
    program: &str,
    cwd: Option<&Path>,
    args: &[&OsStr],
    options: &OperationOptions,
    mut progress: Option<&mut dyn FnMut(OperationProgress)>,
) -> Result<OperationReport, GitError> {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // 独立进程组:取消时连同 git 派生的子进程一起回收(Windows 用 taskkill /T)。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(spawn_error)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (event_tx, event_rx) = mpsc::sync_channel::<OperationProgress>(64);
    let limit = if options.stderr_tail_bytes == 0 {
        256 * 1024
    } else {
        options.stderr_tail_bytes
    };

    let out_reader = stdout.map(|mut pipe| {
        let tx = event_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = tx.try_send(OperationProgress::StdoutChunk(n));
                    }
                }
            }
        })
    });
    let err_reader = stderr.map(|mut pipe| {
        let tx = event_tx.clone();
        std::thread::spawn(move || {
            let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
            let mut truncated = false;
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = tx.try_send(OperationProgress::StderrChunk(n));
                        for byte in &buf[..n] {
                            if tail.len() >= limit {
                                tail.pop_front();
                                truncated = true;
                            }
                            tail.push_back(*byte);
                        }
                    }
                }
            }
            (tail.into_iter().collect::<Vec<u8>>(), truncated)
        })
    });

    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let mut outcome = OperationOutcome::Completed;
    let status = loop {
        while let Ok(event) = event_rx.try_recv() {
            if let Some(callback) = progress.as_deref_mut() {
                callback(event);
            }
        }
        if options
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
        {
            outcome = OperationOutcome::Cancelled;
            kill_process_tree(&mut child);
            break child.wait().ok();
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            outcome = OperationOutcome::TimedOut;
            kill_process_tree(&mut child);
            break child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(done)) => break Some(done),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(spawn_error(error)),
        }
    };
    while let Ok(event) = event_rx.try_recv() {
        if let Some(callback) = progress.as_deref_mut() {
            callback(event);
        }
    }
    let (stderr_bytes, stderr_truncated) = err_reader
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    if let Some(handle) = out_reader {
        let _ = handle.join();
    }
    let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
    let report = OperationReport {
        outcome,
        stderr: stderr.clone(),
        stderr_truncated,
    };
    if outcome == OperationOutcome::Completed && status.is_some_and(|s| s.success()) {
        return Ok(report);
    }
    if outcome != OperationOutcome::Completed {
        return Ok(report);
    }
    let mut message = stderr;
    if stderr_truncated {
        message.push_str("\n… stderr 已截断");
    }
    Err(GitError::new(
        GitErrorKind::Backend,
        if message.is_empty() {
            "git 命令失败(无 stderr)".to_string()
        } else {
            message
        },
    ))
}

fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // 负 pid = 整个进程组;spawn 时已 process_group(0)。
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .output();
    }
    let _ = child.kill();
}

/// 跑 `program args...`,成功返回 `Ok`;失败带 stderr 原文。`program` 可替换,只为了测试
/// "找不到 git" 的分支。
fn run_git(program: &str, cwd: Option<&Path>, args: &[&OsStr]) -> Result<(), GitError> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    // 不允许 git 在终端上询问凭据:从终端启动的 GUI 否则会一直等一个看不见的输入。
    // 凭据助手 / SSH agent 照常工作,拿不到凭据就直接失败并带 stderr。
    cmd.env("GIT_TERMINAL_PROMPT", "0");
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
    fn a_dash_prefixed_branch_name_is_rejected_not_taken_as_an_option() {
        let t = two_branches();
        let err = t.open().checkout_branch("--detach").unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::Backend);
        assert!(err.message().contains("--detach"), "{}", err.message());
        assert_eq!(head_branch(&t).as_deref(), Some("main"));
    }

    #[test]
    fn a_branch_with_the_same_name_as_a_file_checks_out_the_branch() {
        let t = two_branches();
        t.branch("b.txt");
        t.open().checkout_branch("b.txt").unwrap();
        assert_eq!(head_branch(&t).as_deref(), Some("b.txt"));
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

    #[cfg(unix)]
    #[test]
    fn a_cancelled_operation_is_reaped_and_reported_as_cancelled() {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let started = std::time::Instant::now();
        let report = run_cancellable(
            "sh",
            None,
            &[OsStr::new("-c"), OsStr::new("sleep 30")],
            &OperationOptions {
                cancel: Some(cancel),
                ..OperationOptions::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, OperationOutcome::Cancelled);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn an_operation_timeout_is_a_typed_result() {
        let report = run_cancellable(
            "sh",
            None,
            &[OsStr::new("-c"), OsStr::new("sleep 30")],
            &OperationOptions {
                timeout: Some(std::time::Duration::from_millis(100)),
                ..OperationOptions::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(report.outcome, OperationOutcome::TimedOut);
    }

    #[cfg(unix)]
    #[test]
    fn stderr_tail_is_bounded_and_marked_when_truncated() {
        let err = run_cancellable(
            "sh",
            None,
            &[
                OsStr::new("-c"),
                OsStr::new(
                    "i=0; while [ $i -lt 100 ]; do printf 0123456789 >&2; i=$((i+1)); done; exit 1",
                ),
            ],
            &OperationOptions {
                stderr_tail_bytes: 16,
                ..OperationOptions::default()
            },
            None,
        )
        .unwrap_err();
        assert!(err.message().contains("stderr 已截断"), "{}", err.message());
    }

    #[cfg(unix)]
    #[test]
    fn progress_is_bounded_and_drained() {
        let mut events = 0usize;
        let report = run_cancellable(
            "sh",
            None,
            &[
                OsStr::new("-c"),
                OsStr::new("printf hello; printf world >&2"),
            ],
            &OperationOptions::default(),
            Some(&mut |_event| events += 1),
        )
        .unwrap();
        assert_eq!(report.outcome, OperationOutcome::Completed);
        assert!(events > 0, "至少应收到一个输出事件");
    }
}

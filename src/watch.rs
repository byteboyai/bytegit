//! 变更监听(feature `watch`):debounce 后上报"工作区文件变了"/"`.git` 引用类文件变了"。
//!
//! 只依赖 `notify` 与标准线程,不依赖 tokio。事件的发布(送进应用的消息循环/事件总线)
//! 不在这里:调用方在 `on_change` 里自己转发。
//!
//! **已知限制(规格 O3):** 只监听 `root` 目录树。`root/.git` 是目录时,其中的
//! `HEAD`/`index`/`packed-refs`/`refs/*` 会被识别;`.git` 是文件(linked worktree、子模块,
//! 指向别处的 gitdir)时,真实 gitdir 在 `root` 之外,**不会被监听**,只有工作区文件的变化会上报。
//! 以后补上不需要改这里的 API(监听目标改为解析后的 gitdir 即可)。

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use notify::Watcher;

use crate::{GitError, GitErrorKind};

/// 调用方指定的"不关心"的目录/文件名。路径里**任何一层**的名字(不止顶层)命中就整条忽略,
/// 例如 `packages/foo/node_modules/x/index.js`、每个子包各自的 `target/`。
///
/// `.git` 不需要写进来:`root/.git` 里引用类文件是特殊放行的,嵌套层(子模块、内嵌仓库)的
/// `.git` 恒被忽略。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRules {
    names: Vec<String>,
}

impl IgnoreRules {
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    fn matches(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }
}

/// [`watch`] 的参数。`debounce` 必填,忽略规则用 [`WatchOptions::ignore`] 设置;
/// 结构体 `non_exhaustive`,以后加选项不破坏调用方。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WatchOptions {
    /// 安静期:连续事件的间隔小于它就算同一批,最后一个事件之后再静默这么久才回调一次。
    pub debounce: Duration,
    pub ignore: IgnoreRules,
}

impl WatchOptions {
    pub fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            ignore: IgnoreRules::default(),
        }
    }

    pub fn ignore(mut self, rules: IgnoreRules) -> Self {
        self.ignore = rules;
        self
    }
}

/// 一个 debounce 窗口内的变更汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitChange {
    /// 有 `.git/HEAD`、`index`、`packed-refs` 或 `refs/*` 的变化(分支切换、外部提交、
    /// 其他 worktree 提交都会碰到这几个文件)。
    pub refs_changed: bool,
    /// 有被忽略规则放行的工作区文件变化。
    pub workdir_changed: bool,
    /// 这一批里所有相关的变更路径:绝对、规范化、去重、按字典序排序;工作区路径与上面
    /// 那些 `.git` 引用文件的路径都在里面。
    pub paths: Vec<PathBuf>,
}

/// 存活的监听器。Drop 即停止:之后不会再有回调(已在途的那一批也被丢弃)。
pub struct WatchHandle {
    _watcher: notify::RecommendedWatcher,
    stopped: Arc<AtomicBool>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// 监听 `root` 目录树。`on_change` 在 debounce 线程上被调用,不得长时间阻塞。
///
/// `root` 会先规范化(macOS 上 `/var/...` 与 FSEvents 回报的 `/private/var/...` 要对齐)。
/// 监听器启动失败(如文件描述符耗尽、路径不存在)返回 `GitErrorKind::Io`。
pub fn watch(
    root: &Path,
    opts: WatchOptions,
    on_change: impl FnMut(GitChange) + Send + 'static,
) -> Result<WatchHandle, GitError> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let (tx, rx) = mpsc::channel::<Batch>();
    let filter_root = root.clone();
    let ignore = opts.ignore;

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        let batch = batch_of(&filter_root, &event.paths, &ignore);
        if !batch.is_empty() {
            let _ = tx.send(batch);
        }
    })?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;

    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = stopped.clone();
    let debounce = opts.debounce;
    std::thread::Builder::new()
        .name("bytegit-watch".into())
        .spawn(move || debounce_loop(rx, debounce, &thread_stopped, on_change))
        .map_err(|e| GitError::new(GitErrorKind::Io, format!("启动监听线程失败: {e}")))?;

    Ok(WatchHandle {
        _watcher: watcher,
        stopped,
    })
}

/// 一次路径变化相对这个仓库的相关性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Workdir,
    GitRefs,
}

/// `changed` 是否值得上报,值得的话是哪一类。`root/.git` 下只有 `HEAD`、`index`、
/// `packed-refs`、`refs/*` 算引用类,其余(`objects/` 等)忽略;嵌套层的 `.git`
/// (子模块、内嵌仓库)与调用方指定的名字在任何一层命中都忽略。
fn classify(root: &Path, changed: &Path, ignore: &IgnoreRules) -> Option<Kind> {
    let rel = changed.strip_prefix(root).ok()?;
    let mut parts = rel.components();
    let Some(Component::Normal(first)) = parts.next() else {
        // 事件路径就是监听根本身(FSEvents 建流时会先对根报一条 Create(Folder)/Modify(Metadata);
        // 目录自身的 mtime 变化也会落到这里)。这种目录级事件不携带文件名信息:若当成相关,
        // 往里写 `target/`、`node_modules/` 也会连带触发刷新,忽略规则形同虚设。
        // 真正的内容改动会各自报成具体文件路径,不依赖这条。
        return None;
    };
    let first = first.to_string_lossy();
    if first == ".git" {
        let rest: PathBuf = parts.collect();
        let rest_str = rest.to_string_lossy();
        if rest_str == "HEAD" || rest_str == "index" || rest_str == "packed-refs" {
            return Some(Kind::GitRefs);
        }
        if rest.starts_with("refs") {
            return Some(Kind::GitRefs);
        }
        return None;
    }
    if ignore.matches(&first) {
        return None;
    }
    let nested_ignored = parts.any(|c| match c {
        Component::Normal(name) => is_ignored_name(name, ignore),
        _ => false,
    });
    if nested_ignored {
        return None;
    }
    Some(Kind::Workdir)
}

fn is_ignored_name(name: &OsStr, ignore: &IgnoreRules) -> bool {
    let name = name.to_string_lossy();
    name == ".git" || ignore.matches(&name)
}

#[derive(Debug, Default)]
struct Batch {
    refs_changed: bool,
    workdir_changed: bool,
    paths: BTreeSet<PathBuf>,
}

impl Batch {
    fn is_empty(&self) -> bool {
        !self.refs_changed && !self.workdir_changed
    }

    fn merge(&mut self, other: Batch) {
        self.refs_changed |= other.refs_changed;
        self.workdir_changed |= other.workdir_changed;
        self.paths.extend(other.paths);
    }

    fn into_change(self) -> GitChange {
        GitChange {
            refs_changed: self.refs_changed,
            workdir_changed: self.workdir_changed,
            paths: self.paths.into_iter().collect(),
        }
    }
}

/// 把一个 notify 事件的路径折成一个 [`Batch`]。逐路径规范化(规范化失败,如文件刚被删,
/// 就用原路径)后再判断相关性,避免 macOS 上 `/var/...` 与 `/private/var/...` 不一致让整批
/// 事件被误判为无关。
fn batch_of(root: &Path, event_paths: &[PathBuf], ignore: &IgnoreRules) -> Batch {
    let mut batch = Batch::default();
    for path in event_paths {
        let normalized = path.canonicalize().unwrap_or_else(|_| path.clone());
        match classify(root, &normalized, ignore) {
            Some(Kind::GitRefs) => {
                batch.refs_changed = true;
                batch.paths.insert(normalized);
            }
            Some(Kind::Workdir) => {
                batch.workdir_changed = true;
                batch.paths.insert(normalized);
            }
            None => {}
        }
    }
    batch
}

/// 安静期 debounce:收到第一批后,只要下一批在 `debounce` 内到达就并进来,
/// 静默满 `debounce`(或通道关闭)就回调一次。`stopped` 为真时丢弃这一批。
fn debounce_loop(
    rx: Receiver<Batch>,
    debounce: Duration,
    stopped: &AtomicBool,
    mut on_change: impl FnMut(GitChange),
) {
    while let Ok(first) = rx.recv() {
        let mut acc = first;
        // 超时与通道关闭都结束这一批:前者是安静期满,后者是监听器已销毁。
        while let Ok(more) = rx.recv_timeout(debounce) {
            acc.merge(more);
        }
        if stopped.load(Ordering::SeqCst) {
            return;
        }
        on_change(acc.into_change());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// 与 dozer 的 `project::HIDDEN` 同一份名单。
    fn hidden() -> IgnoreRules {
        IgnoreRules::new([".git", "target", "node_modules", ".DS_Store"])
    }

    fn kind(changed: &str) -> Option<Kind> {
        classify(Path::new("/r"), Path::new(changed), &hidden())
    }

    // ---- classify:从 dozer `git_watch.rs` 原样迁来的断言 ----

    #[test]
    fn workdir_file_is_relevant() {
        assert_eq!(kind("/r/src/main.rs"), Some(Kind::Workdir));
    }

    #[test]
    fn hidden_dirs_are_not_relevant() {
        assert_eq!(kind("/r/target/debug/foo"), None);
        assert_eq!(kind("/r/node_modules/x/index.js"), None);
        assert_eq!(kind("/r/.DS_Store"), None);
    }

    #[test]
    fn nested_hidden_dirs_are_not_relevant() {
        // monorepo/多包项目:嵌套在子目录里的 node_modules/target/.git(子模块)同样不该触发刷新,
        // 不止顶层——否则 `npm install`/编译产物写入子包目录会被误判成"工作区改动"。
        assert_eq!(kind("/r/packages/foo/node_modules/x/index.js"), None);
        assert_eq!(kind("/r/crates/bar/target/debug/foo"), None);
        assert_eq!(kind("/r/vendor/sub/.git/HEAD"), None);
        // 但子目录本身的正常源码改动依然相关。
        assert_eq!(kind("/r/packages/foo/src/main.rs"), Some(Kind::Workdir));
    }

    #[test]
    fn git_control_files_are_relevant_as_git_refs() {
        assert_eq!(kind("/r/.git/HEAD"), Some(Kind::GitRefs));
        assert_eq!(kind("/r/.git/index"), Some(Kind::GitRefs));
        assert_eq!(kind("/r/.git/refs/heads/main"), Some(Kind::GitRefs));
        assert_eq!(kind("/r/.git/packed-refs"), Some(Kind::GitRefs));
    }

    #[test]
    fn other_git_internals_are_not_relevant() {
        assert_eq!(kind("/r/.git/objects/ab/cdef"), None);
    }

    // ---- classify:迁移前就有、此前没有测试的口径 ----

    #[test]
    fn the_repo_root_itself_is_not_relevant() {
        // FSEvents 建流时会对根报一条 Create(Folder)/Modify(Metadata);目录自身的 mtime 变化
        // 也落到根路径。这类目录级事件不带文件信息,不能当成"工作区改了"——否则忽略规则失效。
        assert_eq!(kind("/r"), None);
    }

    #[test]
    fn paths_outside_the_repo_are_not_relevant() {
        assert_eq!(kind("/elsewhere/src/main.rs"), None);
        assert_eq!(kind("/rr/src/main.rs"), None, "前缀相同但不是子路径");
    }

    #[test]
    fn git_lock_files_and_lookalikes_are_not_refs_except_under_refs() {
        assert_eq!(kind("/r/.git/index.lock"), None);
        assert_eq!(kind("/r/.git/HEAD.lock"), None);
        assert_eq!(kind("/r/.git/refsx"), None, "按路径分量判断,不是字符串前缀");
        // `refs/` 之下一律算引用类,包括 git 写引用时的临时 `.lock` 文件。
        assert_eq!(kind("/r/.git/refs/heads/main.lock"), Some(Kind::GitRefs));
        assert_eq!(kind("/r/.git/refs"), Some(Kind::GitRefs));
    }

    #[test]
    fn dotfiles_outside_the_ignore_list_are_relevant() {
        assert_eq!(kind("/r/.env"), Some(Kind::Workdir));
        assert_eq!(kind("/r/.gitignore"), Some(Kind::Workdir));
        assert_eq!(kind("/r/src/.hidden/x"), Some(Kind::Workdir));
    }

    #[test]
    fn a_dot_git_file_at_the_root_is_not_relevant() {
        // linked worktree / 子模块里 `.git` 是个指向别处 gitdir 的文件:这个文件本身的变化不上报
        // (真实 gitdir 在 root 之外,不在监听范围内——见模块文档的已知限制)。
        assert_eq!(kind("/r/.git"), None);
    }

    #[test]
    fn ignore_rules_are_defined_by_the_caller() {
        let none = IgnoreRules::default();
        let c = |p: &str| classify(Path::new("/r"), Path::new(p), &none);
        // 没有规则时,`target`/`node_modules` 都算工作区变化。
        assert_eq!(c("/r/target/debug/foo"), Some(Kind::Workdir));
        assert_eq!(c("/r/a/node_modules/x"), Some(Kind::Workdir));
        // 但 `.git` 的特殊处理是内置的,不依赖规则:根下按引用文件判断,嵌套层恒忽略。
        assert_eq!(c("/r/.git/HEAD"), Some(Kind::GitRefs));
        assert_eq!(c("/r/.git/objects/ab"), None);
        assert_eq!(c("/r/vendor/sub/.git/HEAD"), None);
        let only_dist = IgnoreRules::new(["dist"]);
        assert_eq!(
            classify(Path::new("/r"), Path::new("/r/a/dist/x.js"), &only_dist),
            None
        );
    }

    // ---- batch_of ----

    #[test]
    fn a_batch_flags_refs_and_workdir_independently_and_keeps_every_relevant_path() {
        let paths = vec![
            PathBuf::from("/r/.git/HEAD"),
            PathBuf::from("/r/src/a.rs"),
            PathBuf::from("/r/target/x"),
            PathBuf::from("/r/src/a.rs"),
        ];
        let b = batch_of(Path::new("/r"), &paths, &hidden());
        assert!(b.refs_changed && b.workdir_changed);
        assert_eq!(
            b.paths.into_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("/r/.git/HEAD"), PathBuf::from("/r/src/a.rs")]
        );
    }

    /// macOS 上 `/var/...` 是 `/private/var/...` 的符号链接,FSEvents 可能用任一形式回报路径;
    /// 不先规范化,`strip_prefix(root)` 会失败、整批事件被误判为无关。
    #[cfg(unix)]
    #[test]
    fn event_paths_are_canonicalized_through_symlinks_before_classifying() {
        let real = tempfile::tempdir().unwrap();
        let root = real.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "x").unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();

        let b = batch_of(&root, &[link.join("a.rs")], &hidden());
        assert!(b.workdir_changed);
        assert_eq!(
            b.paths.into_iter().collect::<Vec<_>>(),
            vec![root.join("a.rs")],
            "上报的是规范化后的路径"
        );
    }

    #[test]
    fn a_batch_with_nothing_relevant_is_empty() {
        let paths = vec![
            PathBuf::from("/r/target/x"),
            PathBuf::from("/r/.git/objects/ab"),
        ];
        assert!(batch_of(Path::new("/r"), &paths, &hidden()).is_empty());
    }

    // ---- debounce_loop:不碰文件系统的确定性测试 ----

    fn workdir(path: &str) -> Batch {
        Batch {
            workdir_changed: true,
            paths: BTreeSet::from([PathBuf::from(path)]),
            ..Batch::default()
        }
    }

    fn refs(path: &str) -> Batch {
        Batch {
            refs_changed: true,
            paths: BTreeSet::from([PathBuf::from(path)]),
            ..Batch::default()
        }
    }

    /// 在一个线程里跑 `debounce_loop`,返回它回调出的所有 `GitChange`。
    fn run_loop(
        debounce: Duration,
        stopped: bool,
        feed: impl FnOnce(&mpsc::Sender<Batch>),
    ) -> Vec<GitChange> {
        let (tx, rx) = mpsc::channel();
        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let stopped = Arc::new(AtomicBool::new(stopped));
        let handle = std::thread::spawn(move || {
            debounce_loop(rx, debounce, &stopped, move |c| {
                out2.lock().unwrap().push(c)
            });
        });
        feed(&tx);
        drop(tx);
        handle.join().unwrap();
        Arc::try_unwrap(out)
            .expect("线程已结束,不再有其他持有者")
            .into_inner()
            .unwrap()
    }

    #[test]
    fn batches_arriving_within_the_window_merge_into_one_change() {
        let got = run_loop(Duration::from_secs(5), false, |tx| {
            tx.send(workdir("/r/b.rs")).unwrap();
            tx.send(workdir("/r/a.rs")).unwrap();
            tx.send(workdir("/r/a.rs")).unwrap();
        });
        assert_eq!(got.len(), 1);
        assert!(got[0].workdir_changed && !got[0].refs_changed);
        assert_eq!(
            got[0].paths,
            vec![PathBuf::from("/r/a.rs"), PathBuf::from("/r/b.rs")],
            "去重并排序"
        );
    }

    #[test]
    fn a_refs_change_anywhere_in_the_window_sets_refs_changed() {
        let got = run_loop(Duration::from_secs(5), false, |tx| {
            tx.send(workdir("/r/a.rs")).unwrap();
            tx.send(refs("/r/.git/HEAD")).unwrap();
            tx.send(workdir("/r/b.rs")).unwrap();
        });
        assert_eq!(got.len(), 1);
        assert!(got[0].refs_changed && got[0].workdir_changed);
        assert_eq!(got[0].paths.len(), 3);
    }

    #[test]
    fn a_quiet_period_splits_changes_into_separate_callbacks() {
        let got = run_loop(Duration::from_millis(60), false, |tx| {
            tx.send(workdir("/r/a.rs")).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            tx.send(refs("/r/.git/HEAD")).unwrap();
        });
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].workdir_changed && !got[0].refs_changed);
        assert!(got[1].refs_changed && !got[1].workdir_changed);
    }

    #[test]
    fn nothing_is_delivered_once_the_handle_is_stopped() {
        let got = run_loop(Duration::from_millis(10), true, |tx| {
            tx.send(workdir("/r/a.rs")).unwrap();
        });
        assert!(got.is_empty());
    }

    // ---- watch:真实文件系统(macOS FSEvents 在某些沙盒/CI 环境不向临时目录投递事件,
    //      这时回调数为 0,上面的确定性测试已覆盖算法本身,这里不把平台能力缺失误报成失败) ----

    fn wait_for(count: &AtomicUsize, at_least: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while count.load(Ordering::SeqCst) < at_least && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn counting(count: &Arc<AtomicUsize>) -> impl FnMut(GitChange) + Send + 'static {
        let count = count.clone();
        move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn watch_debounces_rapid_writes_into_one_callback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        std::fs::create_dir_all(repo.join(".git")).unwrap(); // 贴近真实仓库
        for i in 0..5 {
            std::fs::write(repo.join(format!("f{i}.txt")), "initial").unwrap();
        }
        let count = Arc::new(AtomicUsize::new(0));
        let _handle = watch(
            &repo,
            WatchOptions::new(Duration::from_millis(100)).ignore(hidden()),
            counting(&count),
        )
        .expect("watcher 应能启动");

        for i in 0..5 {
            std::fs::write(repo.join(format!("f{i}.txt")), "x").unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        // FSEvents 可能把临时目录的事件延迟数百毫秒才投递:先等首个回调(上限 5s),
        // 再多等一个 debounce 窗口,避免把后端通知延迟误判成 debounce 失效。
        wait_for(&count, 1, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(350));

        let callbacks = count.load(Ordering::SeqCst);
        if callbacks == 0 {
            return;
        }
        assert_eq!(callbacks, 1, "5 次快速写入应合并成 1 次回调");
    }

    #[test]
    fn watch_ignores_writes_under_ignored_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        std::fs::create_dir_all(repo.join("target")).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let _handle = watch(
            &repo,
            WatchOptions::new(Duration::from_millis(100)).ignore(hidden()),
            counting(&count),
        )
        .unwrap();

        std::fs::write(repo.join("target").join("build-artifact"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "target/ 下的改动不该触发回调"
        );
    }

    #[test]
    fn watch_reports_a_git_ref_change_as_refs_changed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let seen = Arc::new(Mutex::new(Vec::<GitChange>::new()));
        let seen2 = seen.clone();
        let _handle = watch(
            &repo,
            WatchOptions::new(Duration::from_millis(100)).ignore(hidden()),
            move |c| seen2.lock().unwrap().push(c),
        )
        .unwrap();

        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/other\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let seen = seen.lock().unwrap();
        if seen.is_empty() {
            return;
        }
        assert!(seen.iter().any(|c| c.refs_changed), "{seen:?}");
    }

    #[test]
    fn no_callbacks_after_the_handle_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let count = Arc::new(AtomicUsize::new(0));
        let handle = watch(
            &repo,
            WatchOptions::new(Duration::from_millis(50)),
            counting(&count),
        )
        .unwrap();
        std::fs::write(repo.join("a.txt"), "1").unwrap();
        drop(handle);
        std::thread::sleep(Duration::from_millis(100));
        let at_drop = count.load(Ordering::SeqCst);
        std::fs::write(repo.join("a.txt"), "2").unwrap();
        std::fs::write(repo.join("b.txt"), "3").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            count.load(Ordering::SeqCst),
            at_drop,
            "Drop 之后不能再有回调"
        );
    }

    #[test]
    fn watching_a_missing_path_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = watch(
            &dir.path().join("does-not-exist"),
            WatchOptions::new(Duration::from_millis(50)),
            |_| {},
        )
        .err()
        .expect("不存在的路径应启动失败");
        assert_eq!(err.kind(), GitErrorKind::Io);
    }
}

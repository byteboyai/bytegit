//! 工作区监听(feature `watch`):按 [`RepoIdentity`] 监听整个工作区,分类 Git 变化原因,
//! 并在有界聚合后投递 [`WorkspaceChange`]。
//!
//! 与旧 [`crate::watch`] 的关系:
//!
//! - 旧 API(`watch`/`GitChange`/`WatchOptions`)保持源码兼容,不改变语义;
//! - 新 API 以 [`crate::WorkspaceKey`] 为前提,分别识别私有 gitdir(HEAD/index)、
//!   common dir(refs/packed-refs/config/shallow)与工作区路径,不再把 `git add` 伪装成
//!   "引用变化";
//! - 新 API **不套用树/预览的隐藏名单**:`target/`、`node_modules/` 里可能有已跟踪文件,
//!   事件必须交给上层按"已跟踪路径集合 + Git ignore"判定。隐藏名单只属于树/预览投影。
//!
//! 资源边界:事件只进入一个有界合并状态(路径条数/字节上限),唤醒信号可合并;
//! 安静期 `quiet_delay` 与"从首事件起算"的 `max_batch_age` 双期限;超限时清空路径明细
//! 并升级为 [`WorkspaceChangeKind::FULL_RESCAN`],原因位不会丢。
//!
//! 生命周期:`Drop` 请求停止并**抑制尚未准入的回调**;已经进入 callback 的那一批允许完成。
//! 需要确认线程退出时用 [`WorkspaceWatchHandle::shutdown`];它不能在 worker 自身调用。
//!
//! **已知限制:** 普通非 Git 目录没有 [`RepoIdentity`],不属于本 API;调用方继续用旧
//! [`crate::watch`] 监听它,在发现 `.git` 创建后用 `Repo::discover_workdir` 重新解析身份并
//! 切换到 `watch_workspace`。身份存在期间 `.git` 被删除或重指向会投递
//! [`WorkspaceChangeKind::FULL_RESCAN`],由调用方重新 discover 并重建监听(本 API 不在
//! 内部自动换身份)。

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use notify::Watcher;

use crate::{GitError, GitErrorKind, RepoIdentity};

/// 默认安静期。
pub const DEFAULT_QUIET_DELAY: Duration = Duration::from_millis(300);
/// 默认单批最长等待:从首个相关事件起算,持续事件不会延长它。
pub const DEFAULT_MAX_BATCH_AGE: Duration = Duration::from_millis(1_000);
/// 默认路径条数上限。
pub const DEFAULT_MAX_PATHS: usize = 4_096;
/// 默认路径累计估算字节上限(1MiB)。
pub const DEFAULT_MAX_PATH_BYTES: usize = 1024 * 1024;

const STOP_POLL: Duration = Duration::from_millis(250);

/// 一批变化的原因位。可以用 [`WorkspaceChangeKind::insert`]/[`WorkspaceChangeKind::union`]
/// 或 `|` 组合。新增位是增量 API,调用方应只检查自己关心的位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WorkspaceChangeKind(u8);

impl WorkspaceChangeKind {
    pub const NONE: Self = Self(0);
    /// 工作区(树/预览/已跟踪文件)有路径变化。
    pub const WORKDIR: Self = Self(1 << 0);
    /// 暂存区(index)变化,例如 `git add`。
    pub const INDEX: Self = Self(1 << 1);
    /// HEAD 本身变化(切换分支、detach)。
    pub const HEAD: Self = Self(1 << 2);
    /// 引用变化(`refs/*`、`packed-refs`),例如提交、外部 fetch。
    pub const REFS: Self = Self(1 << 3);
    /// 仓库配置变化(remote、ignore/attributes 等)。
    pub const CONFIG: Self = Self(1 << 4);
    /// 历史边界变化(shallow、grafts、replace、alternates)。
    pub const HISTORY_BOUNDARY: Self = Self(1 << 5);
    /// 路径明细不足以做局部刷新;必须做一次完整重扫。它**不是**"没有文件变化"。
    pub const FULL_RESCAN: Self = Self(1 << 6);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `self` 是否包含 `other` 的全部位。
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl std::ops::BitOr for WorkspaceChangeKind {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for WorkspaceChangeKind {
    fn bitor_assign(&mut self, rhs: Self) {
        self.insert(rhs);
    }
}

/// 新工作区监听的参数。默认值见 [`WorkspaceWatchOptions::default`]。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkspaceWatchOptions {
    /// 安静期:最后一个相关事件之后再等这么久才投递。
    pub quiet_delay: Duration,
    /// 单批最长等待:从首个相关事件起算;持续写入也会按它定期投递。
    pub max_batch_age: Duration,
    /// 单批路径条数上限;超限清空明细并置 [`WorkspaceChangeKind::FULL_RESCAN`]。
    pub max_paths: usize,
    /// 单批路径累计估算字节上限;与条数同时限制。
    pub max_path_bytes: usize,
}

impl Default for WorkspaceWatchOptions {
    fn default() -> Self {
        Self {
            quiet_delay: DEFAULT_QUIET_DELAY,
            max_batch_age: DEFAULT_MAX_BATCH_AGE,
            max_paths: DEFAULT_MAX_PATHS,
            max_path_bytes: DEFAULT_MAX_PATH_BYTES,
        }
    }
}

impl WorkspaceWatchOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn quiet_delay(mut self, delay: Duration) -> Self {
        self.quiet_delay = delay;
        self
    }

    pub fn max_batch_age(mut self, age: Duration) -> Self {
        self.max_batch_age = age;
        self
    }

    pub fn limits(mut self, max_paths: usize, max_path_bytes: usize) -> Self {
        self.max_paths = max_paths;
        self.max_path_bytes = max_path_bytes;
        self
    }
}

/// 一个监听目标启动失败,或通知后端报错。失败不会让监听静默停更:仍然可用的目标继续工作,
/// 失败信息随下一批 [`WorkspaceChange`] 投递(或启动时经 [`WorkspaceWatchHandle::degraded`]
/// 暴露)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchDegraded {
    /// 失败的目标;全局后端错误为 `None`。
    pub path: Option<PathBuf>,
    pub message: String,
}

/// 一批有界的工作区变化。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkspaceChange {
    /// 原因位;同一批里多个原因同时保留。
    pub kinds: WorkspaceChangeKind,
    /// 有上限的逻辑变更路径(绝对路径,去重、按字典序);`FULL_RESCAN` 时为空。
    /// 工作区路径保持通知给出的形式,不逐文件 canonicalize(不跟随 symlink)。
    pub paths: Vec<PathBuf>,
    /// 每个监听句柄内单调递增,从 1 开始。
    pub seq: u64,
    /// 这一批附带的新降级信息。
    pub degraded: Vec<WatchDegraded>,
}

impl WorkspaceChange {
    /// 这一批要求完整重扫。
    pub fn is_full_rescan(&self) -> bool {
        self.kinds.contains(WorkspaceChangeKind::FULL_RESCAN)
    }
}

/// 存活的工作区监听器。
pub struct WorkspaceWatchHandle {
    _watcher: notify::RecommendedWatcher,
    shared: Arc<Shared>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    worker_id: ThreadId,
    degraded: Vec<WatchDegraded>,
}

impl WorkspaceWatchHandle {
    /// 启动时部分目标监听失败的信息(仍可用的目标继续工作)。运行时错误在
    /// [`WorkspaceChange::degraded`] 里。
    pub fn degraded(&self) -> &[WatchDegraded] {
        &self.degraded
    }

    /// 停止监听并等待 worker 退出。不能在监听回调线程内调用(返回
    /// [`GitErrorKind::Io`],不会自等待)。普通 UI 路径用 `Drop` 即可,不需要等待。
    pub fn shutdown(mut self) -> Result<(), GitError> {
        if thread::current().id() == self.worker_id {
            return Err(GitError::new(
                GitErrorKind::Io,
                "不能从监听回调线程内 shutdown/join",
            ));
        }
        self.stop();
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| GitError::new(GitErrorKind::Io, "监听线程异常退出")),
            None => Ok(()),
        }
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
    }
}

impl Drop for WorkspaceWatchHandle {
    fn drop(&mut self) {
        // 只请求停止,不 join:UI 线程 Drop 不能等待可能阻塞的 callback。
        // 已准入的回调允许完成,未准入的批次会被 stopped 检查抑制。
        self.stop();
    }
}

/// 监听 `identity` 对应的工作区。回调运行在专用 worker 线程上,只应转发消息,不得长时间阻塞。
///
/// 裸仓库没有工作区,返回 [`GitErrorKind::NotARepo`]。工作区根监听失败是致命错误;
/// 私有 gitdir / common dir 的部分目标失败进入 `degraded`,不会拖垮工作区监听。
pub fn watch_workspace(
    identity: &RepoIdentity,
    opts: WorkspaceWatchOptions,
    on_change: impl FnMut(WorkspaceChange) + Send + 'static,
) -> Result<WorkspaceWatchHandle, GitError> {
    let classifier = Classifier::new(identity)?;
    let workdir = classifier.workdir.clone();
    let targets = extra_targets(&classifier);

    let shared = Arc::new(Shared {
        state: Mutex::new(Aggregator::new(opts)),
        wake: Condvar::new(),
    });
    let stopped = Arc::new(AtomicBool::new(false));

    let callback_shared = shared.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let raw = match res {
            Ok(event) => {
                // 「文件被打开/读取」不是变更:Linux 的 inotify 会报它,自身读文件会反过来
                // 触发刷新。
                if matches!(event.kind, notify::EventKind::Access(_)) {
                    return;
                }
                classifier.classify_event(&event)
            }
            Err(e) => RawChange::global_error(e.to_string()),
        };
        if raw.is_empty() {
            return;
        }
        {
            let mut state = lock(&callback_shared.state);
            state.push(raw, Instant::now());
        }
        callback_shared.wake.notify_one();
    })?;

    // 工作区根的失败是致命的:没有它就不再是"工作区监听"。
    watcher.watch(&workdir, notify::RecursiveMode::Recursive)?;

    let mut degraded = Vec::new();
    for target in targets {
        if let Err(e) = watcher.watch(&target.path, target.mode) {
            degraded.push(WatchDegraded {
                path: Some(target.path),
                message: e.to_string(),
            });
        }
    }

    let thread_shared = shared.clone();
    let thread_stopped = stopped.clone();
    let worker = thread::Builder::new()
        .name("bytegit-workspace-watch".into())
        .spawn(move || worker_loop(thread_shared, thread_stopped, on_change))
        .map_err(|e| GitError::new(GitErrorKind::Io, format!("启动监听线程失败: {e}")))?;
    let worker_id = worker.thread().id();

    Ok(WorkspaceWatchHandle {
        _watcher: watcher,
        shared,
        stopped,
        worker: Some(worker),
        worker_id,
        degraded,
    })
}

struct Shared {
    state: Mutex<Aggregator>,
    wake: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn worker_loop(
    shared: Arc<Shared>,
    stopped: Arc<AtomicBool>,
    mut on_change: impl FnMut(WorkspaceChange),
) {
    loop {
        let change = {
            let mut state = lock(&shared.state);
            loop {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                let now = Instant::now();
                match state.deadline() {
                    Some(deadline) if now >= deadline => {
                        if let Some(change) = state.take() {
                            break change;
                        }
                    }
                    Some(deadline) => {
                        let (guard, _) = shared
                            .wake
                            .wait_timeout(state, deadline - now)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state = guard;
                    }
                    None => {
                        let (guard, _) = shared
                            .wake
                            .wait_timeout(state, STOP_POLL)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state = guard;
                    }
                }
            }
        };
        on_change(change);
    }
}

// ---- 分类 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitScope {
    /// 当前工作区私有的 gitdir(HEAD/index 属于它)。
    Private,
    /// 共享 common dir(主工作区 HEAD/index 不属于当前工作区)。
    Common,
}

#[derive(Debug, Default)]
struct RawChange {
    kinds: WorkspaceChangeKind,
    paths: Vec<PathBuf>,
    degraded: Vec<WatchDegraded>,
}

impl RawChange {
    fn is_empty(&self) -> bool {
        self.kinds.is_empty() && self.degraded.is_empty()
    }

    fn global_error(message: String) -> Self {
        Self {
            kinds: WorkspaceChangeKind::FULL_RESCAN,
            paths: Vec::new(),
            degraded: vec![WatchDegraded {
                path: None,
                message,
            }],
        }
    }
}

struct Classifier {
    workdir: PathBuf,
    workdir_aliases: Vec<PathBuf>,
    git_dir: PathBuf,
    git_aliases: Vec<PathBuf>,
    common_dir: PathBuf,
    common_aliases: Vec<PathBuf>,
    shares_common: bool,
}

impl Classifier {
    fn new(identity: &RepoIdentity) -> Result<Self, GitError> {
        let Some(workdir) = identity.workdir_root() else {
            return Err(GitError::new(
                GitErrorKind::NotARepo,
                format!(
                    "裸仓库没有工作区,不能建立工作区监听: {}",
                    identity.git_dir().display()
                ),
            ));
        };
        let workdir = workdir.to_path_buf();
        let git_dir = identity.git_dir().to_path_buf();
        let common_dir = identity.common_dir().to_path_buf();
        Ok(Self {
            workdir_aliases: aliases(&workdir),
            workdir,
            git_aliases: aliases(&git_dir),
            git_dir,
            common_aliases: aliases(&common_dir),
            common_dir,
            shares_common: identity.git_dir() == identity.common_dir(),
        })
    }

    fn classify_event(&self, event: &notify::Event) -> RawChange {
        let mut raw = RawChange::default();
        if event.paths.is_empty() {
            // 路径为空的 rescan/后端事件不能解释成"没有变化"。
            raw.kinds.insert(WorkspaceChangeKind::FULL_RESCAN);
            return raw;
        }

        for path in &event.paths {
            let path = lexical_normalize(path);
            let Some(path) = path else {
                raw.kinds.insert(WorkspaceChangeKind::FULL_RESCAN);
                continue;
            };

            // 监听根本身被创建/删除/替换:目录级事件不带文件名,只能完整重扫。
            if self.is_workdir_root(&path) {
                if is_structural_event(&event.kind) {
                    raw.kinds.insert(WorkspaceChangeKind::FULL_RESCAN);
                }
                continue;
            }

            if let Some(kind) = self.classify_path(&path) {
                raw.kinds.insert(kind);
                if !kind.contains(WorkspaceChangeKind::FULL_RESCAN) {
                    raw.paths.push(path);
                }
            }
            if raw.kinds.contains(WorkspaceChangeKind::FULL_RESCAN) {
                raw.paths.clear();
                return raw;
            }
        }
        raw
    }

    fn is_workdir_root(&self, path: &Path) -> bool {
        self.workdir_aliases
            .iter()
            .any(|alias| path == alias.as_path())
    }

    fn classify_path(&self, path: &Path) -> Option<WorkspaceChangeKind> {
        // gitdir 优先:linked worktree 的私有 gitdir 位于 common dir 之下。
        if let Some(rest) = strip_any(&self.git_aliases, path) {
            if rest.as_os_str().is_empty() {
                return Some(WorkspaceChangeKind::FULL_RESCAN);
            }
            return classify_git(rest, GitScope::Private);
        }
        if !self.shares_common
            && let Some(rest) = strip_any(&self.common_aliases, path)
        {
            if rest.as_os_str().is_empty() {
                return Some(WorkspaceChangeKind::FULL_RESCAN);
            }
            return classify_git(rest, GitScope::Common);
        }
        if let Some(rest) = strip_any(&self.workdir_aliases, path) {
            return classify_workdir(rest);
        }
        None
    }
}

/// 工作区(排除已单独处理的 gitdir)内的路径。`.git` 本身或整棵子树的
/// 创建/删除/重指向需要完整重扫;嵌套的 `.git`(子模块、内嵌仓库)不属于本工作区。
fn classify_workdir(rest: &Path) -> Option<WorkspaceChangeKind> {
    if rest.as_os_str().is_empty() {
        return None;
    }
    let mut parts = rest.components();
    let first = match parts.next() {
        Some(Component::Normal(name)) => name.to_string_lossy().into_owned(),
        _ => return Some(WorkspaceChangeKind::FULL_RESCAN),
    };
    if first == ".git" {
        let remainder: PathBuf = parts.collect();
        if remainder.as_os_str().is_empty() {
            // `.git` 被删除/重指向(linked worktree 的引用文件也走这里)。
            return Some(WorkspaceChangeKind::FULL_RESCAN);
        }
        // 正常仓库的 `.git/...` 已由私有 gitdir 规则处理;能走到这里说明它不是当前
        // 身份解析出的 gitdir,按不可解释处理,保守重扫。
        return Some(WorkspaceChangeKind::FULL_RESCAN);
    }
    if parts.any(|c| matches!(c, Component::Normal(name) if name == ".git")) {
        // 嵌套仓库/子模块内部:不冒充外层 refs,也不是外层工作区文件。
        return None;
    }
    Some(WorkspaceChangeKind::WORKDIR)
}

/// 相对某个 git 目录的路径。`scope` 区分私有 gitdir 与 common dir:主工作区的
/// HEAD/index/config.worktree 不能被当成 linked worktree 的私有状态。
fn classify_git(rest: &Path, scope: GitScope) -> Option<WorkspaceChangeKind> {
    use WorkspaceChangeKind as K;

    if rest.as_os_str().is_empty() {
        return Some(K::FULL_RESCAN);
    }
    let mut components = rest.components();
    let first = match components.next() {
        Some(Component::Normal(name)) => name.to_string_lossy().into_owned(),
        _ => return Some(K::FULL_RESCAN),
    };
    let is_leaf = components.next().is_none();

    if is_leaf {
        match first.as_str() {
            "HEAD" => {
                return matches!(scope, GitScope::Private).then_some(K::HEAD);
            }
            "index" => {
                return matches!(scope, GitScope::Private).then_some(K::INDEX);
            }
            "config" => return Some(K::CONFIG),
            "config.worktree" => {
                // 主工作区的私有 config.worktree 不影响 linked worktree。
                return matches!(scope, GitScope::Private).then_some(K::CONFIG);
            }
            "packed-refs" => return Some(K::REFS),
            "shallow" => return Some(K::HISTORY_BOUNDARY),
            "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" | "REBASE_HEAD" | "AUTO_MERGE" => {
                return Some(K::INDEX);
            }
            // 临时锁文件不代表最终状态;refs 下的锁由下面的 refs 分支保守处理。
            "HEAD.lock"
            | "index.lock"
            | "config.lock"
            | "config.worktree.lock"
            | "packed-refs.lock"
            | "shallow.lock" => return None,
            _ => {}
        }
    }

    if rest.starts_with("refs") {
        let mut kinds = K::REFS;
        if rest.starts_with("refs/replace") {
            kinds.insert(K::HISTORY_BOUNDARY);
        }
        return Some(kinds);
    }
    if rest.starts_with("info") {
        if rest == Path::new("info/exclude") || rest == Path::new("info/attributes") {
            return Some(K::CONFIG);
        }
        if rest == Path::new("info/grafts") {
            return Some(K::HISTORY_BOUNDARY);
        }
        return None;
    }
    if rest == Path::new("objects/info/alternates") {
        return Some(K::HISTORY_BOUNDARY);
    }
    if rest.starts_with("rebase-merge")
        || rest.starts_with("rebase-apply")
        || rest.starts_with("sequencer")
    {
        return Some(K::INDEX);
    }
    // objects/、logs/、hooks/、worktrees/、modules/ 等内部目录不直接产生刷新原因。
    None
}

/// `path` 相对某个别名根的余下部分。按路径分量比较,不是字符串前缀。
fn strip_any<'a>(aliases: &'a [PathBuf], path: &'a Path) -> Option<&'a Path> {
    aliases
        .iter()
        .find_map(|alias| path.strip_prefix(alias).ok())
}

/// 纯词法规范化:去掉 `.`、折叠 `..`,不 canonicalize(避免把 symlink 文件事件
/// 变成仓库外目标路径)。
fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// 一个已规范化路径的已知别名。macOS 上 `/var/...` 与 `/private/var/...` 是同一个目录,
/// FSEvents 可能用任一形式回报;只做词法替换,不跟随文件 symlink。
fn aliases(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if let Ok(rest) = path.strip_prefix("/private") {
        out.push(Path::new("/").join(rest));
    } else if let Ok(rest) = path.strip_prefix("/")
        && let Some(Component::Normal(first)) = rest.components().next()
        && matches!(first.to_string_lossy().as_ref(), "var" | "tmp" | "etc")
    {
        out.push(Path::new("/private").join(rest));
    }
    out.dedup();
    out
}

fn is_structural_event(kind: &notify::EventKind) -> bool {
    !matches!(
        kind,
        notify::EventKind::Access(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_))
    )
}

#[derive(Debug, Clone)]
struct Target {
    path: PathBuf,
    mode: notify::RecursiveMode,
}

/// 需要额外监听的目标(工作区根之外)。与工作区重叠时不重复监听。
fn extra_targets(classifier: &Classifier) -> Vec<Target> {
    use notify::RecursiveMode::{NonRecursive, Recursive};
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |path: PathBuf, mode| {
        if path.starts_with(&classifier.workdir) {
            return; // 工作区递归监听已覆盖
        }
        if seen.insert(path.clone()) {
            out.push(Target { path, mode });
        }
    };

    add(classifier.git_dir.clone(), Recursive);
    if !classifier.shares_common && !classifier.common_dir.starts_with(&classifier.git_dir) {
        add(classifier.common_dir.clone(), NonRecursive);
        let refs = classifier.common_dir.join("refs");
        if refs.exists() {
            add(refs, Recursive);
        }
    }
    out
}

// ---- 有界聚合 ----

#[derive(Debug)]
struct Aggregator {
    opts: WorkspaceWatchOptions,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
    kinds: WorkspaceChangeKind,
    paths: BTreeSet<PathBuf>,
    path_bytes: usize,
    degraded: Vec<WatchDegraded>,
    seq: u64,
}

impl Aggregator {
    fn new(opts: WorkspaceWatchOptions) -> Self {
        Self {
            opts,
            first_at: None,
            last_at: None,
            kinds: WorkspaceChangeKind::empty(),
            paths: BTreeSet::new(),
            path_bytes: 0,
            degraded: Vec::new(),
            seq: 0,
        }
    }

    fn has_pending(&self) -> bool {
        !self.kinds.is_empty() || !self.degraded.is_empty()
    }

    /// 投递期限:安静期与"首事件 + max_batch_age"的较早者。持续事件不延长上限。
    fn deadline(&self) -> Option<Instant> {
        let first = self.first_at?;
        let last = self.last_at.unwrap_or(first);
        Some((last + self.opts.quiet_delay).min(first + self.opts.max_batch_age))
    }

    fn push(&mut self, raw: RawChange, now: Instant) {
        if raw.is_empty() {
            return;
        }
        let first = *self.first_at.get_or_insert(now);
        self.last_at = Some(self.last_at.map_or(now, |last| last.max(now)));
        // 保证期限不会早于首事件,即使测试喂入的时间倒退(真实时钟单调)。
        if self.last_at < Some(first) {
            self.last_at = Some(first);
        }
        self.kinds.insert(raw.kinds);
        for item in raw.degraded {
            if !self.degraded.contains(&item) {
                self.degraded.push(item);
            }
        }

        if self.kinds.contains(WorkspaceChangeKind::FULL_RESCAN) {
            self.paths.clear();
            self.path_bytes = 0;
            return;
        }
        for path in raw.paths {
            if self.paths.contains(&path) {
                continue;
            }
            let bytes = path.as_os_str().len();
            if self.paths.len() >= self.opts.max_paths
                || self.path_bytes.saturating_add(bytes) > self.opts.max_path_bytes
            {
                // 超限:清空明细,升级为完整重扫;已收集的原因位保留。
                self.paths.clear();
                self.path_bytes = 0;
                self.kinds.insert(WorkspaceChangeKind::FULL_RESCAN);
                return;
            }
            self.path_bytes += bytes;
            self.paths.insert(path);
        }
    }

    fn take(&mut self) -> Option<WorkspaceChange> {
        if !self.has_pending() {
            self.reset();
            return None;
        }
        self.seq += 1;
        let change = WorkspaceChange {
            kinds: self.kinds,
            paths: std::mem::take(&mut self.paths).into_iter().collect(),
            seq: self.seq,
            degraded: std::mem::take(&mut self.degraded),
        };
        self.reset();
        Some(change)
    }

    fn reset(&mut self) {
        self.first_at = None;
        self.last_at = None;
        self.kinds = WorkspaceChangeKind::empty();
        self.paths.clear();
        self.path_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;
    use std::process::Command;
    use std::sync::OnceLock;
    use std::sync::mpsc::{self, Receiver};

    // ---- 分类:纯路径,不碰真实仓库 ----

    fn fake_identity(workdir: &str, git_dir: &str, common_dir: &str) -> RepoIdentity {
        RepoIdentity::for_test(
            Some(PathBuf::from(workdir)),
            PathBuf::from(git_dir),
            PathBuf::from(common_dir),
        )
    }

    fn classifier(workdir: &str, git_dir: &str, common_dir: &str) -> Classifier {
        Classifier::new(&fake_identity(workdir, git_dir, common_dir)).unwrap()
    }

    fn event(paths: &[&str]) -> notify::Event {
        let mut event =
            notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any));
        for path in paths {
            event = event.add_path(PathBuf::from(path));
        }
        event
    }

    fn kinds(c: &Classifier, path: &str) -> Option<WorkspaceChangeKind> {
        c.classify_path(Path::new(path))
    }

    #[test]
    fn workdir_changes_include_hidden_and_ignored_names() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        assert_eq!(
            kinds(&c, "/r/src/main.rs"),
            Some(WorkspaceChangeKind::WORKDIR)
        );
        // 新 API 不能套用树/预览的隐藏名单:target/ 里可能有已跟踪文件。
        assert_eq!(
            kinds(&c, "/r/target/debug/out"),
            Some(WorkspaceChangeKind::WORKDIR)
        );
        assert_eq!(
            kinds(&c, "/r/node_modules/x/index.js"),
            Some(WorkspaceChangeKind::WORKDIR)
        );
    }

    #[test]
    fn private_git_files_map_to_distinct_reasons() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        use WorkspaceChangeKind as K;
        assert_eq!(kinds(&c, "/r/.git/HEAD"), Some(K::HEAD));
        assert_eq!(kinds(&c, "/r/.git/index"), Some(K::INDEX));
        assert_eq!(kinds(&c, "/r/.git/packed-refs"), Some(K::REFS));
        assert_eq!(kinds(&c, "/r/.git/refs/heads/main"), Some(K::REFS));
        assert_eq!(kinds(&c, "/r/.git/config"), Some(K::CONFIG));
        assert_eq!(kinds(&c, "/r/.git/config.worktree"), Some(K::CONFIG));
        assert_eq!(kinds(&c, "/r/.git/shallow"), Some(K::HISTORY_BOUNDARY));
        assert_eq!(
            kinds(&c, "/r/.git/objects/info/alternates"),
            Some(K::HISTORY_BOUNDARY)
        );
        assert_eq!(
            kinds(&c, "/r/.git/refs/replace/abc"),
            Some(K::REFS | K::HISTORY_BOUNDARY)
        );
        assert_eq!(kinds(&c, "/r/.git/info/exclude"), Some(K::CONFIG));
        assert_eq!(kinds(&c, "/r/.git/MERGE_HEAD"), Some(K::INDEX));
    }

    #[test]
    fn transient_lock_files_do_not_invalidate_head_or_index_by_themselves() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        use WorkspaceChangeKind as K;
        assert_eq!(kinds(&c, "/r/.git/HEAD.lock"), None);
        assert_eq!(kinds(&c, "/r/.git/index.lock"), None);
        assert_eq!(kinds(&c, "/r/.git/config.lock"), None);
        // refs 下的锁保守失效。
        assert_eq!(kinds(&c, "/r/.git/refs/heads/main.lock"), Some(K::REFS));
    }

    #[test]
    fn unrelated_git_internals_are_ignored() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        for p in [
            "/r/.git/objects/ab/cdef",
            "/r/.git/logs/HEAD",
            "/r/.git/hooks/pre-commit",
            "/r/.git/COMMIT_EDITMSG",
            "/r/.git/ORIG_HEAD",
            "/r/.git/description",
        ] {
            assert_eq!(kinds(&c, p), None, "{p}");
        }
    }

    #[test]
    fn nested_dot_git_is_not_the_outer_repositorys_refs() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        assert_eq!(kinds(&c, "/r/vendor/sub/.git/HEAD"), None);
        assert_eq!(kinds(&c, "/r/vendor/sub/.git/index"), None);
    }

    #[test]
    fn a_linked_worktree_separates_private_and_shared_state() {
        let c = classifier("/w", "/main/.git/worktrees/w", "/main/.git");
        use WorkspaceChangeKind as K;
        // 私有 HEAD/index 属于本 worktree。
        assert_eq!(kinds(&c, "/main/.git/worktrees/w/HEAD"), Some(K::HEAD));
        assert_eq!(kinds(&c, "/main/.git/worktrees/w/index"), Some(K::INDEX));
        // 共享 refs 影响所有 worktree。
        assert_eq!(kinds(&c, "/main/.git/refs/heads/main"), Some(K::REFS));
        assert_eq!(kinds(&c, "/main/.git/packed-refs"), Some(K::REFS));
        assert_eq!(kinds(&c, "/main/.git/config"), Some(K::CONFIG));
        // 主工作区 HEAD/index 与别的 worktree 私有状态都不属于本工作区。
        assert_eq!(kinds(&c, "/main/.git/HEAD"), None);
        assert_eq!(kinds(&c, "/main/.git/index"), None);
        assert_eq!(kinds(&c, "/main/.git/config.worktree"), None);
        assert_eq!(kinds(&c, "/main/.git/worktrees/other/HEAD"), None);
    }

    #[test]
    fn dot_git_deletion_or_repointing_is_a_full_rescan() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        assert_eq!(kinds(&c, "/r/.git"), Some(WorkspaceChangeKind::FULL_RESCAN));
        // 目录本身被删除(事件路径就是 gitdir)。
        assert_eq!(kinds(&c, "/r/.git"), Some(WorkspaceChangeKind::FULL_RESCAN));
    }

    #[test]
    fn a_root_structural_event_is_a_full_rescan_but_metadata_is_not() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        let remove = notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Any))
            .add_path(PathBuf::from("/r"));
        assert!(
            c.classify_event(&remove)
                .kinds
                .contains(WorkspaceChangeKind::FULL_RESCAN)
        );

        let metadata = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Metadata(notify::event::MetadataKind::Any),
        ))
        .add_path(PathBuf::from("/r"));
        assert!(c.classify_event(&metadata).is_empty());
    }

    #[test]
    fn an_event_without_paths_is_a_full_rescan_not_silence() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        let empty = notify::Event::new(notify::EventKind::Any);
        assert_eq!(
            c.classify_event(&empty).kinds,
            WorkspaceChangeKind::FULL_RESCAN
        );
    }

    #[test]
    fn a_rename_keeps_both_sides() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        let ev = event(&["/r/old.txt", "/r/new.txt"]);
        let raw = c.classify_event(&ev);
        assert_eq!(raw.kinds, WorkspaceChangeKind::WORKDIR);
        assert_eq!(
            raw.paths,
            vec![PathBuf::from("/r/old.txt"), PathBuf::from("/r/new.txt")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_private_var_alias_still_matches_the_logical_path() {
        let c = classifier(
            "/private/var/t/x",
            "/private/var/t/x/.git",
            "/private/var/t/x/.git",
        );
        let ev = event(&["/var/t/x/a.rs"]);
        let raw = c.classify_event(&ev);
        assert_eq!(raw.kinds, WorkspaceChangeKind::WORKDIR);
        assert_eq!(raw.paths, vec![PathBuf::from("/var/t/x/a.rs")]);
    }

    #[test]
    fn paths_outside_all_roots_are_ignored() {
        let c = classifier("/r", "/r/.git", "/r/.git");
        assert_eq!(kinds(&c, "/elsewhere/a"), None);
        assert_eq!(kinds(&c, "/rr/a"), None);
    }

    // ---- 聚合状态机:注入时间,不 sleep ----

    fn workdir_change(path: &str) -> RawChange {
        RawChange {
            kinds: WorkspaceChangeKind::WORKDIR,
            paths: vec![PathBuf::from(path)],
            degraded: Vec::new(),
        }
    }

    fn agg(opts: WorkspaceWatchOptions) -> Aggregator {
        Aggregator::new(opts)
    }

    #[test]
    fn deadline_is_quiet_period_from_last_event() {
        let opts = WorkspaceWatchOptions::new()
            .quiet_delay(Duration::from_millis(300))
            .max_batch_age(Duration::from_secs(10));
        let base = Instant::now();
        let mut a = agg(opts);
        a.push(workdir_change("/r/a"), base);
        assert_eq!(a.deadline(), Some(base + Duration::from_millis(300)));

        a.push(workdir_change("/r/b"), base + Duration::from_millis(200));
        assert_eq!(
            a.deadline(),
            Some(base + Duration::from_millis(500)),
            "安静期跟着最后一个事件走"
        );
    }

    #[test]
    fn a_continuous_stream_is_capped_by_max_batch_age() {
        let opts = WorkspaceWatchOptions::new()
            .quiet_delay(Duration::from_millis(300))
            .max_batch_age(Duration::from_millis(1000));
        let base = Instant::now();
        let mut a = agg(opts);
        for i in 0..20 {
            a.push(
                workdir_change(&format!("/r/{i}")),
                base + Duration::from_millis(i * 50),
            );
        }
        // 最后一个事件在 950ms,安静期本应到 1250ms,但首事件 + 1000ms 封顶。
        assert_eq!(a.deadline(), Some(base + Duration::from_millis(1000)));
        let change = a.take().unwrap();
        assert_eq!(change.seq, 1);
        assert_eq!(change.paths.len(), 20);
    }

    #[test]
    fn consecutive_batches_get_monotonic_sequence_numbers() {
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new());
        a.push(workdir_change("/r/a"), base);
        assert_eq!(a.take().unwrap().seq, 1);
        assert!(a.take().is_none(), "空状态不产生批次");
        a.push(workdir_change("/r/b"), base);
        assert_eq!(a.take().unwrap().seq, 2);
    }

    #[test]
    fn all_reason_bits_survive_a_batch_and_full_rescan_clears_paths() {
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new());
        a.push(
            RawChange {
                kinds: WorkspaceChangeKind::WORKDIR | WorkspaceChangeKind::INDEX,
                paths: vec![PathBuf::from("/r/a")],
                degraded: Vec::new(),
            },
            base,
        );
        a.push(
            RawChange {
                kinds: WorkspaceChangeKind::HEAD | WorkspaceChangeKind::REFS,
                paths: vec![PathBuf::from("/r/.git/HEAD")],
                degraded: Vec::new(),
            },
            base,
        );
        a.push(
            RawChange {
                kinds: WorkspaceChangeKind::FULL_RESCAN,
                paths: vec![PathBuf::from("/r/b")],
                degraded: Vec::new(),
            },
            base,
        );
        let change = a.take().unwrap();
        use WorkspaceChangeKind as K;
        assert!(change.kinds.contains(K::WORKDIR));
        assert!(change.kinds.contains(K::INDEX));
        assert!(change.kinds.contains(K::HEAD));
        assert!(change.kinds.contains(K::REFS));
        assert!(change.kinds.contains(K::FULL_RESCAN));
        assert!(change.paths.is_empty(), "完整重扫不携带局部路径明细");
    }

    #[test]
    fn too_many_paths_clear_details_and_preserve_reasons() {
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new()
            .limits(2, 1024)
            .max_batch_age(Duration::from_secs(1)));
        a.push(workdir_change("/r/a"), base);
        a.push(workdir_change("/r/b"), base);
        a.push(workdir_change("/r/c"), base);
        let change = a.take().unwrap();
        assert!(change.kinds.contains(WorkspaceChangeKind::FULL_RESCAN));
        assert!(change.kinds.contains(WorkspaceChangeKind::WORKDIR));
        assert!(change.paths.is_empty());
    }

    #[test]
    fn an_overlong_single_path_becomes_a_full_rescan() {
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new().limits(100, 8));
        a.push(workdir_change("/r/a-very-long-name"), base);
        let change = a.take().unwrap();
        assert!(change.kinds.contains(WorkspaceChangeKind::FULL_RESCAN));
        assert!(change.paths.is_empty());
    }

    #[test]
    fn duplicate_paths_are_deduplicated_and_counted_once() {
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new().limits(10, 1024));
        a.push(workdir_change("/r/a"), base);
        a.push(workdir_change("/r/a"), base);
        let change = a.take().unwrap();
        assert_eq!(change.paths, vec![PathBuf::from("/r/a")]);
        assert!(!change.kinds.contains(WorkspaceChangeKind::FULL_RESCAN));
    }

    #[test]
    fn a_push_after_a_take_is_not_lost_by_a_stale_wake() {
        // 共享状态 + 可合并唤醒的结构:worker 取走一批后,生产者紧接着写入的新事件
        // 仍然拥有自己的期限,不会因为"唤醒信号已被消费"而永久睡眠。
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new()
            .quiet_delay(Duration::from_millis(100))
            .max_batch_age(Duration::from_secs(1)));
        a.push(workdir_change("/r/a"), base);
        let first = a.take().unwrap();
        assert_eq!(first.seq, 1);

        a.push(workdir_change("/r/b"), base + Duration::from_millis(10));
        assert_eq!(
            a.deadline(),
            Some(base + Duration::from_millis(110)),
            "取走之后的新事件重新计算安静期"
        );
        let second = a.take().unwrap();
        assert_eq!(second.seq, 2);
        assert_eq!(second.paths, vec![PathBuf::from("/r/b")]);
    }

    #[test]
    fn a_refs_event_is_not_lost_among_a_flood_of_workdir_events() {
        let base = Instant::now();
        let opts = WorkspaceWatchOptions::new()
            .limits(8, 1024)
            .max_batch_age(Duration::from_millis(500));
        let mut a = agg(opts);
        for i in 0..1_000 {
            a.push(
                workdir_change(&format!("/r/build/{i}.o")),
                base + Duration::from_millis(i),
            );
        }
        a.push(
            RawChange {
                kinds: WorkspaceChangeKind::REFS,
                paths: vec![PathBuf::from("/r/.git/refs/heads/main")],
                degraded: Vec::new(),
            },
            base + Duration::from_millis(1_001),
        );
        let change = a.take().unwrap();
        assert!(
            change.kinds.contains(WorkspaceChangeKind::REFS),
            "大量工作区事件不能吞掉 refs 原因位: {change:?}"
        );
        assert!(change.kinds.contains(WorkspaceChangeKind::WORKDIR));
        assert!(
            change.kinds.contains(WorkspaceChangeKind::FULL_RESCAN),
            "超过路径上限应升级为完整重扫"
        );
        assert!(change.paths.is_empty());
    }

    #[test]
    fn a_persistent_stream_does_not_grow_the_pending_path_set_without_bound() {
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new().limits(32, 2048));
        for i in 0..10_000 {
            a.push(
                workdir_change(&format!("/r/dir/{i}/file.txt")),
                base + Duration::from_millis(i),
            );
            let _ = a.deadline();
        }
        assert!(
            a.paths.len() <= 32,
            "待投递路径数必须有界,实际 {}",
            a.paths.len()
        );
        assert!(a.kinds.contains(WorkspaceChangeKind::FULL_RESCAN));
    }

    #[test]
    fn an_error_becomes_a_full_rescan_with_degraded_info() {
        let base = Instant::now();
        let mut a = agg(WorkspaceWatchOptions::new());
        a.push(RawChange::global_error("backend down".into()), base);
        let change = a.take().unwrap();
        assert!(change.kinds.contains(WorkspaceChangeKind::FULL_RESCAN));
        assert_eq!(change.degraded.len(), 1);
        assert_eq!(change.degraded[0].path, None);
        assert_eq!(change.degraded[0].message, "backend down");
    }

    // ---- 真实文件系统 ----

    /// macOS FSEvents 在部分沙盒/CI 环境不向临时目录投递事件。先探测一次;不支持时
    /// 显式跳过真实 FS 断言(确定性分类/状态机测试仍然覆盖算法)。
    fn fs_events_supported() -> bool {
        static SUPPORTED: OnceLock<bool> = OnceLock::new();
        *SUPPORTED.get_or_init(|| {
            let dir = match tempfile::tempdir() {
                Ok(d) => d,
                Err(_) => return false,
            };
            let (tx, rx) = mpsc::channel();
            let mut watcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(event) = res
                        && !matches!(event.kind, notify::EventKind::Access(_))
                    {
                        let _ = tx.send(());
                    }
                }) {
                    Ok(w) => w,
                    Err(_) => return false,
                };
            if watcher
                .watch(dir.path(), notify::RecursiveMode::Recursive)
                .is_err()
            {
                return false;
            }
            if std::fs::write(dir.path().join("probe.txt"), "x").is_err() {
                return false;
            }
            rx.recv_timeout(Duration::from_secs(3)).is_ok()
        })
    }

    fn recv_until(
        rx: &Receiver<WorkspaceChange>,
        timeout: Duration,
        mut pred: impl FnMut(&WorkspaceChange) -> bool,
    ) -> Option<WorkspaceChange> {
        let deadline = Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(change) if pred(&change) => return Some(change),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        None
    }

    fn start(
        identity: &RepoIdentity,
    ) -> (
        WorkspaceWatchHandle,
        Receiver<WorkspaceChange>,
        Arc<Mutex<Vec<WorkspaceChange>>>,
    ) {
        let (tx, rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let handle = watch_workspace(
            identity,
            WorkspaceWatchOptions::new()
                .quiet_delay(Duration::from_millis(80))
                .max_batch_age(Duration::from_millis(500))
                .limits(64, 16 * 1024),
            move |change| {
                seen2.lock().unwrap().push(change.clone());
                let _ = tx.send(change);
            },
        )
        .expect("工作区监听应能启动");
        (handle, rx, seen)
    }

    #[test]
    fn a_workdir_write_is_reported_with_its_path() {
        if !fs_events_supported() {
            return;
        }
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        let identity = t.open().identity().unwrap();
        let (_handle, rx, _seen) = start(&identity);

        let file = t.path().canonicalize().unwrap().join("new.txt");
        std::fs::write(&file, "x").unwrap();
        let change = recv_until(&rx, Duration::from_secs(5), |c| {
            c.kinds.contains(WorkspaceChangeKind::WORKDIR)
        })
        .expect("应收到工作区变化");
        assert!(change.paths.contains(&file), "{change:?}");
    }

    #[test]
    fn git_add_is_reported_as_index_not_refs() {
        if !fs_events_supported() {
            return;
        }
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        let identity = t.open().identity().unwrap();
        let (_handle, rx, _seen) = start(&identity);

        t.write_untracked("b.txt", "x").stage("b.txt");
        let change = recv_until(&rx, Duration::from_secs(5), |c| {
            c.kinds.contains(WorkspaceChangeKind::INDEX)
        })
        .expect("git add 应上报 INDEX");
        assert!(
            !change.kinds.contains(WorkspaceChangeKind::HEAD),
            "git add 不应假装成 HEAD 变化: {change:?}"
        );
    }

    #[test]
    fn a_subdirectory_project_sees_an_external_empty_commit() {
        if !fs_events_supported() {
            return;
        }
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.write_untracked("proj/.keep", "");
        let identity = crate::Repo::discover_workdir(&t.path().join("proj"))
            .unwrap()
            .identity()
            .unwrap();
        let (_handle, rx, _seen) = start(&identity);

        let out = Command::new("git")
            .args(["commit", "--allow-empty", "-m", "outside project"])
            .current_dir(t.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");

        let change = recv_until(&rx, Duration::from_secs(5), |c| {
            c.kinds.contains(WorkspaceChangeKind::REFS)
        })
        .expect("子目录项目应收到仓库级 refs 变化");
        assert!(
            change
                .paths
                .iter()
                .all(|p| !p.starts_with(t.path().join("proj"))),
            "项目外路径不应被当成项目内文件: {change:?}"
        );
    }

    #[test]
    fn checkout_is_reported_as_head_change() {
        if !fs_events_supported() {
            return;
        }
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.branch("other");
        let identity = t.open().identity().unwrap();
        let (_handle, rx, _seen) = start(&identity);

        let out = Command::new("git")
            .args(["checkout", "other"])
            .current_dir(t.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");

        let change = recv_until(&rx, Duration::from_secs(5), |c| {
            c.kinds.contains(WorkspaceChangeKind::HEAD)
        })
        .expect("checkout 应上报 HEAD");
        // 只切分支不会重写 refs;REFS 不是必需位。
        assert!(
            !change.kinds.contains(WorkspaceChangeKind::FULL_RESCAN) || !change.paths.is_empty()
        );
    }

    #[test]
    fn an_explicit_shutdown_stops_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = watch_workspace(
            &fake_real_identity(dir.path()),
            WorkspaceWatchOptions::new().quiet_delay(Duration::from_millis(50)),
            move |_| {
                let _ = tx.send(());
            },
        )
        .unwrap();
        handle.shutdown().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.try_recv().is_err(), "shutdown 之后不能再有回调");
    }

    #[test]
    fn dropping_the_handle_suppresses_not_yet_admitted_batches() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count2 = count.clone();
        let handle = watch_workspace(
            &fake_real_identity(dir.path()),
            WorkspaceWatchOptions::new().quiet_delay(Duration::from_secs(5)),
            move |_| {
                count2.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        // 安静期 5s,批次尚未准入;Drop 必须抑制它。
        drop(handle);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_admitted_callback_is_allowed_to_finish_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (entered2, release2, finished2) = (entered.clone(), release.clone(), finished.clone());
        let handle = watch_workspace(
            &fake_real_identity(dir.path()),
            WorkspaceWatchOptions::new().quiet_delay(Duration::from_millis(20)),
            move |_| {
                entered2.store(true, Ordering::SeqCst);
                while !release2.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                finished2.store(true, Ordering::SeqCst);
            },
        )
        .unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !entered.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !entered.load(Ordering::SeqCst) {
            return; // 该平台不投递事件
        }
        drop(handle);
        release.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !finished.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            finished.load(Ordering::SeqCst),
            "已准入的回调允许完成(Drop 不取消运行中的 callback)"
        );
    }

    #[test]
    fn a_failed_extra_target_is_reported_as_degraded() {
        // linked worktree 的私有 gitdir 与 common dir 在工作区之外,是额外监听目标。
        // 临时把 common dir(连同其中的私有 gitdir)改名,制造启动失败;工作区根仍可用。
        let main = TempRepo::new();
        main.commit_file("a.txt", "1\n", "init");
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().canonicalize().unwrap().join("wt");
        let out = Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&wt)
            .current_dir(main.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");

        let identity = crate::Repo::discover_workdir(&wt)
            .unwrap()
            .identity()
            .unwrap();
        let common = identity.common_dir().to_path_buf();
        let parked = common.with_file_name("git.common-bytegit-degraded-test");
        std::fs::rename(&common, &parked).unwrap();

        let handle = watch_workspace(&identity, WorkspaceWatchOptions::new(), |_| {});
        let result = handle.map(|h| h.degraded().to_vec());
        std::fs::rename(&parked, &common).unwrap();

        let degraded = result.expect("工作区监听本身不因部分目标失败而失败");
        assert!(
            degraded
                .iter()
                .any(|d| d.path.as_deref() == Some(common.as_path())),
            "common dir 启动失败应进入 degraded: {degraded:?}"
        );
    }

    #[test]
    fn a_bare_identity_cannot_be_watched() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(dir.path()).unwrap();
        let identity = crate::Repo::discover(dir.path())
            .unwrap()
            .identity()
            .unwrap();
        let err = watch_workspace(&identity, WorkspaceWatchOptions::new(), |_| {})
            .err()
            .expect("bare 仓库不能建立工作区监听");
        assert_eq!(err.kind(), GitErrorKind::NotARepo);
    }

    fn init_repo(path: &Path) {
        let repo = git2::Repository::init(path).unwrap();
        repo.set_head("refs/heads/main").unwrap();
    }

    fn fake_real_identity(path: &Path) -> RepoIdentity {
        let path = path.canonicalize().unwrap();
        RepoIdentity::for_test(Some(path.clone()), path.join(".git"), path.join(".git"))
    }
}

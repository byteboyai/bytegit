//! 提交历史与单个提交的改动。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{BlobId, ChangeKind, CommitId, GitError, GitErrorKind, Repo};

/// `Repo::log` 的参数。`max_count` 必填,其余用方法设置;结构体 `non_exhaustive`,
/// 以后加字段(如 `since`)不破坏调用方。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LogOptions {
    pub max_count: usize,
    /// 只列出改动过这个路径的提交。按 git pathspec 解释(与 `git log -- <path>` 相同:
    /// `*`、`[` 等有通配含义;没有 `--follow` 的重命名跟踪)。
    pub path: Option<PathBuf>,
}

impl LogOptions {
    pub fn new(max_count: usize) -> Self {
        Self {
            max_count,
            path: None,
        }
    }

    pub fn path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    pub id: CommitId,
    pub parents: Vec<CommitId>,
    pub author: Signature,
    /// 作者时间。早于 1970 的时间戳按 `UNIX_EPOCH` 之前表示。
    pub time: SystemTime,
    /// 提交说明首行。
    pub summary: String,
    pub message: String,
}

/// 一个提交里的一个改动文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// 新路径;删除时为旧路径。
    pub path: PathBuf,
    /// 改名/复制前的路径,只有与 `path` 不同时才有。`commit_files` 不做改名检测,
    /// 所以目前总是 `None`。
    pub old_path: Option<PathBuf>,
    pub kind: ChangeKind,
    /// 旧版本 blob(新增文件为 `None`)。
    pub old_blob: Option<BlobId>,
    /// 新版本 blob(删除文件为 `None`)。
    pub new_blob: Option<BlobId>,
}

/// 一段 unified patch 文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub text: String,
    /// 是否因超过上限而被截断(`text` 里不含任何截断提示,提示文案归调用方)。
    pub truncated: bool,
}

fn system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

fn path_of(delta: &git2::DiffDelta<'_>) -> Option<PathBuf> {
    delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())
        .map(Path::to_path_buf)
}

impl Repo {
    /// 从 HEAD 起按提交时间倒序列出提交。带 `path` 时只保留改动过该路径的提交
    /// (合并提交相对第一父判断,根提交相对空树)。HEAD 未诞生(没有提交)返回
    /// `GitErrorKind::NoCommits`。
    pub fn log(&self, opts: LogOptions) -> Result<Vec<CommitSummary>, GitError> {
        let repo = self.raw();
        let mut revwalk = repo.revwalk()?;
        if let Err(e) = revwalk.push_head() {
            // HEAD 指向未诞生的分支(还没有提交):libgit2 报的是"引用不存在",换成明确的分类。
            return Err(match repo.head() {
                Err(h) if h.code() == git2::ErrorCode::UnbornBranch => {
                    GitError::new(GitErrorKind::NoCommits, "仓库还没有任何提交")
                }
                _ => e.into(),
            });
        }
        revwalk.set_sorting(git2::Sort::TIME)?;
        let pathspec = opts.path.as_ref().map(|p| p.to_string_lossy().into_owned());

        let mut out = Vec::new();
        for oid in revwalk {
            if out.len() >= opts.max_count {
                break;
            }
            let commit = repo.find_commit(oid?)?;
            if let Some(spec) = &pathspec {
                let new_tree = commit.tree()?;
                let old_tree = match commit.parent(0) {
                    Ok(parent) => Some(parent.tree()?),
                    Err(_) => None,
                };
                let mut diff_opts = git2::DiffOptions::new();
                diff_opts.pathspec(spec);
                let diff = repo.diff_tree_to_tree(
                    old_tree.as_ref(),
                    Some(&new_tree),
                    Some(&mut diff_opts),
                )?;
                if diff.deltas().next().is_none() {
                    continue;
                }
            }
            let author = commit.author();
            out.push(CommitSummary {
                id: CommitId::from_oid(commit.id()),
                parents: commit.parent_ids().map(CommitId::from_oid).collect(),
                author: Signature {
                    name: author.name().ok().map(str::to_string),
                    email: author.email().ok().map(str::to_string),
                },
                time: system_time(commit.time().seconds()),
                summary: commit.summary().ok().flatten().unwrap_or("").to_string(),
                message: commit.message().ok().unwrap_or("").to_string(),
            });
        }
        Ok(out)
    }

    /// `path` 的"上一版本":最近一次改动它的提交**之前**的那个版本;只有一次提交
    /// 时回落到那唯一一次;没有任何提交碰过它(未跟踪)返回 `None`。
    pub fn previous_version(&self, path: &Path) -> Result<Option<CommitId>, GitError> {
        let log = self.log(LogOptions::new(2).path(path))?;
        Ok(log.get(1).or_else(|| log.first()).map(|c| c.id))
    }

    /// 一个提交改动了哪些文件。合并提交相对第一父,根提交相对空树(全部为新增)。
    /// 不做重命名/复制检测。
    pub fn commit_files(&self, commit: CommitId) -> Result<Vec<FileChange>, GitError> {
        let repo = self.raw();
        let commit = repo.find_commit(commit.oid())?;
        let new_tree = commit.tree()?;
        let old_tree = match commit.parent(0) {
            Ok(parent) => Some(parent.tree()?),
            Err(_) => None,
        };
        let diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)?;
        let mut files = Vec::new();
        for delta in diff.deltas() {
            let Some(kind) = ChangeKind::from_delta(delta.status()) else {
                continue;
            };
            let Some(path) = path_of(&delta) else {
                continue;
            };
            let old_path = delta
                .old_file()
                .path()
                .filter(|old| *old != path.as_path())
                .map(Path::to_path_buf);
            let blob =
                |f: git2::DiffFile<'_>| (!f.id().is_zero()).then(|| BlobId::from_oid(f.id()));
            files.push(FileChange {
                path,
                old_path,
                kind,
                old_blob: blob(delta.old_file()),
                new_blob: blob(delta.new_file()),
            });
        }
        Ok(files)
    }

    /// `commit` 的树与**当前工作区**里 `path` 的 unified patch。直接读磁盘上的实时内容
    /// (含未提交改动)。两边相同时 `text` 为空。
    ///
    /// 截断规则:每追加一行之前检查已累积的字节数,达到 `max_bytes` 就停止并标记
    /// `truncated`,所以 `text` 可能略超过 `max_bytes`(最多多出最后一行)。
    pub fn workdir_patch(
        &self,
        commit: CommitId,
        path: &Path,
        max_bytes: usize,
    ) -> Result<Patch, GitError> {
        let repo = self.raw();
        let commit = repo.find_commit(commit.oid())?;
        let tree = commit.tree()?;
        let mut opts = git2::DiffOptions::new();
        opts.pathspec(path.to_string_lossy().into_owned());
        let diff = repo.diff_tree_to_workdir(Some(&tree), Some(&mut opts))?;

        let mut text = String::new();
        let mut truncated = false;
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            if truncated {
                return true;
            }
            if text.len() >= max_bytes {
                truncated = true;
                return true;
            }
            if matches!(line.origin(), '+' | '-' | ' ') {
                text.push(line.origin());
            }
            text.push_str(&String::from_utf8_lossy(line.content()));
            true
        })?;
        Ok(Patch { text, truncated })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    /// c1: 新建 a.txt / c2: 新建无关的 b.txt / c3: 改 a.txt。
    fn three_commits() -> (TempRepo, [CommitId; 3]) {
        let t = TempRepo::new();
        let c1 = t.commit_file("a.txt", "one\n", "c1: add a.txt\n\nbody line");
        let c2 = t.commit_file("b.txt", "unrelated\n", "c2: add b.txt");
        let c3 = t.commit_file("a.txt", "two\n", "c3: change a.txt");
        (t, [c1, c2, c3])
    }

    fn ids(log: &[CommitSummary]) -> Vec<CommitId> {
        log.iter().map(|c| c.id).collect()
    }

    #[test]
    fn log_lists_newest_first_with_all_fields() {
        let (t, [c1, c2, c3]) = three_commits();
        let log = t.open().log(LogOptions::new(10)).unwrap();
        assert_eq!(ids(&log), vec![c3, c2, c1]);
        assert_eq!(log[2].summary, "c1: add a.txt");
        assert_eq!(log[2].message, "c1: add a.txt\n\nbody line");
        assert_eq!(log[2].author.name.as_deref(), Some("Test"));
        assert_eq!(log[2].author.email.as_deref(), Some("test@example.com"));
        assert!(log[2].parents.is_empty(), "根提交没有父提交");
        assert_eq!(log[1].parents, vec![c1]);
        assert!(log[0].time > log[1].time && log[1].time > log[2].time);
    }

    #[test]
    fn log_max_count_truncates_and_zero_is_empty() {
        let (t, [_, c2, c3]) = three_commits();
        let repo = t.open();
        assert_eq!(ids(&repo.log(LogOptions::new(2)).unwrap()), vec![c3, c2]);
        assert!(repo.log(LogOptions::new(0)).unwrap().is_empty());
    }

    #[test]
    fn log_with_path_keeps_only_commits_touching_it() {
        let (t, [c1, _, c3]) = three_commits();
        let log = t.open().log(LogOptions::new(10).path("a.txt")).unwrap();
        assert_eq!(ids(&log), vec![c3, c1]);
    }

    #[test]
    fn log_with_path_max_count_counts_matches_not_commits_walked() {
        let (t, [_, _, c3]) = three_commits();
        let log = t.open().log(LogOptions::new(1).path("a.txt")).unwrap();
        assert_eq!(ids(&log), vec![c3]);
    }

    #[test]
    fn log_with_path_includes_the_deleting_commit() {
        let t = TempRepo::new();
        let c1 = t.commit_file("a.txt", "one\n", "add");
        t.stage_remove("a.txt");
        let c2 = t.commit_staged("delete");
        let log = t.open().log(LogOptions::new(10).path("a.txt")).unwrap();
        assert_eq!(ids(&log), vec![c2, c1]);
    }

    #[test]
    fn log_path_does_not_match_a_sibling_with_the_same_prefix() {
        let t = TempRepo::new();
        t.commit_file("a.txt.bak", "x\n", "bak");
        let c2 = t.commit_file("a.txt", "x\n", "real");
        let log = t.open().log(LogOptions::new(10).path("a.txt")).unwrap();
        assert_eq!(ids(&log), vec![c2]);
    }

    #[test]
    fn log_path_under_a_directory_matches_by_full_relative_path() {
        let t = TempRepo::new();
        let c1 = t.commit_file("src/a.rs", "x\n", "src");
        t.commit_file("other/a.rs", "x\n", "other");
        let log = t.open().log(LogOptions::new(10).path("src/a.rs")).unwrap();
        assert_eq!(ids(&log), vec![c1]);
    }

    /// 已知怪癖(从迁移前的 `file_history::build` 原样带过来):`path` 按 pathspec 解释,
    /// 文件名里的 `[`、`*` 是通配符,所以 `a[1].txt` 的历史里会混进 `a1.txt` 的提交。
    /// 要改成字面匹配是单独的行为变更(见 P2 计划"待决事项" D3),改的时候同时改这条测试。
    #[test]
    fn log_path_is_a_pathspec_so_glob_characters_match_other_files_known_quirk() {
        let t = TempRepo::new();
        t.commit_file("a[1].txt", "x\n", "literal bracket");
        t.commit_file("a1.txt", "x\n", "a1");
        let summaries = |p: &str| -> Vec<String> {
            t.open()
                .log(LogOptions::new(10).path(p))
                .unwrap()
                .into_iter()
                .map(|c| c.summary)
                .collect()
        };
        assert_eq!(summaries("a1.txt"), vec!["a1"]);
        assert_eq!(summaries("a[1].txt"), vec!["a1", "literal bracket"]);
    }

    #[test]
    fn log_of_an_empty_repo_is_a_no_commits_error() {
        let t = TempRepo::new();
        let err = t.open().log(LogOptions::new(10)).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NoCommits);
    }

    #[test]
    fn log_with_path_judges_a_merge_commit_against_its_first_parent_only() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "a\n", "base");
        t.branch("other").checkout("other");
        let on_other = t.commit_file("b.txt", "b\n", "other adds b");
        t.checkout("main");
        t.commit_file("c.txt", "c\n", "main adds c");
        let merge = t.merge_commit("other", "merge other");
        let repo = t.open();

        let all = repo.log(LogOptions::new(10)).unwrap();
        let merge_summary = all.iter().find(|c| c.id == merge).unwrap();
        assert_eq!(merge_summary.parents.len(), 2);

        // 合并提交相对第一父(main 一侧)新增了 b.txt,所以它和 other 上的提交都算"碰过 b.txt"。
        let for_b = repo.log(LogOptions::new(10).path("b.txt")).unwrap();
        let got = ids(&for_b);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&merge) && got.contains(&on_other));
    }

    #[test]
    fn previous_version_is_the_commit_before_the_latest_change() {
        let (t, [c1, _, _]) = three_commits();
        assert_eq!(
            t.open().previous_version(Path::new("a.txt")).unwrap(),
            Some(c1)
        );
    }

    #[test]
    fn previous_version_falls_back_to_the_only_commit() {
        let t = TempRepo::new();
        let c1 = t.commit_file("a.txt", "one\n", "add");
        t.commit_file("b.txt", "x\n", "other");
        assert_eq!(
            t.open().previous_version(Path::new("a.txt")).unwrap(),
            Some(c1)
        );
    }

    #[test]
    fn previous_version_of_an_untracked_path_is_none() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "add");
        assert_eq!(
            t.open().previous_version(Path::new("nope.txt")).unwrap(),
            None
        );
    }

    #[test]
    fn commit_files_of_a_root_commit_are_all_added_without_old_blobs() {
        let t = TempRepo::new();
        t.write_untracked("a.txt", "one\n").stage("a.txt");
        t.write_untracked("sub/b.txt", "two\n").stage("sub/b.txt");
        let root = t.commit_staged("root");
        let mut files = t.open().commit_files(root).unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(files.len(), 2);
        for f in &files {
            assert_eq!(f.kind, ChangeKind::Added);
            assert_eq!(f.old_blob, None);
            assert!(f.new_blob.is_some());
            assert_eq!(f.old_path, None);
        }
        assert_eq!(files[1].path, PathBuf::from("sub/b.txt"));
    }

    #[test]
    fn commit_files_reports_modified_and_deleted_with_blob_ids() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "a");
        t.commit_file("b.txt", "two\n", "b");
        t.write_untracked("a.txt", "one\nmodified\n").stage("a.txt");
        t.stage_remove("b.txt");
        let c = t.commit_staged("modify and delete");
        let files = t.open().commit_files(c).unwrap();
        let a = files.iter().find(|f| f.path == Path::new("a.txt")).unwrap();
        let b = files.iter().find(|f| f.path == Path::new("b.txt")).unwrap();
        assert_eq!(a.kind, ChangeKind::Modified);
        assert!(a.old_blob.is_some() && a.new_blob.is_some() && a.old_blob != a.new_blob);
        assert_eq!(b.kind, ChangeKind::Deleted);
        assert!(b.old_blob.is_some());
        assert_eq!(b.new_blob, None);
    }

    #[test]
    fn commit_files_does_not_detect_renames() {
        let t = TempRepo::new();
        t.commit_file("old.txt", "same content\nline 2\nline 3\n", "add");
        t.stage_remove("old.txt");
        t.write_untracked("new.txt", "same content\nline 2\nline 3\n")
            .stage("new.txt");
        let c = t.commit_staged("rename");
        let files = t.open().commit_files(c).unwrap();
        let kind_of = |name: &str| {
            files
                .iter()
                .find(|f| f.path == Path::new(name))
                .map(|f| f.kind)
        };
        assert_eq!(files.len(), 2);
        assert_eq!(kind_of("new.txt"), Some(ChangeKind::Added));
        assert_eq!(kind_of("old.txt"), Some(ChangeKind::Deleted));
        assert!(files.iter().all(|f| f.old_path.is_none()));
    }

    #[test]
    fn commit_files_of_a_merge_commit_are_relative_to_the_first_parent() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "a\n", "base");
        t.branch("other").checkout("other");
        t.commit_file("b.txt", "b\n", "other adds b");
        t.checkout("main");
        t.commit_file("c.txt", "c\n", "main adds c");
        let merge = t.merge_commit("other", "merge");
        let files = t.open().commit_files(merge).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, PathBuf::from("b.txt"));
        assert_eq!(files[0].kind, ChangeKind::Added);
    }

    #[test]
    fn commit_files_keeps_non_ascii_paths_verbatim() {
        let t = TempRepo::new();
        let c = t.commit_file("文档/说明.md", "x\n", "cn");
        let files = t.open().commit_files(c).unwrap();
        assert_eq!(files[0].path, PathBuf::from("文档/说明.md"));
    }

    #[test]
    fn workdir_patch_is_empty_when_disk_matches_the_commit() {
        let (t, [c1, _, _]) = three_commits();
        t.write_untracked("a.txt", "one\n");
        let p = t
            .open()
            .workdir_patch(c1, Path::new("a.txt"), 20_000)
            .unwrap();
        assert_eq!(
            p,
            Patch {
                text: String::new(),
                truncated: false
            }
        );
    }

    #[test]
    fn workdir_patch_shows_prefixed_lines_when_disk_differs() {
        let (t, [c1, _, _]) = three_commits();
        let p = t
            .open()
            .workdir_patch(c1, Path::new("a.txt"), 20_000)
            .unwrap();
        assert!(p.text.contains("-one\n"), "{}", p.text);
        assert!(p.text.contains("+two\n"), "{}", p.text);
        assert!(!p.truncated);
    }

    #[test]
    fn workdir_patch_truncates_without_adding_a_notice() {
        let t = TempRepo::new();
        let many: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        let c1 = t.commit_file("big.txt", &many, "big");
        let other: String = (0..2000).map(|i| format!("changed {i}\n")).collect();
        t.write_untracked("big.txt", &other);
        let p = t
            .open()
            .workdir_patch(c1, Path::new("big.txt"), 1_000)
            .unwrap();
        assert!(p.truncated);
        assert!(p.text.len() >= 1_000, "达到上限才停:{}", p.text.len());
        assert!(
            p.text.len() < 1_000 + 200,
            "最多多出最后一行:{}",
            p.text.len()
        );
        assert!(!p.text.contains("截断"));
        let full = t
            .open()
            .workdir_patch(c1, Path::new("big.txt"), usize::MAX)
            .unwrap();
        assert!(!full.truncated);
        assert!(full.text.len() > p.text.len());
    }

    #[test]
    fn workdir_patch_of_a_file_deleted_on_disk_shows_the_removal() {
        let (t, [c1, _, _]) = three_commits();
        t.delete_file("a.txt");
        let p = t
            .open()
            .workdir_patch(c1, Path::new("a.txt"), 20_000)
            .unwrap();
        assert!(p.text.contains("-one\n"), "{}", p.text);
    }

    #[test]
    fn system_time_handles_pre_epoch_timestamps() {
        assert_eq!(system_time(0), SystemTime::UNIX_EPOCH);
        assert_eq!(
            system_time(-60),
            SystemTime::UNIX_EPOCH - Duration::from_secs(60)
        );
    }
}

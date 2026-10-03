//! 工作区状态。

use std::path::PathBuf;

use crate::{ChangeKind, GitError, Repo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusOptions {
    /// 包含未跟踪文件,且递归进未跟踪目录、逐个列出其中的文件。
    pub include_untracked: bool,
    /// 包含被 `.gitignore` 忽略的条目(被忽略的目录只作为一个条目出现,路径带尾部 `/`)。
    pub include_ignored: bool,
    /// 做重命名检测(HEAD→暂存区、暂存区→工作区):重命名只报新路径,
    /// 否则表现为"旧路径删除 + 新路径新增"。与 `git status --porcelain` 的默认行为一致。
    pub detect_renames: bool,
}

impl Default for StatusOptions {
    /// 含未跟踪、不含被忽略、不做重命名检测。
    fn default() -> Self {
        Self {
            include_untracked: true,
            include_ignored: false,
            detect_renames: false,
        }
    }
}

/// 一个路径的状态。`index` 是相对 HEAD 的暂存改动,`worktree` 是相对暂存区的工作区改动,
/// 两边可同时有值(部分暂存后又改)。未跟踪的新文件表现为 `worktree == Some(Added)` 且 `index == None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileState {
    pub index: Option<ChangeKind>,
    pub worktree: Option<ChangeKind>,
    pub ignored: bool,
    /// 合并冲突中。冲突文件的 `index`/`worktree` 可能都是 `None`。
    pub conflicted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// 相对仓库根。
    pub path: PathBuf,
    pub state: FileState,
}

fn index_kind(s: git2::Status) -> Option<ChangeKind> {
    use git2::Status as S;
    if s.contains(S::INDEX_NEW) {
        Some(ChangeKind::Added)
    } else if s.contains(S::INDEX_DELETED) {
        Some(ChangeKind::Deleted)
    } else if s.contains(S::INDEX_RENAMED) {
        Some(ChangeKind::Renamed)
    } else if s.contains(S::INDEX_TYPECHANGE) {
        Some(ChangeKind::TypeChange)
    } else if s.contains(S::INDEX_MODIFIED) {
        Some(ChangeKind::Modified)
    } else {
        None
    }
}

fn worktree_kind(s: git2::Status) -> Option<ChangeKind> {
    use git2::Status as S;
    if s.contains(S::WT_NEW) {
        Some(ChangeKind::Added)
    } else if s.contains(S::WT_DELETED) {
        Some(ChangeKind::Deleted)
    } else if s.contains(S::WT_RENAMED) {
        Some(ChangeKind::Renamed)
    } else if s.contains(S::WT_TYPECHANGE) {
        Some(ChangeKind::TypeChange)
    } else if s.contains(S::WT_MODIFIED) {
        Some(ChangeKind::Modified)
    } else {
        None
    }
}

/// 条目的路径。重命名时 libgit2 的 `entry.path()` 给的是**旧**路径,而 `git status --porcelain`
/// 与调用方想要的是新路径,所以重命名要从对应的 diff delta 里取 `new_file`。
/// 路径不是合法 UTF-8 的条目返回 `None`(被跳过)。
fn entry_path(entry: &git2::StatusEntry<'_>, s: git2::Status) -> Option<PathBuf> {
    let renamed = if s.contains(git2::Status::INDEX_RENAMED) {
        entry.head_to_index()
    } else if s.contains(git2::Status::WT_RENAMED) {
        entry.index_to_workdir()
    } else {
        None
    };
    let rel = match renamed.and_then(|d| d.new_file().path().map(|p| p.to_path_buf())) {
        Some(new_path) => new_path.to_str().map(str::to_string)?,
        None => entry.path().ok()?.to_string(),
    };
    (!rel.is_empty()).then(|| PathBuf::from(rel))
}

fn git2_options(opts: StatusOptions) -> git2::StatusOptions {
    let mut o = git2::StatusOptions::new();
    o.include_untracked(opts.include_untracked)
        .recurse_untracked_dirs(opts.include_untracked)
        .include_ignored(opts.include_ignored);
    if opts.detect_renames {
        o.renames_head_to_index(true).renames_index_to_workdir(true);
    }
    o
}

impl Repo {
    /// 有改动的路径及其状态。路径不是合法 UTF-8 的条目被跳过。bare 仓库返回错误。
    pub fn status(&self, opts: StatusOptions) -> Result<Vec<StatusEntry>, GitError> {
        let mut o = git2_options(opts);
        let statuses = self.raw().statuses(Some(&mut o))?;
        let mut out = Vec::new();
        for entry in statuses.iter() {
            let s = entry.status();
            if s.is_empty() {
                continue;
            }
            let Some(path) = entry_path(&entry, s) else {
                continue;
            };
            out.push(StatusEntry {
                path,
                state: FileState {
                    index: index_kind(s),
                    worktree: worktree_kind(s),
                    ignored: s.contains(git2::Status::IGNORED),
                    conflicted: s.contains(git2::Status::CONFLICTED),
                },
            });
        }
        Ok(out)
    }

    /// 是否有任何改动。与 `status(opts)` 是否非空一致,但不会因为路径不是 UTF-8 而漏报。
    pub fn is_dirty(&self, opts: StatusOptions) -> Result<bool, GitError> {
        let mut o = git2_options(opts);
        Ok(!self.raw().statuses(Some(&mut o))?.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;
    use std::path::Path;

    fn entry<'a>(v: &'a [StatusEntry], p: &str) -> &'a StatusEntry {
        v.iter()
            .find(|e| e.path == Path::new(p))
            .unwrap_or_else(|| panic!("没有 {p} 的状态: {v:?}"))
    }

    fn all() -> StatusOptions {
        StatusOptions {
            include_untracked: true,
            include_ignored: true,
            detect_renames: false,
        }
    }

    #[test]
    fn clean_repo_has_no_entries_and_is_not_dirty() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        let repo = t.open();
        assert!(repo.status(StatusOptions::default()).unwrap().is_empty());
        assert!(!repo.is_dirty(StatusOptions::default()).unwrap());
    }

    #[test]
    fn modified_staged_and_untracked_are_distinguished() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.commit_file("b.txt", "one\n", "c2");
        // 只暂存
        t.write_untracked("a.txt", "staged\n").stage("a.txt");
        // 未暂存修改
        t.write_untracked("b.txt", "worktree\n");
        // 未跟踪
        t.write_untracked("new.txt", "n\n");
        let v = t.open().status(StatusOptions::default()).unwrap();
        let a = entry(&v, "a.txt").state;
        assert_eq!((a.index, a.worktree), (Some(ChangeKind::Modified), None));
        let b = entry(&v, "b.txt").state;
        assert_eq!((b.index, b.worktree), (None, Some(ChangeKind::Modified)));
        let n = entry(&v, "new.txt").state;
        assert_eq!((n.index, n.worktree), (None, Some(ChangeKind::Added)));
    }

    #[test]
    fn staged_then_modified_again_has_both_sides() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.write_untracked("a.txt", "staged\n").stage("a.txt");
        t.write_untracked("a.txt", "staged\nplus more\n");
        let v = t.open().status(StatusOptions::default()).unwrap();
        let a = entry(&v, "a.txt").state;
        assert_eq!(a.index, Some(ChangeKind::Modified));
        assert_eq!(a.worktree, Some(ChangeKind::Modified));
    }

    #[test]
    fn staged_new_file_then_edited_is_added_in_index_modified_in_worktree() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.write_untracked("n.txt", "1\n").stage("n.txt");
        t.write_untracked("n.txt", "2\n");
        let v = t.open().status(StatusOptions::default()).unwrap();
        let n = entry(&v, "n.txt").state;
        assert_eq!(n.index, Some(ChangeKind::Added));
        assert_eq!(n.worktree, Some(ChangeKind::Modified));
    }

    #[test]
    fn deleted_files_staged_and_unstaged() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.commit_file("b.txt", "one\n", "c2");
        t.stage_remove("a.txt");
        t.delete_file("b.txt");
        let v = t.open().status(StatusOptions::default()).unwrap();
        assert_eq!(entry(&v, "a.txt").state.index, Some(ChangeKind::Deleted));
        assert_eq!(entry(&v, "a.txt").state.worktree, None);
        assert_eq!(entry(&v, "b.txt").state.index, None);
        assert_eq!(entry(&v, "b.txt").state.worktree, Some(ChangeKind::Deleted));
    }

    #[test]
    fn untracked_files_in_new_directories_are_listed_individually() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.write_untracked("dir/sub/x.txt", "x")
            .write_untracked("dir/y.txt", "y");
        let v = t.open().status(StatusOptions::default()).unwrap();
        entry(&v, "dir/sub/x.txt");
        entry(&v, "dir/y.txt");
    }

    #[test]
    fn untracked_can_be_excluded() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.write_untracked("new.txt", "n");
        let opts = StatusOptions {
            include_untracked: false,
            ..StatusOptions::default()
        };
        let repo = t.open();
        assert!(repo.status(opts).unwrap().is_empty());
        assert!(!repo.is_dirty(opts).unwrap());
        assert!(repo.is_dirty(StatusOptions::default()).unwrap());
    }

    #[test]
    fn ignored_entries_only_appear_when_requested_and_never_make_the_repo_dirty() {
        let t = TempRepo::new();
        t.commit_file(".gitignore", "*.log\ntarget/\n", "ignore");
        t.write_untracked("debug.log", "x")
            .write_untracked("target/out.bin", "x");
        let repo = t.open();
        assert!(repo.status(StatusOptions::default()).unwrap().is_empty());
        assert!(!repo.is_dirty(StatusOptions::default()).unwrap());
        let v = repo.status(all()).unwrap();
        assert!(entry(&v, "debug.log").state.ignored);
        // 被忽略的目录只作为一个条目出现,路径带尾部 `/`
        assert!(entry(&v, "target/").state.ignored);
        assert_eq!(entry(&v, "target/").state.index, None);
    }

    #[test]
    fn rename_is_one_entry_with_detection_and_delete_plus_add_without() {
        let t = TempRepo::new();
        let body = "line one\nline two\nline three\nline four\nline five\n";
        t.commit_file("old.txt", body, "c1");
        t.stage_remove("old.txt");
        t.write_untracked("new.txt", body).stage("new.txt");
        let repo = t.open();

        let without = repo.status(StatusOptions::default()).unwrap();
        assert_eq!(
            entry(&without, "old.txt").state.index,
            Some(ChangeKind::Deleted)
        );
        assert_eq!(
            entry(&without, "new.txt").state.index,
            Some(ChangeKind::Added)
        );

        let with = repo
            .status(StatusOptions {
                detect_renames: true,
                ..StatusOptions::default()
            })
            .unwrap();
        assert_eq!(with.len(), 1, "{with:?}");
        assert_eq!(
            entry(&with, "new.txt").state.index,
            Some(ChangeKind::Renamed)
        );
    }

    #[test]
    fn conflicted_files_are_flagged_and_count_as_dirty() {
        let t = TempRepo::new();
        t.make_conflict("c.txt");
        let repo = t.open();
        let v = repo.status(StatusOptions::default()).unwrap();
        let c = entry(&v, "c.txt").state;
        assert!(c.conflicted);
        assert_eq!((c.index, c.worktree), (None, None));
        assert!(repo.is_dirty(StatusOptions::default()).unwrap());
    }

    #[test]
    fn non_ascii_paths_are_reported_as_real_utf8() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "one\n", "c1");
        t.write_untracked("文档/说明.md", "x");
        let v = t.open().status(StatusOptions::default()).unwrap();
        entry(&v, "文档/说明.md");
    }

    #[test]
    fn status_on_a_bare_repo_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(dir.path()).unwrap();
        let repo = crate::Repo::discover(dir.path()).unwrap();
        assert!(repo.status(StatusOptions::default()).is_err());
        assert!(repo.is_dirty(StatusOptions::default()).is_err());
    }
}

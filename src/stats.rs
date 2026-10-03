//! 历史统计:HEAD 提交时间、提交计数、按日分桶、近期文件改动频率。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::history::{path_of, system_time};
use crate::{GitError, Repo};

const SECS_PER_DAY: i64 = 86_400;

fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

impl Repo {
    /// HEAD 提交的提交时间(committer time,等于 `git log -1 --format=%ct`)。
    /// 仓库还没有提交返回 `None`。
    pub fn head_commit_time(&self) -> Result<Option<SystemTime>, GitError> {
        let head = match self.raw().head() {
            Ok(head) => head,
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let commit = head.peel_to_commit()?;
        Ok(Some(system_time(commit.time().seconds())))
    }

    /// 从 HEAD 可达的提交总数(当前分支口径,合并进来的提交只算一次)。
    /// HEAD 未诞生返回 `GitErrorKind::NoCommits`。
    pub fn commit_count(&self) -> Result<u64, GitError> {
        let mut count = 0u64;
        for oid in self.head_walk(git2::Sort::NONE)? {
            oid?;
            count += 1;
        }
        Ok(count)
    }

    /// 从 HEAD 可达的提交按**提交时间**(committer time)的 UTC 日分桶计数:
    /// 日索引 = `提交时间秒 / 86_400`(整数除法,与迁移前 `usage` 同口径)。只含"有提交的那些天"。任何一个提交读取失败整体报错;
    /// HEAD 未诞生返回 `GitErrorKind::NoCommits`。
    pub fn commit_count_by_day(&self) -> Result<BTreeMap<i64, u64>, GitError> {
        let repo = self.raw();
        let mut by_day: BTreeMap<i64, u64> = BTreeMap::new();
        for oid in self.head_walk(git2::Sort::NONE)? {
            let commit = repo.find_commit(oid?)?;
            *by_day
                .entry(commit.time().seconds() / SECS_PER_DAY)
                .or_insert(0) += 1;
        }
        Ok(by_day)
    }

    /// 提交时间不早于 `since` 的提交里,每个文件被改动的提交数(路径相对**仓库根**)。
    ///
    /// 口径与 `git log --since=<since> --name-only` 一致:
    /// - 合并提交不计(`git log --name-only` 不列合并提交的文件);根提交相对空树;
    /// - 做重命名检测(libgit2 默认参数,与 git 默认一致),重命名只计新路径;删除计旧路径;
    /// - 按提交时间倒序遍历,遇到第一个早于 `since` 的提交即停止:逐提交做 diff 的只有近期那些。
    ///   (libgit2 对"按时间排序"的遍历会先解析整段可达历史再产出,所以排序是全局的、
    ///   提前停止不会漏掉因时钟偏差而时间戳偏新的更早提交;代价是解析成本与历史长度成正比,
    ///   与 `git log --since` 借助提交图提前结束不同,超大仓库上会更慢。)
    ///
    /// HEAD 未诞生返回 `GitErrorKind::NoCommits`。
    pub fn churn(&self, since: SystemTime) -> Result<HashMap<PathBuf, u32>, GitError> {
        let repo = self.raw();
        let since_secs = unix_secs(since);
        let mut counts: HashMap<PathBuf, u32> = HashMap::new();
        for oid in self.head_walk(git2::Sort::TIME)? {
            let commit = repo.find_commit(oid?)?;
            if commit.time().seconds() < since_secs {
                break;
            }
            if commit.parent_count() > 1 {
                continue;
            }
            let new_tree = commit.tree()?;
            let old_tree = match commit.parent(0) {
                Ok(parent) => Some(parent.tree()?),
                Err(_) => None,
            };
            let mut diff = repo.diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)?;
            diff.find_similar(None)?;
            for delta in diff.deltas() {
                if let Some(path) = path_of(&delta) {
                    *counts.entry(path).or_insert(0) += 1;
                }
            }
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GitErrorKind;
    use crate::testutil::TempRepo;
    use std::time::Duration;

    /// 2024-10-04 00:00:00 UTC 附近的整日起点(日索引 20000)。
    const DAY_20000: i64 = 20_000 * SECS_PER_DAY;

    fn at(secs: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    }

    fn churn_of(t: &TempRepo, since: i64) -> HashMap<PathBuf, u32> {
        t.open().churn(at(since)).unwrap()
    }

    fn count_of(m: &HashMap<PathBuf, u32>, path: &str) -> Option<u32> {
        m.get(&PathBuf::from(path)).copied()
    }

    // ---- head_commit_time ----

    #[test]
    fn head_commit_time_is_none_for_an_empty_repo() {
        assert_eq!(TempRepo::new().open().head_commit_time().unwrap(), None);
    }

    #[test]
    fn head_commit_time_is_the_head_commit_time_not_the_newest_in_history() {
        let t = TempRepo::new();
        t.commit_file_at("a.txt", "1", "newer", DAY_20000 + 5_000);
        // HEAD 提交的时间比它的父提交更早(时钟偏差):取 HEAD 的,不是历史里最大的。
        t.commit_file_at("a.txt", "2", "head", DAY_20000);
        assert_eq!(t.open().head_commit_time().unwrap(), Some(at(DAY_20000)));
    }

    #[test]
    fn head_commit_time_works_on_a_detached_head() {
        let t = TempRepo::new();
        t.commit_file_at("a.txt", "1", "one", DAY_20000);
        t.detach_head();
        assert_eq!(t.open().head_commit_time().unwrap(), Some(at(DAY_20000)));
    }

    // ---- commit_count ----

    #[test]
    fn commit_count_of_an_empty_repo_is_a_no_commits_error() {
        let err = TempRepo::new().open().commit_count().unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NoCommits);
    }

    #[test]
    fn commit_count_counts_commits_reachable_from_head() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "1", "one");
        t.commit_file("a.txt", "2", "two");
        t.commit_file("a.txt", "3", "three");
        assert_eq!(t.open().commit_count().unwrap(), 3);
    }

    #[test]
    fn commit_count_counts_a_merged_commit_once() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "a", "base");
        t.branch("other").checkout("other");
        t.commit_file("b.txt", "b", "other adds b");
        t.checkout("main");
        t.commit_file("c.txt", "c", "main adds c");
        t.merge_commit("other", "merge");
        // base + other + main + merge
        assert_eq!(t.open().commit_count().unwrap(), 4);
    }

    #[test]
    fn commit_count_only_counts_the_current_branch() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "a", "base");
        t.branch("other").checkout("other");
        t.commit_file("b.txt", "b", "only on other");
        t.checkout("main");
        assert_eq!(t.open().commit_count().unwrap(), 1);
    }

    #[test]
    fn commit_count_works_on_a_detached_head() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "1", "one");
        t.commit_file("a.txt", "2", "two");
        t.detach_head();
        assert_eq!(t.open().commit_count().unwrap(), 2);
    }

    // ---- commit_count_by_day ----

    #[test]
    fn commit_count_by_day_buckets_by_utc_commit_day() {
        let t = TempRepo::new();
        t.commit_file_at("a.txt", "1", "d0 start", DAY_20000);
        t.commit_file_at("a.txt", "2", "d0 end", DAY_20000 + SECS_PER_DAY - 1);
        t.commit_file_at("a.txt", "3", "d1", DAY_20000 + SECS_PER_DAY);
        let by_day = t.open().commit_count_by_day().unwrap();
        assert_eq!(by_day, BTreeMap::from([(20_000, 2), (20_001, 1)]));
    }

    #[test]
    fn commit_count_by_day_of_an_empty_repo_is_a_no_commits_error() {
        let err = TempRepo::new().open().commit_count_by_day().unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NoCommits);
    }

    // ---- churn ----

    #[test]
    fn churn_counts_the_commits_touching_each_file() {
        let t = TempRepo::new();
        t.commit_file_at("hot.rs", "1", "c1", DAY_20000);
        t.commit_file_at("hot.rs", "2", "c2", DAY_20000 + 10);
        t.commit_file_at("cold.rs", "x", "c3", DAY_20000 + 20);
        let churn = churn_of(&t, DAY_20000);
        assert_eq!(count_of(&churn, "hot.rs"), Some(2));
        assert_eq!(count_of(&churn, "cold.rs"), Some(1));
        assert_eq!(churn.len(), 2);
    }

    #[test]
    fn churn_includes_the_root_commit_files() {
        let t = TempRepo::new();
        t.write_untracked("a.rs", "1").stage("a.rs");
        t.write_untracked("sub/b.rs", "2").stage("sub/b.rs");
        t.commit_staged_at("root", DAY_20000);
        let churn = churn_of(&t, DAY_20000);
        assert_eq!(count_of(&churn, "a.rs"), Some(1));
        assert_eq!(count_of(&churn, "sub/b.rs"), Some(1), "路径相对仓库根");
    }

    #[test]
    fn churn_excludes_commits_older_than_since_and_includes_the_boundary() {
        let t = TempRepo::new();
        t.commit_file_at("a.rs", "1", "old", DAY_20000 - 1);
        t.commit_file_at("a.rs", "2", "on the boundary", DAY_20000);
        t.commit_file_at("a.rs", "3", "newer", DAY_20000 + 1);
        let churn = churn_of(&t, DAY_20000);
        assert_eq!(count_of(&churn, "a.rs"), Some(2));
    }

    #[test]
    fn churn_of_a_repo_whose_commits_are_all_too_old_is_empty_not_an_error() {
        let t = TempRepo::new();
        t.commit_file_at("a.rs", "1", "old", DAY_20000);
        assert!(churn_of(&t, DAY_20000 + 1).is_empty());
    }

    #[test]
    fn churn_skips_merge_commits() {
        let t = TempRepo::new();
        t.commit_file_at("a.rs", "a", "base", DAY_20000);
        t.branch("other").checkout("other");
        t.commit_file_at("b.rs", "b", "other adds b", DAY_20000 + 10);
        t.checkout("main");
        t.commit_file_at("c.rs", "c", "main adds c", DAY_20000 + 20);
        // 合并提交相对第一父会"新增 b.rs",但合并提交本身不计。
        t.merge_commit_at("other", "merge", DAY_20000 + 30);
        let churn = churn_of(&t, DAY_20000);
        assert_eq!(count_of(&churn, "a.rs"), Some(1));
        assert_eq!(count_of(&churn, "b.rs"), Some(1), "只算 other 上那一次");
        assert_eq!(count_of(&churn, "c.rs"), Some(1));
    }

    #[test]
    fn churn_counts_a_pure_rename_for_the_new_path_only() {
        let t = TempRepo::new();
        t.commit_file_at("old.rs", "same content\nline 2\nline 3\n", "add", DAY_20000);
        t.stage_remove("old.rs");
        t.write_untracked("new.rs", "same content\nline 2\nline 3\n")
            .stage("new.rs");
        t.commit_staged_at("rename", DAY_20000 + 10);
        let churn = churn_of(&t, DAY_20000);
        assert_eq!(count_of(&churn, "new.rs"), Some(1));
        assert_eq!(count_of(&churn, "old.rs"), Some(1), "只有最初添加那一次");
    }

    #[test]
    fn churn_counts_the_old_path_of_a_deletion() {
        let t = TempRepo::new();
        t.commit_file_at("gone.rs", "1", "add", DAY_20000);
        t.stage_remove("gone.rs");
        t.commit_staged_at("delete", DAY_20000 + 10);
        assert_eq!(count_of(&churn_of(&t, DAY_20000), "gone.rs"), Some(2));
    }

    #[test]
    fn churn_keeps_non_ascii_paths_verbatim() {
        let t = TempRepo::new();
        t.commit_file_at("文档/说明.md", "x", "cn", DAY_20000);
        assert_eq!(count_of(&churn_of(&t, DAY_20000), "文档/说明.md"), Some(1));
    }

    #[test]
    fn churn_of_an_empty_repo_is_a_no_commits_error() {
        let err = TempRepo::new().open().churn(at(0)).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::NoCommits);
    }

    /// 历史(从老到新):`skew.rs`(时钟偏差,时间戳反而新)→ 几个很老的提交 → `new.rs`(新)。
    /// 遍历是全局按时间排序的,所以提前停止不会漏掉"时间戳偏新"的更早提交。
    #[test]
    fn churn_counts_every_recent_commit_even_with_clock_skew() {
        let t = TempRepo::new();
        t.commit_file_at("skew.rs", "x", "skewed but recent", DAY_20000 + 100);
        for i in 0..8 {
            t.commit_file_at(&format!("old{i}.rs"), "x", "old", DAY_20000 - 1000 + i);
        }
        t.commit_file_at("new.rs", "x", "new", DAY_20000 + 5);
        let churn = churn_of(&t, DAY_20000);
        assert_eq!(count_of(&churn, "new.rs"), Some(1));
        assert_eq!(count_of(&churn, "skew.rs"), Some(1));
        assert_eq!(churn.len(), 2, "老提交里的文件不计");
    }
}

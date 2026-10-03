//! HEAD、分支与远程信息。

use crate::{CommitId, GitError, Repo};

/// HEAD 的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadInfo {
    /// 当前分支短名。detached HEAD、HEAD 指向非分支引用、HEAD 还没有诞生(空仓库)、
    /// 或分支名不是合法 UTF-8 时为 `None`。
    pub branch: Option<String>,
    /// HEAD 解析到的提交;空仓库(HEAD 未诞生)为 `None`。detached HEAD 时有值。
    pub commit: Option<CommitId>,
}

impl HeadInfo {
    /// HEAD 能解析到一个提交。detached HEAD 也算有提交。
    pub fn has_commits(&self) -> bool {
        self.commit.is_some()
    }
}

/// 一个远程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    /// fetch URL(已应用 `url.<base>.insteadOf` 改写)。
    pub url: String,
    /// 单独配置的 push URL;没有则为 `None`。
    pub push_url: Option<String>,
}

impl Repo {
    pub fn head(&self) -> Result<HeadInfo, GitError> {
        match self.raw().head() {
            Ok(head) => {
                let branch = if head.is_branch() {
                    head.shorthand().ok().map(str::to_string)
                } else {
                    None
                };
                let commit = head
                    .peel_to_commit()
                    .ok()
                    .map(|c| CommitId::from_oid(c.id()));
                Ok(HeadInfo { branch, commit })
            }
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => Ok(HeadInfo {
                branch: None,
                commit: None,
            }),
            Err(e) => Err(GitError::from(e)),
        }
    }

    /// 全部本地分支短名,按名字升序(与 `git for-each-ref refs/heads/` 的顺序一致)。
    /// 空仓库(没有任何分支引用)返回空列表;名字不是合法 UTF-8 的分支被跳过。
    pub fn local_branches(&self) -> Result<Vec<String>, GitError> {
        let mut names = Vec::new();
        for item in self.raw().branches(Some(git2::BranchType::Local))? {
            let (branch, _) = item?;
            // 0.21:`name()` 是 `Result<Option<&str>>`,名字不是 UTF-8 时为 `None`。
            if let Ok(Some(name)) = branch.name() {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    /// 全部远程,按名字升序。没有 URL 的远程被跳过。
    pub fn remotes(&self) -> Result<Vec<Remote>, GitError> {
        let raw = self.raw();
        let mut out = Vec::new();
        for name in raw.remotes()?.iter().flatten().flatten() {
            let remote = raw.find_remote(name)?;
            let Ok(url) = remote.url() else { continue };
            out.push(Remote {
                name: name.to_string(),
                url: url.to_string(),
                push_url: remote.pushurl().ok().flatten().map(str::to_string),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::TempRepo;

    #[test]
    fn head_on_a_branch_reports_name_and_commit() {
        let t = TempRepo::new();
        let id = t.commit_file("a.txt", "x", "one");
        let head = t.open().head().unwrap();
        assert_eq!(head.branch.as_deref(), Some("main"));
        assert_eq!(head.commit, Some(id));
        assert!(head.has_commits());
    }

    #[test]
    fn head_of_an_empty_repo_has_no_branch_and_no_commit() {
        let t = TempRepo::new();
        let head = t.open().head().unwrap();
        assert_eq!(head.branch, None);
        assert_eq!(head.commit, None);
        assert!(!head.has_commits());
    }

    #[test]
    fn detached_head_has_no_branch_but_still_has_a_commit() {
        let t = TempRepo::new();
        let id = t.commit_file("a.txt", "x", "one");
        t.detach_head();
        let head = t.open().head().unwrap();
        assert_eq!(head.branch, None);
        assert_eq!(head.commit, Some(id));
        assert!(head.has_commits());
    }

    #[test]
    fn local_branches_are_sorted_and_empty_repo_has_none() {
        let t = TempRepo::new();
        assert!(t.open().local_branches().unwrap().is_empty());
        t.commit_file("a.txt", "x", "one");
        t.branch("zeta").branch("alpha").branch("feature/x");
        assert_eq!(
            t.open().local_branches().unwrap(),
            vec!["alpha", "feature/x", "main", "zeta"]
        );
    }

    #[test]
    fn remotes_are_sorted_by_name_with_urls() {
        let t = TempRepo::new();
        t.add_remote("upstream", "https://example.com/y.git");
        t.add_remote("origin", "https://example.com/x.git");
        let remotes = t.open().remotes().unwrap();
        let got: Vec<(&str, &str)> = remotes
            .iter()
            .map(|r| (r.name.as_str(), r.url.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("origin", "https://example.com/x.git"),
                ("upstream", "https://example.com/y.git")
            ]
        );
        assert!(remotes.iter().all(|r| r.push_url.is_none()));
    }

    #[test]
    fn no_remotes_is_an_empty_list() {
        let t = TempRepo::new();
        assert!(t.open().remotes().unwrap().is_empty());
    }
}

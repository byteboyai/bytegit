//! 测试夹具:用 git2 构建确定性的临时仓库,不调用命令行 `git`。
//!
//! 作者、邮箱与提交时间固定(第 n 次提交时间 = 基准 + n 分钟),
//! 所以同样的操作序列在任何机器上得到同样的提交 id。

use std::cell::Cell;
use std::path::Path;

use crate::{CommitId, Repo};

const BASE_TIME: i64 = 1_700_000_000;

pub struct TempRepo {
    dir: tempfile::TempDir,
    repo: git2::Repository,
    commits: Cell<i64>,
}

impl Default for TempRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl TempRepo {
    /// 空仓库,初始分支 `main`。
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let repo = git2::Repository::init(dir.path()).expect("git init 失败");
        repo.set_head("refs/heads/main").expect("设置初始分支失败");
        // 隔离用户的全局 git 配置:libgit2 会读 ~/.gitconfig,`core.autocrlf` 等会改变
        // blob 内容与提交 id,让"同样操作得到同样 id"在不同机器上失效。仓库级配置优先于全局。
        let empty_attributes = repo.path().join("bytegit_empty_attributes");
        std::fs::write(&empty_attributes, "").expect("写空 attributes 失败");
        let mut cfg = repo.config().expect("读取仓库配置失败");
        cfg.set_str("core.autocrlf", "false").expect("配置失败");
        cfg.set_str("core.eol", "lf").expect("配置失败");
        cfg.set_str("core.safecrlf", "false").expect("配置失败");
        cfg.set_str(
            "core.attributesFile",
            empty_attributes.to_str().expect("路径不是 UTF-8"),
        )
        .expect("配置失败");
        Self {
            dir,
            repo,
            commits: Cell::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn raw_repo(&self) -> &git2::Repository {
        &self.repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// 以 bytegit 的 `Repo` 打开(每次新开一个句柄)。
    pub fn open(&self) -> Repo {
        Repo::discover(self.path()).expect("打开临时仓库失败")
    }

    fn signature(&self) -> git2::Signature<'static> {
        let n = self.commits.get();
        git2::Signature::new(
            "Test",
            "test@example.com",
            &git2::Time::new(BASE_TIME + n * 60, 0),
        )
        .expect("构造签名失败")
    }

    /// 写入(必要时创建父目录)但不加入暂存区。
    pub fn write_untracked(&self, rel: &str, content: &str) -> &Self {
        let full = self.path().join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("创建目录失败");
        }
        std::fs::write(full, content).expect("写文件失败");
        self
    }

    /// 写文件、暂存并提交到当前分支。
    pub fn commit_file(&self, rel: &str, content: &str, message: &str) -> CommitId {
        self.commit_bytes(rel, content.as_bytes(), message)
    }

    /// 同 [`TempRepo::commit_file`],内容是任意字节(二进制、非 UTF-8)。
    pub fn commit_bytes(&self, rel: &str, content: &[u8], message: &str) -> CommitId {
        let full = self.path().join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("创建目录失败");
        }
        std::fs::write(full, content).expect("写文件失败");
        self.stage(rel);
        self.commit_staged(message)
    }

    /// 把当前暂存区原样提交到当前分支(配合 `stage`/`stage_remove` 做一次含多个改动的提交)。
    pub fn commit_staged(&self, message: &str) -> CommitId {
        let mut index = self.repo.index().expect("读取 index 失败");
        let tree = self
            .repo
            .find_tree(index.write_tree().expect("写 tree 失败"))
            .expect("找不到 tree");
        let sig = self.signature();
        let parents: Vec<git2::Commit> = match self.repo.head() {
            Ok(head) => vec![head.peel_to_commit().expect("HEAD 不是提交")],
            Err(_) => Vec::new(), // 第一次提交
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .expect("提交失败");
        self.commits.set(self.commits.get() + 1);
        CommitId::from_oid(oid)
    }

    /// 把分支 `other` 合并进当前分支,生成一个两父提交(两边改的文件不能冲突)。
    pub fn merge_commit(&self, other: &str, message: &str) -> CommitId {
        let ours = self
            .repo
            .head()
            .expect("合并前需要至少一次提交")
            .peel_to_commit()
            .expect("HEAD 不是提交");
        let theirs = self
            .repo
            .find_branch(other, git2::BranchType::Local)
            .expect("找不到分支")
            .get()
            .peel_to_commit()
            .expect("分支不是提交");
        let mut merged = self
            .repo
            .merge_commits(&ours, &theirs, None)
            .expect("合并失败");
        assert!(!merged.has_conflicts(), "merge_commit 只用于无冲突合并");
        let tree = self
            .repo
            .find_tree(merged.write_tree_to(&self.repo).expect("写 tree 失败"))
            .expect("找不到 tree");
        let sig = self.signature();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&ours, &theirs])
            .expect("提交失败");
        self.commits.set(self.commits.get() + 1);
        self.repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .expect("检出失败");
        CommitId::from_oid(oid)
    }

    /// 在当前 HEAD 创建分支(不切换)。
    pub fn branch(&self, name: &str) -> &Self {
        let head = self
            .repo
            .head()
            .expect("创建分支前需要至少一次提交")
            .peel_to_commit()
            .expect("HEAD 不是提交");
        self.repo.branch(name, &head, false).expect("创建分支失败");
        self
    }

    /// 切换到已有分支并更新工作区。
    pub fn checkout(&self, name: &str) -> &Self {
        let refname = format!("refs/heads/{name}");
        let obj = self.repo.revparse_single(&refname).expect("分支不存在");
        self.repo
            .checkout_tree(&obj, Some(git2::build::CheckoutBuilder::new().force()))
            .expect("检出失败");
        self.repo.set_head(&refname).expect("切换 HEAD 失败");
        self
    }

    pub fn add_remote(&self, name: &str, url: &str) -> &Self {
        self.repo.remote(name, url).expect("添加远程失败");
        self
    }

    /// 把工作区里的文件加入暂存区(不提交)。
    pub fn stage(&self, rel: &str) -> &Self {
        let mut index = self.repo.index().expect("读取 index 失败");
        index.add_path(Path::new(rel)).expect("暂存失败");
        index.write().expect("写 index 失败");
        self
    }

    /// 从工作区删除文件但不暂存(未暂存的删除)。
    pub fn delete_file(&self, rel: &str) -> &Self {
        std::fs::remove_file(self.path().join(rel)).expect("删除文件失败");
        self
    }

    /// 从暂存区和工作区一起删除(等价于 `git rm`)。
    pub fn stage_remove(&self, rel: &str) -> &Self {
        self.delete_file(rel);
        let mut index = self.repo.index().expect("读取 index 失败");
        index.remove_path(Path::new(rel)).expect("移除失败");
        index.write().expect("写 index 失败");
        self
    }

    /// 让 HEAD 脱离分支,指向当前提交。
    pub fn detach_head(&self) -> &Self {
        let id = self
            .repo
            .head()
            .expect("detach 前需要至少一次提交")
            .peel_to_commit()
            .expect("HEAD 不是提交")
            .id();
        self.repo.set_head_detached(id).expect("detach 失败");
        self
    }

    /// 制造一个合并冲突:两个分支对同一文件做不同修改,再在 `main` 上合并 `other`,
    /// 合并停在冲突状态(冲突文件留在 index 里)。结束时位于 `main`。
    pub fn make_conflict(&self, rel: &str) -> &Self {
        self.commit_file(rel, "base\n", "base");
        self.branch("other").checkout("other");
        self.commit_file(rel, "other\n", "other change");
        self.checkout("main");
        self.commit_file(rel, "main\n", "main change");
        let other = self
            .repo
            .find_branch("other", git2::BranchType::Local)
            .expect("找不到 other 分支")
            .get()
            .peel_to_commit()
            .expect("other 不是提交")
            .id();
        let annotated = self
            .repo
            .find_annotated_commit(other)
            .expect("构造 annotated commit 失败");
        self.repo
            .merge(&[&annotated], None, None)
            .expect("合并失败");
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_operations_give_same_commit_ids() {
        let a = TempRepo::new();
        let b = TempRepo::new();
        let ia = a.commit_file("f.txt", "1", "one");
        let ib = b.commit_file("f.txt", "1", "one");
        assert_eq!(ia, ib);
        let ja = a.commit_file("f.txt", "2", "two");
        let jb = b.commit_file("f.txt", "2", "two");
        assert_eq!(ja, jb);
        assert_ne!(ia, ja);
    }

    #[test]
    fn initial_branch_is_main() {
        let t = TempRepo::new();
        t.commit_file("f.txt", "1", "one");
        let head = t.repo.head().unwrap();
        assert_eq!(head.shorthand().unwrap(), "main");
    }

    #[test]
    fn branch_and_checkout_switch_head_and_files() {
        let t = TempRepo::new();
        t.commit_file("f.txt", "main-content", "one");
        t.branch("feature").checkout("feature");
        t.commit_file("f.txt", "feature-content", "two");
        t.checkout("main");
        let content = std::fs::read_to_string(t.path().join("f.txt")).unwrap();
        assert_eq!(content, "main-content");
        assert_eq!(t.repo.head().unwrap().shorthand().unwrap(), "main");
    }

    #[test]
    fn untracked_file_is_written_but_not_committed() {
        let t = TempRepo::new();
        t.commit_file("a.txt", "a", "one");
        t.write_untracked("sub/b.txt", "b");
        assert!(t.path().join("sub/b.txt").exists());
        let tree = t.repo.head().unwrap().peel_to_tree().unwrap();
        assert!(tree.get_path(Path::new("sub/b.txt")).is_err());
    }

    #[test]
    fn add_remote_is_visible_to_git2() {
        let t = TempRepo::new();
        t.add_remote("origin", "https://example.com/x.git");
        let remote = t.repo.find_remote("origin").unwrap();
        assert_eq!(remote.url().unwrap(), "https://example.com/x.git");
    }

    #[test]
    fn crlf_content_is_stored_byte_for_byte_regardless_of_global_git_config() {
        let t = TempRepo::new();
        let id = t.commit_file("crlf.txt", "a\r\nb\r\n", "crlf");
        let commit = t.repo.find_commit(id.oid()).unwrap();
        let entry = commit
            .tree()
            .unwrap()
            .get_path(Path::new("crlf.txt"))
            .unwrap();
        let blob = t.repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), b"a\r\nb\r\n");
    }

    #[test]
    fn checkout_does_not_rewrite_line_endings() {
        let t = TempRepo::new();
        t.commit_file("crlf.txt", "a\r\nb\r\n", "crlf");
        t.branch("other").checkout("other").checkout("main");
        let bytes = std::fs::read(t.path().join("crlf.txt")).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\n");
    }
}

//! 文件内容读取与分类:blob、某提交里的文件、提交 vs 工作区。

use std::io::Read;
use std::path::Path;

use crate::{BlobId, CommitId, GitError, Repo};

/// 读内容时的大小上限。超过上限的内容不读入内存,只报字节数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentLimits {
    pub max_bytes: usize,
}

impl ContentLimits {
    pub const fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

/// 一份内容的分类结果。判定顺序:先看大小,再看是否含 NUL 字节,最后看是否合法 UTF-8。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    /// 超过 [`ContentLimits::max_bytes`](恰好等于上限不算超过)。`bytes` 是实际大小。
    TooLarge {
        bytes: usize,
    },
    /// 含 NUL 字节。
    Binary,
    /// 不含 NUL,但不是合法 UTF-8。
    NotUtf8,
    /// 路径指向的不是文件(目录、子模块)。
    NotAFile,
}

impl Content {
    pub fn into_text(self) -> Option<String> {
        match self {
            Content::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// 提交里的文件与工作区里的同一路径。`None` 表示那一侧没有这个文件
/// (提交里不存在,或磁盘上不存在/读不了)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentPair {
    pub old: Option<Content>,
    pub new: Option<Content>,
}

pub(crate) fn classify(bytes: &[u8], limits: ContentLimits) -> Content {
    if bytes.len() > limits.max_bytes {
        return Content::TooLarge { bytes: bytes.len() };
    }
    if bytes.contains(&0u8) {
        return Content::Binary;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Content::Text(s.to_string()),
        Err(_) => Content::NotUtf8,
    }
}

fn classify_blob(blob: &git2::Blob<'_>, limits: ContentLimits) -> Content {
    // 先看大小,超限的 blob 不拷贝内容。
    if blob.size() > limits.max_bytes {
        return Content::TooLarge { bytes: blob.size() };
    }
    classify(blob.content(), limits)
}

impl Repo {
    /// 读一个 blob 并分类。blob 不存在返回错误。
    pub fn blob_text(&self, blob: BlobId, limits: ContentLimits) -> Result<Content, GitError> {
        let blob = self.raw().find_blob(blob.oid())?;
        Ok(classify_blob(&blob, limits))
    }

    /// `commit` 里 `path` 处的内容。该提交里没有这个路径返回 `Ok(None)`;
    /// 路径是目录或子模块返回 `Content::NotAFile`。
    pub fn file_at(
        &self,
        commit: CommitId,
        path: &Path,
        limits: ContentLimits,
    ) -> Result<Option<Content>, GitError> {
        let Some(entry) = self.tree_entry(commit, path)? else {
            return Ok(None);
        };
        let object = entry.to_object(self.raw())?;
        Ok(Some(match object.as_blob() {
            Some(blob) => classify_blob(blob, limits),
            None => Content::NotAFile,
        }))
    }

    /// `commit` 里 `path` 处的原始字节,不做大小或编码判定(回滚要原样写回,
    /// 二进制、超大、非 UTF-8 的文件也必须能取到)。该提交里没有这个路径返回
    /// `Ok(None)`;路径是目录或子模块返回错误。
    pub fn file_bytes_at(
        &self,
        commit: CommitId,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, GitError> {
        let Some(entry) = self.tree_entry(commit, path)? else {
            return Ok(None);
        };
        let object = entry.to_object(self.raw())?;
        match object.as_blob() {
            Some(blob) => Ok(Some(blob.content().to_vec())),
            None => Err(GitError::new(
                crate::GitErrorKind::Backend,
                "该历史版本对应的不是一个文件",
            )),
        }
    }

    /// 提交里的 `path`(旧侧)与工作区里的同一路径(新侧)。新侧直接读磁盘上的
    /// 实时内容(可能含未提交改动);文件缺失、是目录或读取失败时新侧为 `None`。
    /// 超过上限的磁盘文件只读到上限 + 1 字节就停,不整个读入内存。
    pub fn workdir_vs_commit(
        &self,
        commit: CommitId,
        path: &Path,
        limits: ContentLimits,
    ) -> Result<ContentPair, GitError> {
        let old = self.file_at(commit, path, limits)?;
        let new = read_disk(&self.root().join(path), limits);
        Ok(ContentPair { old, new })
    }

    fn tree_entry(
        &self,
        commit: CommitId,
        path: &Path,
    ) -> Result<Option<git2::TreeEntry<'static>>, GitError> {
        let commit = self.raw().find_commit(commit.oid())?;
        let tree = commit.tree()?;
        match tree.get_path(path) {
            Ok(entry) => Ok(Some(entry.to_owned())),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

fn read_disk(path: &Path, limits: ContentLimits) -> Option<Content> {
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limits.max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > limits.max_bytes {
        // 只读了 max+1 字节,真实大小以 metadata 为准(读的过程中文件可能在变,取较大者)。
        let size = std::fs::metadata(path).map_or(bytes.len(), |m| m.len() as usize);
        return Some(Content::TooLarge {
            bytes: size.max(bytes.len()),
        });
    }
    Some(classify(&bytes, limits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GitErrorKind;
    use crate::testutil::TempRepo;

    const LIMIT: ContentLimits = ContentLimits::new(16);

    #[test]
    fn classify_checks_size_then_nul_then_utf8() {
        assert_eq!(classify(b"", LIMIT), Content::Text(String::new()));
        assert_eq!(classify(&[b'a'; 16], LIMIT), Content::Text("a".repeat(16)));
        assert_eq!(
            classify(&[b'a'; 17], LIMIT),
            Content::TooLarge { bytes: 17 }
        );
        assert_eq!(classify(b"a\0b", LIMIT), Content::Binary);
        // 超限优先于二进制判定。
        assert_eq!(classify(&[0u8; 17], LIMIT), Content::TooLarge { bytes: 17 });
        assert_eq!(classify(&[0xff, 0xfe, b'a'], LIMIT), Content::NotUtf8);
        // BOM 是合法 UTF-8,原样保留。
        assert_eq!(
            classify("\u{feff}hi".as_bytes(), LIMIT),
            Content::Text("\u{feff}hi".to_string())
        );
    }

    #[test]
    fn into_text_only_yields_text() {
        assert_eq!(Content::Text("x".into()).into_text(), Some("x".to_string()));
        assert_eq!(Content::Binary.into_text(), None);
    }

    #[test]
    fn file_at_reads_text_and_reports_absent_paths_as_none() {
        let t = TempRepo::new();
        let c = t.commit_file("a.txt", "one\n", "add");
        let repo = t.open();
        assert_eq!(
            repo.file_at(c, Path::new("a.txt"), LIMIT).unwrap(),
            Some(Content::Text("one\n".to_string()))
        );
        assert_eq!(repo.file_at(c, Path::new("nope.txt"), LIMIT).unwrap(), None);
    }

    #[test]
    fn file_at_a_directory_is_not_a_file() {
        let t = TempRepo::new();
        let c = t.commit_file("sub/a.txt", "x", "add");
        assert_eq!(
            t.open().file_at(c, Path::new("sub"), LIMIT).unwrap(),
            Some(Content::NotAFile)
        );
    }

    #[test]
    fn file_at_classifies_binary_oversized_and_non_utf8_blobs() {
        let t = TempRepo::new();
        t.commit_bytes("bin.dat", b"ab\0cd", "bin");
        t.commit_bytes("big.txt", &[b'a'; 17], "big");
        let c = t.commit_bytes("latin.txt", &[0xe9, b'a'], "latin");
        let repo = t.open();
        assert_eq!(
            repo.file_at(c, Path::new("bin.dat"), LIMIT).unwrap(),
            Some(Content::Binary)
        );
        assert_eq!(
            repo.file_at(c, Path::new("big.txt"), LIMIT).unwrap(),
            Some(Content::TooLarge { bytes: 17 })
        );
        assert_eq!(
            repo.file_at(c, Path::new("latin.txt"), LIMIT).unwrap(),
            Some(Content::NotUtf8)
        );
    }

    #[test]
    fn blob_text_reads_a_blob_by_id() {
        let t = TempRepo::new();
        let c = t.commit_file("a.txt", "one\n", "add");
        let repo = t.open();
        let tree = t.raw_repo().find_commit(c.oid()).unwrap().tree().unwrap();
        let blob = BlobId::from_oid(tree.get_path(Path::new("a.txt")).unwrap().id());
        assert_eq!(
            repo.blob_text(blob, LIMIT).unwrap(),
            Content::Text("one\n".to_string())
        );
        let missing: BlobId = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
        assert!(repo.blob_text(missing, LIMIT).is_err());
    }

    #[test]
    fn file_bytes_at_returns_exact_bytes_regardless_of_limits_or_encoding() {
        let t = TempRepo::new();
        let bin = [0u8, 159, 146, 150, 0xff, 0];
        let c = t.commit_bytes("bin.dat", &bin, "bin");
        let repo = t.open();
        assert_eq!(
            repo.file_bytes_at(c, Path::new("bin.dat")).unwrap(),
            Some(bin.to_vec())
        );
        assert_eq!(repo.file_bytes_at(c, Path::new("nope")).unwrap(), None);
    }

    #[test]
    fn file_bytes_at_a_directory_is_an_error() {
        let t = TempRepo::new();
        let c = t.commit_file("sub/a.txt", "x", "add");
        let err = t.open().file_bytes_at(c, Path::new("sub")).unwrap_err();
        assert_eq!(err.kind(), GitErrorKind::Backend);
        assert_eq!(err.message(), "该历史版本对应的不是一个文件");
    }

    #[test]
    fn workdir_vs_commit_reads_the_live_disk_content() {
        let t = TempRepo::new();
        let c1 = t.commit_file("a.txt", "one\n", "add");
        t.write_untracked("a.txt", "edited, not committed\n");
        let pair = t
            .open()
            .workdir_vs_commit(c1, Path::new("a.txt"), ContentLimits::new(1024))
            .unwrap();
        assert_eq!(pair.old, Some(Content::Text("one\n".to_string())));
        assert_eq!(
            pair.new,
            Some(Content::Text("edited, not committed\n".to_string()))
        );
    }

    #[test]
    fn workdir_vs_commit_old_side_is_none_when_the_commit_has_no_such_path() {
        let t = TempRepo::new();
        let c1 = t.commit_file("a.txt", "one\n", "add");
        t.write_untracked("later.txt", "x\n");
        let pair = t
            .open()
            .workdir_vs_commit(c1, Path::new("later.txt"), LIMIT)
            .unwrap();
        assert_eq!(pair.old, None);
        assert_eq!(pair.new, Some(Content::Text("x\n".to_string())));
    }

    #[test]
    fn workdir_vs_commit_new_side_is_none_when_disk_file_is_missing_or_a_directory() {
        let t = TempRepo::new();
        let c1 = t.commit_file("a.txt", "one\n", "add");
        t.delete_file("a.txt");
        std::fs::create_dir(t.path().join("a.txt")).unwrap();
        let repo = t.open();
        assert_eq!(
            repo.workdir_vs_commit(c1, Path::new("a.txt"), LIMIT)
                .unwrap()
                .new,
            None
        );
        std::fs::remove_dir(t.path().join("a.txt")).unwrap();
        assert_eq!(
            repo.workdir_vs_commit(c1, Path::new("a.txt"), LIMIT)
                .unwrap()
                .new,
            None
        );
    }

    #[test]
    fn workdir_vs_commit_reports_the_real_size_of_an_oversized_disk_file() {
        let t = TempRepo::new();
        let c1 = t.commit_file("a.txt", "one\n", "add");
        t.write_untracked("a.txt", &"x".repeat(100));
        let pair = t
            .open()
            .workdir_vs_commit(c1, Path::new("a.txt"), LIMIT)
            .unwrap();
        assert_eq!(pair.new, Some(Content::TooLarge { bytes: 100 }));
    }
}

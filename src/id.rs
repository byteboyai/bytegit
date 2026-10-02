use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{GitError, GitErrorKind};

macro_rules! object_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(git2::Oid);

        impl $name {
            // 后续阶段(log/diff)才会在非测试代码里用到这两个转换。
            #[allow(dead_code)]
            pub(crate) fn from_oid(oid: git2::Oid) -> Self {
                Self(oid)
            }

            #[allow(dead_code)]
            pub(crate) fn oid(self) -> git2::Oid {
                self.0
            }

            /// 完整的十六进制 id。
            pub fn to_hex(self) -> String {
                self.0.to_string()
            }

            /// 前 `n` 位十六进制(`n` 超过长度时返回完整 id)。
            pub fn short(self, n: usize) -> String {
                let full = self.0.to_string();
                full[..n.min(full.len())].to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = GitError;

            fn from_str(s: &str) -> Result<Self, GitError> {
                if s.len() != 40 {
                    return Err(GitError::new(
                        GitErrorKind::Backend,
                        format!("不是 40 位十六进制 id: {s}"),
                    ));
                }
                git2::Oid::from_str(s).map(Self).map_err(GitError::from)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

object_id!(
    /// 提交 id。对外不暴露 `git2::Oid`。
    CommitId
);
object_id!(
    /// 文件内容(blob)id。
    BlobId
);

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parses_and_displays_round_trip() {
        let id: CommitId = HEX.parse().unwrap();
        assert_eq!(id.to_hex(), HEX);
        assert_eq!(id.to_string(), HEX);
    }

    #[test]
    fn short_truncates_and_clamps() {
        let id: CommitId = HEX.parse().unwrap();
        assert_eq!(id.short(7), "0123456");
        assert_eq!(id.short(100), HEX);
        assert_eq!(id.short(0), "");
    }

    #[test]
    fn rejects_malformed_hex() {
        assert!("abc".parse::<CommitId>().is_err());
        assert!(
            "zz23456789abcdef0123456789abcdef01234567"
                .parse::<CommitId>()
                .is_err()
        );
    }

    #[test]
    fn serde_is_a_plain_hex_string() {
        let id: CommitId = HEX.parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{HEX}\""));
        let back: CommitId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn debug_names_the_type() {
        let id: BlobId = HEX.parse().unwrap();
        assert_eq!(format!("{id:?}"), format!("BlobId({HEX})"));
    }
}

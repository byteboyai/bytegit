/// 一次变更的种类。替换调用方现在直接用的 `git2::Delta`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChange,
}

impl ChangeKind {
    /// `Unmodified`/`Ignored`/`Untracked`/`Unreadable`/`Conflicted` 不是"变更",返回 `None`。
    pub(crate) fn from_delta(delta: git2::Delta) -> Option<Self> {
        match delta {
            git2::Delta::Added => Some(Self::Added),
            git2::Delta::Modified => Some(Self::Modified),
            git2::Delta::Deleted => Some(Self::Deleted),
            git2::Delta::Renamed => Some(Self::Renamed),
            git2::Delta::Copied => Some(Self::Copied),
            git2::Delta::Typechange => Some(Self::TypeChange),
            git2::Delta::Unmodified
            | git2::Delta::Ignored
            | git2::Delta::Untracked
            | git2::Delta::Unreadable
            | git2::Delta::Conflicted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_real_changes() {
        assert_eq!(
            ChangeKind::from_delta(git2::Delta::Added),
            Some(ChangeKind::Added)
        );
        assert_eq!(
            ChangeKind::from_delta(git2::Delta::Modified),
            Some(ChangeKind::Modified)
        );
        assert_eq!(
            ChangeKind::from_delta(git2::Delta::Deleted),
            Some(ChangeKind::Deleted)
        );
        assert_eq!(
            ChangeKind::from_delta(git2::Delta::Renamed),
            Some(ChangeKind::Renamed)
        );
        assert_eq!(
            ChangeKind::from_delta(git2::Delta::Copied),
            Some(ChangeKind::Copied)
        );
        assert_eq!(
            ChangeKind::from_delta(git2::Delta::Typechange),
            Some(ChangeKind::TypeChange)
        );
    }

    #[test]
    fn non_changes_map_to_none() {
        for d in [
            git2::Delta::Unmodified,
            git2::Delta::Ignored,
            git2::Delta::Untracked,
            git2::Delta::Unreadable,
            git2::Delta::Conflicted,
        ] {
            assert_eq!(ChangeKind::from_delta(d), None, "{d:?}");
        }
    }
}

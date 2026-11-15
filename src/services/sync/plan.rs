use super::{ConflictPolicy, Direction, local::LocalFile};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RemoteFile {
    pub(super) bytes: i64,
    pub(super) etag: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Pair {
    pub(super) local: LocalFile,
    pub(super) remote: RemoteFile,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
    Upload,
    Download,
    Keep,
    Conflict,
    DeleteLocal,
    DeleteRemote,
}

pub(super) fn decide(
    direction: Direction,
    propagate_deletes: bool,
    previous: Option<&Pair>,
    local: Option<&LocalFile>,
    remote: Option<&RemoteFile>,
) -> Action {
    match (local, remote, previous) {
        (Some(_), None, None) if direction != Direction::Download => Action::Upload,
        (None, Some(_), None) if direction != Direction::Upload => Action::Download,
        (Some(local), Some(remote), Some(previous)) => {
            match (local != &previous.local, remote != &previous.remote) {
                (false, false) => Action::Keep,
                (true, false) if direction != Direction::Download => Action::Upload,
                (false, true) if direction != Direction::Upload => Action::Download,
                _ => Action::Conflict,
            }
        }
        (Some(local), None, Some(previous))
            if propagate_deletes && direction != Direction::Upload && local == &previous.local =>
        {
            Action::DeleteLocal
        }
        (None, Some(remote), Some(previous))
            if propagate_deletes
                && direction != Direction::Download
                && remote == &previous.remote =>
        {
            Action::DeleteRemote
        }
        (Some(_), Some(_), None) | (Some(_), None, Some(_)) | (None, Some(_), Some(_)) => {
            Action::Conflict
        }
        _ => Action::Keep,
    }
}

impl ConflictPolicy {
    pub(super) fn resolve(self, direction: Direction, action: Action, both_exist: bool) -> Action {
        match (direction, action, both_exist, self) {
            (Direction::Both, Action::Conflict, true, Self::PreferLocal) => Action::Upload,
            (Direction::Both, Action::Conflict, true, Self::PreferRemote) => Action::Download,
            (_, action, _, _) => action,
        }
    }

    pub(super) fn resolve_once(
        self,
        action: Action,
        local_exists: bool,
        remote_exists: bool,
    ) -> Action {
        match (self, action) {
            (Self::PreferLocal, Action::Conflict) if local_exists => Action::Upload,
            (Self::PreferRemote, Action::Conflict) if remote_exists => Action::Download,
            (_, action) => action,
        }
    }
}

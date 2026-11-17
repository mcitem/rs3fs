use super::store::TransferCancelled;
use anyhow::Result;

#[derive(Clone)]
pub(crate) enum TransferState {
    Queued,
    Sending,
    Complete,
    Cancelled,
    Failed(String),
}

impl TransferState {
    pub(crate) fn active(&self) -> bool {
        matches!(self, Self::Queued | Self::Sending)
    }

    pub(crate) fn from_result(result: Result<()>) -> Self {
        match result {
            Ok(()) => Self::Complete,
            Err(error) if error.is::<TransferCancelled>() => Self::Cancelled,
            Err(error) => Self::Failed(format!("{error:#}")),
        }
    }
}

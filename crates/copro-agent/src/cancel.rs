use tokio_util::sync::CancellationToken;

/// Per-turn cancellation source shared by the driver and in-flight model/tool work.
#[derive(Debug, Clone)]
pub(crate) struct TurnCancellation {
    token: CancellationToken,
}

impl TurnCancellation {
    pub(crate) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.token.cancel();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::TurnCancellation;

    #[test]
    fn cancellation_state_is_shared_across_clones() {
        let cancellation = TurnCancellation::new();
        let clone = cancellation.clone();

        assert!(!cancellation.is_cancelled());
        clone.cancel();
        assert!(cancellation.is_cancelled());
    }
}

//! Polling change detection: a pure state machine the CLI loop drives.
//! No sleeping, no clock — callers act on the returned [`Tick`].

use std::time::Duration;

use crate::api::{ApiError, FigmaApi};

pub(crate) const BACKOFF_START: Duration = Duration::from_secs(5);
pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Outcome of one poll.
#[derive(Debug)]
pub enum Tick {
    /// File unchanged since the last seen `last_touched_at`.
    Unchanged,
    /// File changed — caller should pull. The new watermark is already
    /// absorbed into `self.last_seen`; every caller (`serve.rs`'s
    /// `watch_tick`) just matches `Changed { .. }` and re-pulls from the
    /// store's own state, so this variant carries no payload.
    Changed,
    /// Transient failure or rate limit — caller should sleep `after`
    /// (instead of its normal interval), then tick again.
    Wait { after: Duration },
}

/// Tracks the last seen content-modification time and failure backoff.
pub struct Watcher {
    last_seen: Option<String>,
    backoff: Duration,
}

impl Watcher {
    /// `last_seen`: the stored `FileMeta.last_modified`, if any. A spurious
    /// mismatch only costs one pull that produces zero churn.
    pub fn new(last_seen: Option<String>) -> Self {
        Watcher {
            last_seen,
            backoff: BACKOFF_START,
        }
    }

    /// Poll `file_meta` once and classify the result. Never fetches the
    /// file itself — that's the caller's job on [`Tick::Changed`].
    pub fn tick(&mut self, api: &dyn FigmaApi, key: &str) -> Tick {
        match api.file_meta(key) {
            Ok(meta) => {
                self.backoff = BACKOFF_START;
                if self.last_seen.as_deref() == Some(meta.last_touched_at.as_str()) {
                    Tick::Unchanged
                } else {
                    self.last_seen = Some(meta.last_touched_at);
                    Tick::Changed
                }
            }
            Err(ApiError::RateLimited { retry_after }) => Tick::Wait { after: retry_after },
            Err(_) => {
                let after = self.backoff;
                self.backoff = (self.backoff * 2).min(BACKOFF_CAP);
                Tick::Wait { after }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiError, FigmaApi, FileMetaResp};
    use std::cell::RefCell;
    use std::time::Duration;

    /// Scripted API: pops one response per call; panics if `file` is called.
    struct Script(RefCell<Vec<Result<FileMetaResp, ApiError>>>);
    impl Script {
        fn new(mut responses: Vec<Result<FileMetaResp, ApiError>>) -> Self {
            responses.reverse();
            Script(RefCell::new(responses))
        }
    }
    impl FigmaApi for Script {
        fn file_meta(&self, _key: &str) -> Result<FileMetaResp, ApiError> {
            self.0
                .borrow_mut()
                .pop()
                .expect("unexpected extra file_meta call")
        }
        fn file(&self, _key: &str, _geometry: bool) -> Result<serde_json::Value, ApiError> {
            panic!("watcher must never fetch the file itself");
        }
    }

    fn meta(t: &str) -> Result<FileMetaResp, ApiError> {
        Ok(FileMetaResp {
            name: "F".into(),
            last_touched_at: t.into(),
        })
    }

    #[test]
    fn unchanged_then_changed() {
        let api = Script::new(vec![meta("t1"), meta("t1"), meta("t2"), meta("t2")]);
        let mut w = Watcher::new(Some("t1".into()));
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
        // The watermark that advanced internally on the `Changed` tick
        // ("t2") is now what a repeat sees as unchanged — `Tick::Changed`
        // carries no payload, so this is how the update is observed.
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
    }

    #[test]
    fn first_tick_with_no_history_is_changed() {
        let api = Script::new(vec![meta("t1")]);
        let mut w = Watcher::new(None);
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
    }

    #[test]
    fn rate_limit_uses_retry_after() {
        let api = Script::new(vec![
            Err(ApiError::RateLimited {
                retry_after: Duration::from_secs(30),
            }),
            meta("t1"),
        ]);
        let mut w = Watcher::new(Some("t1".into()));
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(30))
        );
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
    }

    #[test]
    fn failures_back_off_exponentially_and_reset_on_success() {
        let api = Script::new(vec![
            Err(ApiError::Network("down".into())),
            Err(ApiError::Network("down".into())),
            Err(ApiError::Network("down".into())),
            meta("t1"),
            Err(ApiError::Network("down".into())),
        ]);
        let mut w = Watcher::new(Some("t1".into()));
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(5))
        );
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(10))
        );
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(20))
        );
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(5))
        );
    }

    #[test]
    fn backoff_caps_at_five_minutes() {
        let mut responses: Vec<Result<FileMetaResp, ApiError>> = (0..10)
            .map(|_| Err(ApiError::Network("down".into())))
            .collect();
        responses.push(meta("t1"));
        let api = Script::new(responses);
        let mut w = Watcher::new(Some("t1".into()));
        let mut last = Duration::ZERO;
        for _ in 0..10 {
            match w.tick(&api, "k") {
                Tick::Wait { after } => last = after,
                other => panic!("expected Wait, got {other:?}"),
            }
        }
        assert_eq!(last, Duration::from_secs(300));
    }
}

use std::time::{Duration, Instant};

use tokio::sync::RwLock;

// Re-export protocol types so existing `use super::presence::X` imports work.
pub use protocol::{PresenceState, PresenceUpdate};

struct PresenceInner {
    state: PresenceState,
    updated_at: Instant,
}

pub struct Presence {
    inner: RwLock<PresenceInner>,
    ttl: Duration,
}

impl Presence {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            inner: RwLock::new(PresenceInner {
                state: PresenceState::Away,
                updated_at: Instant::now(),
            }),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Returns the stored presence state without TTL fallback.
    pub async fn raw_state(&self) -> PresenceState {
        self.inner.read().await.state
    }

    /// Returns the current presence state, falling back to Away if stale.
    pub async fn get(&self) -> PresenceState {
        let inner = self.inner.read().await;
        if inner.updated_at.elapsed() > self.ttl {
            PresenceState::Away
        } else {
            inner.state
        }
    }

    pub async fn set(&self, state: PresenceState) {
        let mut inner = self.inner.write().await;
        inner.state = state;
        inner.updated_at = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defaults_to_away() {
        let p = Presence::new(120);
        assert_eq!(p.get().await, PresenceState::Away);
    }

    #[tokio::test]
    async fn set_and_get() {
        let p = Presence::new(120);
        p.set(PresenceState::Present).await;
        assert_eq!(p.get().await, PresenceState::Present);
    }

    #[tokio::test]
    async fn stale_returns_away() {
        let p = Presence::new(0); // 0s TTL = always stale
        p.set(PresenceState::Present).await;
        assert_eq!(p.get().await, PresenceState::Away);
    }
}

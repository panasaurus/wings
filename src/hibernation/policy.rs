use std::time::{Duration, Instant};

pub struct IdlePolicy {
    empty_since: Option<Instant>,
    started_at: Instant,
    last_join: Instant,
    idle_timeout: Duration,
    startup_grace: Duration,
}

impl IdlePolicy {
    pub fn new(now: Instant, idle_timeout: Duration, startup_grace: Duration) -> Self {
        Self {
            empty_since: None,
            started_at: now,
            last_join: now,
            idle_timeout,
            startup_grace,
        }
    }

    pub fn started(&mut self, now: Instant) {
        self.started_at = now;
        self.joined(now);
    }

    pub fn joined(&mut self, now: Instant) {
        self.last_join = now;
        self.empty_since = None;
    }

    pub fn observe(
        &mut self,
        now: Instant,
        players: Option<u64>,
        pending_connections: usize,
    ) -> bool {
        if players != Some(0) || pending_connections > 0 {
            self.empty_since = None;
            return false;
        }
        let empty_since = *self.empty_since.get_or_insert(now);
        now.saturating_duration_since(self.started_at) >= self.startup_grace
            && now.saturating_duration_since(empty_since.max(self.last_join)) >= self.idle_timeout
    }
}

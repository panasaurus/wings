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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_requires_a_continuous_verified_empty_period() {
        let now = Instant::now();
        let mut policy = IdlePolicy::new(now, Duration::from_secs(60), Duration::from_secs(120));
        assert!(!policy.observe(now, Some(0), 0));
        assert!(!policy.observe(now + Duration::from_secs(60), Some(0), 0));
        assert!(policy.observe(now + Duration::from_secs(120), Some(0), 0));
    }

    #[test]
    fn unknown_count_players_and_pending_joins_reset_the_timer() {
        let now = Instant::now();
        let mut policy = IdlePolicy::new(now, Duration::from_secs(60), Duration::ZERO);
        for (players, connections) in [(None, 0), (Some(1), 0), (Some(0), 1)] {
            policy.started(now);
            assert!(!policy.observe(now, Some(0), 0));
            assert!(!policy.observe(now + Duration::from_secs(59), players, connections));
            assert!(!policy.observe(now + Duration::from_secs(60), Some(0), 0));
            assert!(!policy.observe(now + Duration::from_secs(119), Some(0), 0));
            assert!(policy.observe(now + Duration::from_secs(120), Some(0), 0));
        }
    }

    #[test]
    fn join_and_restart_each_require_a_new_idle_period() {
        let now = Instant::now();
        let mut policy = IdlePolicy::new(now, Duration::from_secs(60), Duration::ZERO);
        policy.observe(now, Some(0), 0);
        policy.joined(now + Duration::from_secs(59));
        assert!(!policy.observe(now + Duration::from_secs(60), Some(0), 0));
        assert!(policy.observe(now + Duration::from_secs(120), Some(0), 0));
        policy.started(now + Duration::from_secs(120));
        assert!(!policy.observe(now + Duration::from_secs(121), Some(0), 0));
    }
}

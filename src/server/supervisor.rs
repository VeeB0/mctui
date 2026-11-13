//! Политика авто-рестарта с защитой от crash-loop.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Restart,
    GiveUp,
}

pub struct CrashPolicy {
    history: VecDeque<Instant>,
    max: usize,
    window: Duration,
}

impl CrashPolicy {
    pub fn new(max: usize, window: Duration) -> Self {
        Self { history: VecDeque::new(), max, window }
    }

    /// Ручной запуск пользователем сбрасывает счётчик падений.
    pub fn reset(&mut self) {
        self.history.clear();
    }

    pub fn on_crash(&mut self, now: Instant) -> Decision {
        while let Some(&t) = self.history.front() {
            if now.duration_since(t) > self.window {
                self.history.pop_front();
            } else {
                break;
            }
        }
        self.history.push_back(now);
        if self.history.len() >= self.max {
            self.history.clear();
            Decision::GiveUp
        } else {
            Decision::Restart
        }
    }
}

impl Default for CrashPolicy {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_up_after_three_crashes_in_window() {
        let mut p = CrashPolicy::default();
        let t0 = Instant::now();
        assert_eq!(p.on_crash(t0), Decision::Restart);
        assert_eq!(p.on_crash(t0 + Duration::from_secs(60)), Decision::Restart);
        assert_eq!(p.on_crash(t0 + Duration::from_secs(120)), Decision::GiveUp);
    }

    #[test]
    fn old_crashes_expire() {
        let mut p = CrashPolicy::default();
        let t0 = Instant::now();
        p.on_crash(t0);
        p.on_crash(t0 + Duration::from_secs(10));
        assert_eq!(p.on_crash(t0 + Duration::from_secs(400)), Decision::Restart);
        assert_eq!(p.on_crash(t0 + Duration::from_secs(401)), Decision::Restart);
        assert_eq!(p.on_crash(t0 + Duration::from_secs(402)), Decision::GiveUp);
    }
}

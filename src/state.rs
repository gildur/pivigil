use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
}

// Times are monotonic offsets from the first completed camera frame.
pub struct RecorderState {
    warmup: Duration,
    tail: Duration,
    motion: bool,
    recording: bool,
    quiet_since: Option<Duration>,
}

impl RecorderState {
    pub fn new(warmup: Duration, tail: Duration) -> Self {
        Self {
            warmup,
            tail,
            motion: false,
            recording: false,
            quiet_since: None,
        }
    }

    pub fn motion(&mut self, detected: bool, now: Duration) {
        if detected == self.motion {
            return;
        }
        self.motion = detected;
        self.quiet_since = if detected { None } else { Some(now) };
    }

    pub fn tick(&mut self, now: Duration) -> Option<Action> {
        let wanted = now >= self.warmup
            && (self.motion
                || (self.recording
                    && self
                        .quiet_since
                        .is_some_and(|t| now.saturating_sub(t) < self.tail)));
        if wanted == self.recording {
            return None;
        }
        self.recording = wanted;
        Some(if wanted { Action::Start } else { Action::Stop })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn warmup_persistent_motion_tail_and_retrigger() {
        let mut r = RecorderState::new(s(3), s(10));
        r.motion(true, s(1));
        assert_eq!(r.tick(s(2)), None);
        assert_eq!(r.tick(s(3)), Some(Action::Start));
        assert_eq!(r.tick(s(100)), None); // continuous motion never times out
        r.motion(false, s(101));
        assert_eq!(r.tick(s(110)), None);
        r.motion(true, s(110));
        assert_eq!(r.tick(s(120)), None);
        r.motion(false, s(121));
        assert_eq!(r.tick(s(131)), Some(Action::Stop));
        assert_eq!(r.tick(s(132)), None);
    }

    #[test]
    fn transient_warmup_motion_does_not_record() {
        let mut r = RecorderState::new(s(3), s(10));
        r.motion(true, s(1));
        r.motion(false, s(2));
        assert_eq!(r.tick(s(3)), None);
    }

    #[test]
    fn repeated_quiet_samples_do_not_extend_the_tail() {
        let mut r = RecorderState::new(s(0), s(3));
        r.motion(true, s(0));
        assert_eq!(r.tick(s(0)), Some(Action::Start));
        r.motion(false, s(1));
        r.motion(false, s(2));
        r.motion(false, s(3));
        assert_eq!(r.tick(s(4)), Some(Action::Stop));
    }

    #[test]
    fn finished_motion_does_not_restart_for_a_stale_tail() {
        let mut r = RecorderState::new(s(0), s(10));
        r.motion(true, s(0));
        assert_eq!(r.tick(s(0)), Some(Action::Start));
        r.motion(false, s(1));
        assert_eq!(r.tick(s(11)), Some(Action::Stop));
        assert_eq!(r.tick(s(12)), None);
    }
}

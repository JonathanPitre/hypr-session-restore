//! Pure save scheduler. The daemon injects the clock and applies the tick.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowStamp {
    pub address: String,
    pub workspace: String,
    pub monitor: String,
    pub floating: bool,
    pub fullscreen: bool,
    pub position: (i32, i32),
    pub size: (i32, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta {
    Unchanged,
    VanishOnly,
    Material,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveReason {
    Periodic,
    Signal,
}

#[derive(Debug, Clone)]
pub struct SettleState {
    pub delay: Duration,
    pub deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Save,
    Wait(Instant),
    Idle,
}

/// `None` previous is the first tick and is always [`Delta::Material`].
///
/// [`Delta::VanishOnly`] means every current window still matches the previous
/// stamp with the same address, at least one previous address is gone, and
/// none were added.
pub fn classify(previous: Option<&[WindowStamp]>, current: &[WindowStamp]) -> Delta {
    let Some(previous) = previous else {
        return Delta::Material;
    };
    let mut prior = HashMap::<&str, &WindowStamp>::new();
    for stamp in previous {
        prior.insert(stamp.address.as_str(), stamp);
    }
    let mut seen = HashSet::<&str>::new();
    for stamp in current {
        let Some(old) = prior.get(stamp.address.as_str()) else {
            return Delta::Material;
        };
        if *old != stamp {
            return Delta::Material;
        }
        seen.insert(stamp.address.as_str());
    }
    if prior.keys().any(|address| !seen.contains(address)) {
        Delta::VanishOnly
    } else {
        Delta::Unchanged
    }
}

pub fn on_tick(
    delta: Delta,
    reason: SaveReason,
    settle: &mut SettleState,
    now: Instant,
    window_count: usize,
) -> Tick {
    if window_count == 0 {
        settle.deadline = None;
        return Tick::Idle;
    }
    if reason == SaveReason::Signal {
        settle.deadline = None;
        return Tick::Save;
    }
    match delta {
        Delta::VanishOnly => {
            let deadline = now + settle.delay;
            settle.deadline = Some(deadline);
            Tick::Wait(deadline)
        }
        Delta::Unchanged | Delta::Material => {
            settle.deadline = None;
            Tick::Save
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(address: &str, workspace: &str) -> WindowStamp {
        WindowStamp {
            address: address.to_string(),
            workspace: workspace.to_string(),
            monitor: "DP-1".to_string(),
            floating: false,
            fullscreen: false,
            position: (10, 20),
            size: (800, 600),
        }
    }

    fn settle() -> SettleState {
        SettleState {
            delay: Duration::from_secs(10),
            deadline: None,
        }
    }

    #[test]
    fn subset_vanish_arms_wait() {
        let previous = vec![stamp("a", "1"), stamp("b", "1")];
        let current = vec![stamp("a", "1")];
        assert_eq!(classify(None, &current), Delta::Material);
        assert_eq!(classify(Some(&previous), &current), Delta::VanishOnly);
        let now = Instant::now();
        let mut state = settle();
        assert_eq!(
            on_tick(Delta::VanishOnly, SaveReason::Periodic, &mut state, now, 1),
            Tick::Wait(now + Duration::from_secs(10))
        );
        assert_eq!(state.deadline, Some(now + Duration::from_secs(10)));
    }

    #[test]
    fn material_cancels_deadline_and_saves() {
        let previous = vec![stamp("a", "1")];
        let moved = vec![stamp("a", "2")];
        assert_eq!(classify(Some(&previous), &moved), Delta::Material);
        let now = Instant::now();
        let mut state = settle();
        state.deadline = Some(now + Duration::from_secs(10));
        assert_eq!(
            on_tick(Delta::Material, SaveReason::Periodic, &mut state, now, 1),
            Tick::Save
        );
        assert!(state.deadline.is_none());
    }

    #[test]
    fn signal_saves_immediately() {
        let now = Instant::now();
        let mut state = settle();
        state.deadline = Some(now + Duration::from_secs(10));
        assert_eq!(
            on_tick(Delta::VanishOnly, SaveReason::Signal, &mut state, now, 2),
            Tick::Save
        );
        assert!(state.deadline.is_none());
    }

    #[test]
    fn repeated_vanish_pushes_deadline() {
        let start = Instant::now();
        let mut state = settle();
        let first = on_tick(
            Delta::VanishOnly,
            SaveReason::Periodic,
            &mut state,
            start,
            2,
        );
        assert_eq!(first, Tick::Wait(start + Duration::from_secs(10)));
        let later = start + Duration::from_secs(4);
        let second = on_tick(
            Delta::VanishOnly,
            SaveReason::Periodic,
            &mut state,
            later,
            1,
        );
        assert_eq!(second, Tick::Wait(later + Duration::from_secs(10)));
        assert_ne!(first, second);
        assert_eq!(
            on_tick(Delta::Unchanged, SaveReason::Periodic, &mut state, later, 1),
            Tick::Save
        );
        assert!(state.deadline.is_none());
    }
}

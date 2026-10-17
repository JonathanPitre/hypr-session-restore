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
    Added,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveReason {
    Periodic,
    Explicit,
    Shutdown,
}

#[derive(Debug, Default)]
pub struct SaveHistory {
    pub observed: Option<Vec<WindowStamp>>,
    protected_addresses: Option<HashSet<String>>,
    startup_saved_count: Option<usize>,
}

impl SaveHistory {
    pub fn new(initial: Vec<WindowStamp>, startup_saved_count: Option<usize>) -> Self {
        Self {
            protected_addresses: Some(initial.iter().map(|stamp| stamp.address.clone()).collect()),
            observed: Some(initial),
            startup_saved_count,
        }
    }

    pub fn record_save(&mut self, stamps: Vec<WindowStamp>) {
        self.protected_addresses = Some(stamps.iter().map(|stamp| stamp.address.clone()).collect());
        self.observed = Some(stamps);
        self.startup_saved_count = None;
    }

    pub fn startup_would_shrink(&self, count: usize) -> bool {
        self.startup_saved_count.is_some_and(|saved| count < saved)
    }

    pub fn shutdown_would_shrink(&self, current: &[WindowStamp]) -> bool {
        self.startup_would_shrink(current.len())
            || self.protected_addresses.as_ref().is_some_and(|saved| {
                current.len() < saved.len()
                    && current.iter().all(|stamp| saved.contains(&stamp.address))
            })
    }
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
/// [`Delta::VanishOnly`] means at least one previous address is gone and none
/// were added. Surviving windows may have reflowed as their neighbours closed.
pub fn classify(previous: Option<&[WindowStamp]>, current: &[WindowStamp]) -> Delta {
    let Some(previous) = previous else {
        return Delta::Material;
    };
    let mut prior = HashMap::<&str, &WindowStamp>::new();
    for stamp in previous {
        prior.insert(stamp.address.as_str(), stamp);
    }
    let mut seen = HashSet::<&str>::new();
    let mut changed = false;
    for stamp in current {
        let Some(old) = prior.get(stamp.address.as_str()) else {
            return Delta::Added;
        };
        changed |= *old != stamp;
        seen.insert(stamp.address.as_str());
    }
    if prior.keys().any(|address| !seen.contains(address)) {
        Delta::VanishOnly
    } else if changed {
        Delta::Material
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
    if reason != SaveReason::Periodic {
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
            if let Some(deadline) = settle.deadline
                && now < deadline
            {
                return Tick::Wait(deadline);
            }
            settle.deadline = None;
            Tick::Save
        }
        Delta::Added => {
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
    fn disappearance_with_reflow_still_settles() {
        let previous = vec![stamp("a", "1"), stamp("b", "1")];
        let mut helper = stamp("a", "1");
        helper.position = (0, 0);
        helper.size = (1920, 1080);
        let now = Instant::now();
        let mut state = settle();
        assert_eq!(
            on_tick(
                classify(Some(&previous), &[helper]),
                SaveReason::Periodic,
                &mut state,
                now,
                1,
            ),
            Tick::Wait(now + Duration::from_secs(10)),
            "a close-induced reflow must not save the reduced capture before settling"
        );
    }

    #[test]
    fn shutdown_subset_requires_a_saved_capture_and_no_new_addresses() {
        let mut history = SaveHistory::default();
        let current = vec![stamp("a", "2")];
        assert!(!history.shutdown_would_shrink(&current));
        history.record_save(vec![stamp("a", "1"), stamp("b", "1")]);
        assert!(history.shutdown_would_shrink(&current));
        assert!(!history.shutdown_would_shrink(&[stamp("c", "1")]));
        assert!(!history.shutdown_would_shrink(&[stamp("a", "2"), stamp("b", "2")]));
    }

    #[test]
    fn material_respects_deadline() {
        let previous = vec![stamp("a", "1")];
        let moved = vec![stamp("a", "2")];
        assert_eq!(classify(Some(&previous), &moved), Delta::Material);
        let now = Instant::now();
        let mut state = settle();
        state.deadline = Some(now + Duration::from_secs(10));
        assert_eq!(
            on_tick(Delta::Material, SaveReason::Periodic, &mut state, now, 1),
            Tick::Wait(now + Duration::from_secs(10))
        );
        assert_eq!(
            on_tick(
                Delta::Unchanged,
                SaveReason::Periodic,
                &mut state,
                now + Duration::from_secs(10),
                1,
            ),
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
            on_tick(Delta::VanishOnly, SaveReason::Explicit, &mut state, now, 2),
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
            Tick::Wait(later + Duration::from_secs(10))
        );
        assert_eq!(state.deadline, Some(later + Duration::from_secs(10)));
    }

    #[test]
    fn added_window_saves_immediately() {
        let previous = vec![stamp("a", "1")];
        let current = vec![stamp("a", "1"), stamp("c", "2")];
        let now = Instant::now();
        let mut state = settle();
        state.deadline = Some(now + state.delay);
        assert_eq!(classify(Some(&previous), &current), Delta::Added);
        assert_eq!(
            on_tick(Delta::Added, SaveReason::Periodic, &mut state, now, 2),
            Tick::Save
        );
        assert!(state.deadline.is_none());
    }

    #[test]
    fn initial_membership_and_disk_count_protect_shutdown_until_save() {
        let mut history = SaveHistory::new(vec![stamp("a", "1"), stamp("b", "1")], Some(6));
        assert_eq!(history.observed.as_ref().unwrap().len(), 2);
        assert!(history.shutdown_would_shrink(&[stamp("a", "2")]));
        assert!(history.shutdown_would_shrink(&[stamp("c", "1")]));
        history.observed = Some(vec![stamp("a", "2")]);
        assert!(history.shutdown_would_shrink(&[stamp("a", "2")]));
        history.record_save(vec![stamp("a", "2")]);
        assert!(!history.shutdown_would_shrink(&[stamp("a", "2")]));
        assert!(!history.startup_would_shrink(1));
    }

    #[test]
    fn initial_membership_protects_shutdown_without_a_disk_count() {
        let mut history = SaveHistory::new(vec![stamp("a", "1"), stamp("b", "1")], None);
        history.observed = Some(vec![stamp("a", "2")]);
        assert!(history.shutdown_would_shrink(&[stamp("a", "2")]));
    }
}

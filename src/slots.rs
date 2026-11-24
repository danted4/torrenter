//! Download slots: how many jobs download at once.
//!
//! A job holds a slot only while it downloads. A paused job and a seeding
//! share job give theirs back, and a waiting job takes the next free one in
//! list order (lowest `rank` first). The cap changes live: raising it starts
//! waiting jobs, lowering it never interrupts a running one. A disk-full
//! failure *holds* the queue (no new starts) until something lifts it.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

/// Downloads at once when the setting is unset.
pub const DEFAULT_MAX_DOWNLOADS: usize = 4;
/// Upper end of the "downloads at once" setting.
pub const MAX_DOWNLOADS_CAP: u8 = 8;

pub fn clamp_max_downloads(value: u8) -> u8 {
    value.clamp(1, MAX_DOWNLOADS_CAP)
}

#[derive(Debug)]
struct State {
    cap: usize,
    active: usize,
    held: bool,
    /// Jobs waiting for a slot: (rank, ticket). Paused jobs are left out.
    waiting: BTreeSet<(u64, u64)>,
    next_ticket: u64,
}

#[derive(Debug)]
pub struct Slots {
    state: Mutex<State>,
    notify: tokio::sync::Notify,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Slots {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                cap: cap.clamp(1, usize::from(MAX_DOWNLOADS_CAP)),
                active: 0,
                held: false,
                waiting: BTreeSet::new(),
                next_ticket: 1,
            }),
            notify: tokio::sync::Notify::new(),
        })
    }

    pub fn cap(&self) -> usize {
        lock(&self.state).cap
    }

    /// Lowering never takes a slot back from a running job.
    pub fn set_cap(&self, cap: usize) {
        lock(&self.state).cap = cap.clamp(1, usize::from(MAX_DOWNLOADS_CAP));
        self.notify.notify_waiters();
    }

    pub fn active(&self) -> usize {
        lock(&self.state).active
    }

    pub fn held(&self) -> bool {
        lock(&self.state).held
    }

    /// Disk full: no waiting job starts until [`Slots::lift`].
    pub fn hold(&self) {
        lock(&self.state).held = true;
    }

    pub fn lift(&self) {
        lock(&self.state).held = false;
        self.notify.notify_waiters();
    }

    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    fn ticket(&self) -> u64 {
        let mut state = lock(&self.state);
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        ticket
    }

    /// One poll for a waiting job: registers it (unless paused) and takes a
    /// slot when it is first in line, there is room and the queue is not held.
    fn try_take(&self, key: (u64, u64), paused: bool) -> bool {
        let mut state = lock(&self.state);
        if paused {
            if state.waiting.remove(&key) {
                drop(state);
                self.notify.notify_waiters();
            }
            return false;
        }
        state.waiting.insert(key);
        let first = state.waiting.first() == Some(&key);
        if first && !state.held && state.active < state.cap {
            state.waiting.remove(&key);
            state.active += 1;
            return true;
        }
        false
    }

    fn forget(&self, key: (u64, u64)) {
        if lock(&self.state).waiting.remove(&key) {
            self.notify.notify_waiters();
        }
    }

    fn give_back(&self) {
        {
            let mut state = lock(&self.state);
            state.active = state.active.saturating_sub(1);
        }
        self.notify.notify_waiters();
    }
}

/// A job's place in the slot queue, kept on its `JobSnapshot` for as long as
/// its engine task runs. Dropping it gives the slot back and leaves the line.
#[derive(Debug)]
pub struct SlotTicket {
    slots: Arc<Slots>,
    key: (u64, u64),
    holding: bool,
}

impl SlotTicket {
    pub fn new(slots: Arc<Slots>, rank: u64) -> Self {
        let key = (rank, slots.ticket());
        Self {
            slots,
            key,
            holding: false,
        }
    }

    pub fn holding(&self) -> bool {
        self.holding
    }

    pub fn slots(&self) -> &Arc<Slots> {
        &self.slots
    }

    /// True once this job holds a slot (taking one now if it may).
    pub fn poll(&mut self, paused: bool) -> bool {
        if !self.holding && self.slots.try_take(self.key, paused) {
            self.holding = true;
        }
        self.holding
    }

    pub fn release(&mut self) {
        if self.holding {
            self.holding = false;
            self.slots.give_back();
        } else {
            self.slots.forget(self.key);
        }
    }
}

impl Drop for SlotTicket {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_is_honoured_and_lowest_rank_goes_first() {
        let slots = Slots::new(1);
        let mut a = SlotTicket::new(Arc::clone(&slots), 1);
        let mut b = SlotTicket::new(Arc::clone(&slots), 3);
        let mut c = SlotTicket::new(Arc::clone(&slots), 2);
        assert!(a.poll(false));
        assert!(!b.poll(false));
        assert!(!c.poll(false));
        a.release();
        assert!(!b.poll(false), "rank 2 is ahead of rank 3");
        assert!(c.poll(false));
        assert_eq!(slots.active(), 1);
    }

    #[test]
    fn a_paused_waiter_does_not_block_the_line() {
        let slots = Slots::new(1);
        let mut running = SlotTicket::new(Arc::clone(&slots), 1);
        assert!(running.poll(false));
        let mut paused = SlotTicket::new(Arc::clone(&slots), 2);
        let mut next = SlotTicket::new(Arc::clone(&slots), 3);
        assert!(!paused.poll(false));
        assert!(!next.poll(false));
        assert!(!paused.poll(true), "pausing leaves the line");
        running.release();
        assert!(next.poll(false));
        assert!(!paused.poll(true));
    }

    #[test]
    fn raising_the_cap_starts_more_and_lowering_keeps_running_jobs() {
        let slots = Slots::new(1);
        let mut tickets: Vec<_> = (1..=3)
            .map(|rank| SlotTicket::new(Arc::clone(&slots), rank))
            .collect();
        let held: Vec<_> = tickets.iter_mut().map(|t| t.poll(false)).collect();
        assert_eq!(held, [true, false, false]);
        slots.set_cap(3);
        assert!(tickets[1].poll(false) && tickets[2].poll(false));
        slots.set_cap(1);
        assert_eq!(slots.active(), 3, "nothing is interrupted");
        tickets[0].release();
        tickets[1].release();
        let mut late = SlotTicket::new(Arc::clone(&slots), 9);
        assert!(!late.poll(false), "still one running against a cap of 1");
        tickets[2].release();
        assert!(late.poll(false));
    }

    #[test]
    fn a_hold_blocks_new_starts_until_lifted() {
        let slots = Slots::new(4);
        slots.hold();
        let mut job = SlotTicket::new(Arc::clone(&slots), 1);
        assert!(!job.poll(false));
        slots.lift();
        assert!(job.poll(false));
        slots.hold();
        assert!(job.poll(false), "a running job keeps its slot");
    }

    #[test]
    fn dropping_a_ticket_gives_back_and_leaves_the_line() {
        let slots = Slots::new(1);
        let mut first = SlotTicket::new(Arc::clone(&slots), 1);
        assert!(first.poll(false));
        let mut waiting = SlotTicket::new(Arc::clone(&slots), 2);
        assert!(!waiting.poll(false));
        drop(waiting);
        drop(first);
        assert_eq!(slots.active(), 0);
        let mut later = SlotTicket::new(Arc::clone(&slots), 5);
        assert!(later.poll(false), "the dropped waiter no longer blocks");
    }

    #[test]
    fn clamp_keeps_the_setting_in_range() {
        assert_eq!(clamp_max_downloads(0), 1);
        assert_eq!(clamp_max_downloads(5), 5);
        assert_eq!(clamp_max_downloads(99), MAX_DOWNLOADS_CAP);
        assert_eq!(Slots::new(0).cap(), 1);
    }
}

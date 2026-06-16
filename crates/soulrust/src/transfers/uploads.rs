//! The upload-candidate scheduler, a faithful port of Nicotine+'s
//! `uploads.py:_get_upload_candidate` and the `_user_update_counter` mechanics
//! that drive its round-robin / FIFO / privileged ordering. Pure — no I/O — so
//! the precise candidate sequences in `test_get_upload_candidate.py` can be
//! replayed as tests.
//!
//! Model: every transfer has an id; it is *queued*, then *active* once an upload
//! slot picks it up, then *finished* (removed). Round-robin fairness comes from
//! the per-user update counter: a user enqueuing more files keeps their place,
//! but finishing an upload re-stamps them to the back of the queue.

use std::collections::{HashMap, HashSet};

pub type TransferId = usize;

pub struct UploadQueue {
    fifo: bool,
    privileged: HashSet<String>,

    // id-indexed transfer fields
    usernames: Vec<String>,

    // queued state. A transfer's id is its global insertion order, so FIFO
    // ordering is recoverable from ids — no separate ordered list to keep in
    // sync (or to scan/shift on dequeue).
    queued_users: HashMap<String, Vec<TransferId>>, // per-user queued ids, insertion order

    // active state: username -> number of active uploads
    active_users: HashMap<String, usize>,

    // round-robin counters
    counter: u64,
    counters: HashMap<String, u64>,
}

impl UploadQueue {
    pub fn new(fifo: bool) -> Self {
        UploadQueue {
            fifo,
            privileged: HashSet::new(),
            usernames: Vec::new(),
            queued_users: HashMap::new(),
            active_users: HashMap::new(),
            counter: 0,
            counters: HashMap::new(),
        }
    }

    pub fn set_privileged<I, S>(&mut self, users: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.privileged = users.into_iter().map(Into::into).collect();
    }

    fn is_privileged(&self, username: &str) -> bool {
        self.privileged.contains(username)
    }

    pub fn username_of(&self, id: TransferId) -> &str {
        &self.usernames[id]
    }

    /// Number of transfers still tracked (queued + active) — when this reaches
    /// zero there is nothing left to schedule.
    pub fn len(&self) -> usize {
        let queued: usize = self.queued_users.values().map(Vec::len).sum();
        queued + self.active_users.values().sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The 1-based position of a still-queued transfer among all queued
    /// transfers, in insertion (FIFO) order — the value a downloader's
    /// `PlaceInQueueRequest` expects. Returns 0 if `id` is no longer queued
    /// (already activated, finished, or never queued), which a downloader reads
    /// as "not waiting".
    pub fn place_in_queue(&self, id: TransferId) -> usize {
        let still_queued = id < self.usernames.len()
            && self
                .queued_users
                .get(&self.usernames[id])
                .is_some_and(|ids| ids.contains(&id));
        if !still_queued {
            return 0;
        }
        // ids increase with insertion, so the FIFO rank is how many queued ids
        // precede this one.
        self.queued_users.values().flatten().filter(|&&other| other < id).count() + 1
    }

    /// Enqueue a new upload for `username`. Mirrors `_enqueue_transfer` followed
    /// by `_update_transfer`.
    pub fn enqueue(&mut self, username: &str) -> TransferId {
        let id = self.usernames.len();
        self.usernames.push(username.to_owned());
        self.queued_users.entry(username.to_owned()).or_default().push(id);
        // `_update_transfer`: a brand-new queued file for a user we have no
        // counter for stamps them in; an extra file for a known user does not
        // (so re-queuing doesn't push them back).
        if !self.counters.contains_key(username) {
            self.update_user_counter(username);
        }
        id
    }

    /// Activate a transfer that was never queued — used to set up in-progress
    /// uploads. Mirrors `_activate_transfer` (which pops the user's counter).
    pub fn activate_unqueued(&mut self, username: &str) {
        *self.active_users.entry(username.to_owned()).or_insert(0) += 1;
        self.counters.remove(username);
    }

    fn update_user_counter(&mut self, username: &str) {
        // Only users with queued files who are not currently active wait in the
        // round-robin; bump the global counter and record their place.
        if self.queued_users.contains_key(username) && !self.active_users.contains_key(username) {
            self.counter += 1;
            self.counters.insert(username.to_owned(), self.counter);
        }
    }

    /// Pick the next queued transfer to upload (round-robin: the queued file of
    /// the longest-waiting user; FIFO: the first queued file overall),
    /// preferring privileged users. Does not dequeue — the caller does.
    /// Returns `(candidate, has_active_uploads)`.
    pub fn get_upload_candidate(&self) -> (Option<TransferId>, bool) {
        let has_active = !self.active_users.is_empty();
        if self.counters.is_empty() {
            return (None, has_active);
        }

        let privileged_users: HashSet<&str> = self
            .counters
            .keys()
            .map(String::as_str)
            .filter(|u| self.is_privileged(u))
            .collect();
        let eligible = |u: &str| {
            (privileged_users.is_empty() || privileged_users.contains(u))
                && self.counters.contains_key(u)
        };

        let target: Option<String> = if self.fifo {
            // FIFO = the globally-earliest queued transfer of an eligible user.
            // Since ids increase with insertion, that's the eligible user whose
            // first queued id is smallest.
            self.queued_users
                .iter()
                .filter(|(u, _)| eligible(u))
                .filter_map(|(u, ids)| ids.first().map(|&id| (id, u)))
                .min_by_key(|(id, _)| *id)
                .map(|(_, u)| u.clone())
        } else {
            self.counters
                .iter()
                .filter(|(u, _)| eligible(u))
                .min_by_key(|(_, &t)| t)
                .map(|(u, _)| u.clone())
        };

        let candidate =
            target.and_then(|u| self.queued_users.get(&u).and_then(|ids| ids.first().copied()));
        (candidate, has_active)
    }

    /// Remove a transfer from the queue. Mirrors `_dequeue_transfer`: if it was
    /// the user's last queued file, their counter is dropped.
    pub fn dequeue(&mut self, id: TransferId) {
        let username = self.usernames[id].clone();
        if let Some(ids) = self.queued_users.get_mut(&username) {
            if let Some(pos) = ids.iter().position(|&i| i == id) {
                ids.remove(pos);
            }
            if ids.is_empty() {
                self.queued_users.remove(&username);
            }
        }
        if !self.queued_users.contains_key(&username) {
            self.counters.remove(&username);
        }
    }

    /// Mark a (just-dequeued) transfer active. Mirrors `_activate_transfer`.
    pub fn activate(&mut self, id: TransferId) {
        let username = self.usernames[id].clone();
        *self.active_users.entry(username.clone()).or_insert(0) += 1;
        self.counters.remove(&username);
    }

    /// Finish an active upload for `username` (Nicotine+'s `_finish_transfer` +
    /// clear): the slot frees, and if the user still has queued files they are
    /// re-stamped to the back of the round-robin.
    pub fn finish(&mut self, username: &str) {
        if let Some(count) = self.active_users.get_mut(username) {
            *count -= 1;
            if *count == 0 {
                self.active_users.remove(username);
            }
        }
        self.update_user_counter(username);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays Nicotine+'s `consume_transfers`: repeatedly pick a candidate,
    /// finishing one in-progress upload each round (before or after picking),
    /// until nothing is left. Returns the sequence of candidate usernames.
    fn consume(
        queue: &mut UploadQueue,
        in_progress: &mut Vec<String>,
        clear_first: bool,
    ) -> Vec<Option<String>> {
        let mut candidates = Vec::new();
        let mut none_count = 0;
        while !queue.is_empty() && none_count < 2 {
            if clear_first && !in_progress.is_empty() {
                let u = in_progress.remove(0);
                queue.finish(&u);
            }
            let (candidate, _has_active) = queue.get_upload_candidate();
            if !clear_first && !in_progress.is_empty() {
                let u = in_progress.remove(0);
                queue.finish(&u);
            }
            match candidate {
                None => {
                    none_count += 1;
                    candidates.push(None);
                }
                Some(id) => {
                    none_count = 0;
                    queue.dequeue(id);
                    let username = queue.username_of(id).to_owned();
                    candidates.push(Some(username.clone()));
                    in_progress.push(username);
                    queue.activate(id);
                }
            }
        }
        candidates
    }

    /// Ported from `test_get_upload_candidate.py::base_test`.
    fn base_test(
        queued: &[&str],
        in_progress: &[&str],
        expected: &[Option<&str>],
        round_robin: bool,
        clear_first: bool,
    ) {
        let mut queue = UploadQueue::new(!round_robin);
        queue.set_privileged(["puser1", "puser2"]);

        for &username in queued {
            queue.enqueue(username);
        }
        let mut in_prog: Vec<String> = Vec::new();
        for &username in in_progress {
            queue.activate_unqueued(username);
            in_prog.push(username.to_owned());
        }

        let got = consume(&mut queue, &mut in_prog, clear_first);
        let expected: Vec<Option<String>> =
            expected.iter().map(|o| o.map(str::to_owned)).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn round_robin_basic() {
        base_test(
            &["user1", "user1", "user2", "user2", "user3", "user3"],
            &[],
            &[Some("user1"), Some("user2"), Some("user3"), Some("user1"), Some("user2"), Some("user3"), None],
            true,
            false,
        );
    }

    #[test]
    fn round_robin_no_contention() {
        base_test(
            &["user1", "user1", "user2", "user2", "user3", "user3"],
            &[],
            &[Some("user1"), Some("user2"), Some("user3"), Some("user1"), Some("user2"), Some("user3"), None],
            true,
            true,
        );
    }

    #[test]
    fn round_robin_one_user() {
        base_test(&["user1", "user1"], &[], &[Some("user1"), None, Some("user1"), None], true, false);
    }

    #[test]
    fn round_robin_returning_user() {
        base_test(
            &["user1", "user1", "user2", "user2", "user2", "user3", "user3", "user3", "user1", "user1"],
            &[],
            &[
                Some("user1"), Some("user2"), Some("user3"), Some("user1"), Some("user2"),
                Some("user3"), Some("user1"), Some("user2"), Some("user3"), Some("user1"), None,
            ],
            true,
            false,
        );
    }

    #[test]
    fn round_robin_in_progress() {
        base_test(
            &["user1", "user1", "user2", "user2"],
            &["user1"],
            &[Some("user2"), Some("user1"), Some("user2"), Some("user1"), None],
            true,
            false,
        );
    }

    #[test]
    fn round_robin_privileged() {
        base_test(
            &["user1", "user2", "puser1", "puser1", "puser2"],
            &[],
            &[Some("puser1"), Some("puser2"), Some("puser1"), Some("user1"), Some("user2"), None],
            true,
            false,
        );
    }

    #[test]
    fn fifo_basic() {
        base_test(
            &["user1", "user1", "user2", "user2", "user3", "user3"],
            &[],
            &[
                Some("user1"), Some("user2"), Some("user1"), Some("user2"), Some("user3"),
                None, Some("user3"), None,
            ],
            false,
            false,
        );
    }

    #[test]
    fn fifo_no_contention() {
        base_test(
            &["user1", "user1", "user2", "user2", "user3", "user3"],
            &[],
            &[
                Some("user1"), Some("user1"), Some("user2"), Some("user2"), Some("user3"),
                Some("user3"), None,
            ],
            false,
            true,
        );
    }

    #[test]
    fn fifo_one_user() {
        base_test(&["user1", "user1"], &[], &[Some("user1"), None, Some("user1"), None], false, false);
    }

    #[test]
    fn fifo_returning_user() {
        base_test(
            &["user1", "user1", "user2", "user2", "user2", "user3", "user3", "user3", "user1", "user1"],
            &[],
            &[
                Some("user1"), Some("user2"), Some("user1"), Some("user2"), Some("user3"),
                Some("user2"), Some("user3"), Some("user1"), Some("user3"), Some("user1"), None,
            ],
            false,
            false,
        );
    }

    #[test]
    fn fifo_in_progress() {
        base_test(
            &["user1", "user1", "user2", "user2"],
            &["user1"],
            &[Some("user2"), Some("user1"), Some("user2"), Some("user1"), None],
            false,
            false,
        );
    }

    #[test]
    fn fifo_privileged() {
        base_test(
            &["user1", "user2", "puser1", "puser1", "puser2"],
            &[],
            &[Some("puser1"), Some("puser2"), Some("puser1"), Some("user1"), Some("user2"), None],
            false,
            false,
        );
    }

    #[test]
    fn place_in_queue_is_fifo_rank_among_queued() {
        let mut q = UploadQueue::new(false);
        let a = q.enqueue("alice"); // 1st queued
        let b = q.enqueue("bob"); // 2nd
        let c = q.enqueue("alice"); // 3rd

        assert_eq!(q.place_in_queue(a), 1);
        assert_eq!(q.place_in_queue(b), 2);
        assert_eq!(q.place_in_queue(c), 3);

        // Dequeueing the head shifts everyone behind it forward by one.
        q.dequeue(a);
        assert_eq!(q.place_in_queue(a), 0, "a is no longer queued");
        assert_eq!(q.place_in_queue(b), 1);
        assert_eq!(q.place_in_queue(c), 2);
    }

    #[test]
    fn place_in_queue_is_zero_for_unknown_or_active() {
        let mut q = UploadQueue::new(false);
        assert_eq!(q.place_in_queue(999), 0, "never-seen id");
        let a = q.enqueue("alice");
        q.dequeue(a);
        q.activate(a);
        assert_eq!(q.place_in_queue(a), 0, "active transfer is not queued");
    }
}

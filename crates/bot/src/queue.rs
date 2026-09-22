#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    /// When a track ends naturally, queue it again at the end.
    All,
    /// When a track ends naturally, play it again immediately.
    One,
}

impl RepeatMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(RepeatMode::Off),
            "all" | "queue" => Some(RepeatMode::All),
            "one" | "track" => Some(RepeatMode::One),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub tid: u64,
    pub title: String,
    pub artist: String,
    pub duration: i32,
    pub requestor: String,
    pub requestor_name: String,
    pub cover_url: String,
}

#[derive(Debug, Default)]
pub struct Queue {
    tracks: Vec<Track>,
    pos: usize,
    repeat: RepeatMode,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, track: Track) {
        self.tracks.push(track);
    }

    pub fn dequeue(&mut self) -> Option<Track> {
        if self.pos >= self.tracks.len() {
            return None;
        }
        let track = self.tracks[self.pos].clone();
        self.pos += 1;
        Some(track)
    }

    pub fn current(&self) -> Option<&Track> {
        if self.pos == 0 || self.pos > self.tracks.len() {
            return None;
        }
        self.tracks.get(self.pos - 1)
    }

    pub fn skip(&mut self) -> bool {
        if self.pos >= self.tracks.len() {
            return false;
        }
        self.pos += 1;
        true
    }

    pub fn total_len(&self) -> usize {
        self.tracks.len()
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.pos = 0;
        self.repeat = RepeatMode::Off;
    }

    pub fn len(&self) -> usize {
        self.tracks.len().saturating_sub(self.pos)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn list(&self) -> &[Track] {
        if self.pos >= self.tracks.len() {
            return &[];
        }
        &self.tracks[self.pos..]
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    /// A track just finished naturally: honour the repeat mode by queuing it
    /// again (immediately, for `One`, or at the tail, for `All`).
    pub fn on_track_ended(&mut self, track: &Track) {
        match self.repeat {
            RepeatMode::Off => {}
            RepeatMode::All => self.tracks.push(track.clone()),
            RepeatMode::One => self.tracks.insert(self.pos, track.clone()),
        }
    }

    /// Remove the nth pending track (1-based). Returns it if it existed.
    pub fn remove_pending(&mut self, n: usize) -> Option<Track> {
        if n == 0 {
            return None;
        }
        let idx = self.pos.checked_add(n - 1)?;
        if idx >= self.tracks.len() {
            return None;
        }
        let track = self.tracks.remove(idx);
        if idx < self.pos {
            self.pos -= 1;
        }
        Some(track)
    }

    /// Insert a track so it plays next, before the rest of the queue.
    pub fn insert_next(&mut self, track: Track) {
        self.tracks.insert(self.pos, track);
    }

    /// Move the nth pending track to the mth position (both 1-based).
    pub fn move_pending(&mut self, from: usize, to: usize) -> bool {
        if from == 0 || to == 0 || from == to {
            return from == to && from > 0 && self.list().len() >= from;
        }
        let count = self.list().len();
        if from > count || to > count {
            return false;
        }
        let src = self.pos + from - 1;
        let track = self.tracks.remove(src);
        let dst = self.pos + to - 1;
        self.tracks.insert(dst, track);
        true
    }

    /// Randomize the pending part of the queue (Fisher-Yates). The already
    /// played history keeps its order so `current()` still resolves.
    pub fn shuffle_pending(&mut self) {
        let mut state = seed_rng();
        for i in (self.pos + 1..self.tracks.len()).rev() {
            let j = self.pos + (splitmix64(&mut state) as usize % (i - self.pos + 1));
            self.tracks.swap(i, j);
        }
    }
}

fn seed_rng() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED);
    nanos ^ ((std::process::id() as u64) << 32)
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(tid: u64, title: &str, duration: i32) -> Track {
        Track {
            tid,
            title: title.to_owned(),
            artist: String::new(),
            duration,
            requestor: String::new(),
            requestor_name: String::new(),
            cover_url: String::new(),
        }
    }

    #[test]
    fn queue_add_and_len() {
        let mut q = Queue::new();
        assert_eq!(q.len(), 0);

        q.add(track(1, "A", 100));
        q.add(track(2, "B", 200));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn queue_next() {
        let mut q = Queue::new();
        q.add(track(1, "A", 100));
        q.add(track(2, "B", 200));

        let t1 = q.dequeue().unwrap();
        assert_eq!(t1.tid, 1);
        assert_eq!(q.len(), 1);

        let t2 = q.dequeue().unwrap();
        assert_eq!(t2.tid, 2);
        assert_eq!(q.len(), 0);

        assert!(q.dequeue().is_none());
    }

    #[test]
    fn queue_current() {
        let mut q = Queue::new();
        assert!(q.current().is_none());

        q.add(track(1, "A", 100));
        assert!(q.current().is_none());

        q.dequeue();
        assert_eq!(q.current().unwrap().tid, 1);
    }

    #[test]
    fn queue_skip() {
        let mut q = Queue::new();
        q.add(track(1, "A", 100));
        q.add(track(2, "B", 200));

        assert!(q.skip());
        assert_eq!(q.len(), 1);
        assert_eq!(q.dequeue().unwrap().tid, 2);
        assert!(!q.skip());
    }

    #[test]
    fn queue_clear() {
        let mut q = Queue::new();
        q.add(track(1, "A", 100));
        q.add(track(2, "B", 200));
        q.set_repeat(RepeatMode::All);
        q.clear();

        assert_eq!(q.len(), 0);
        assert!(q.dequeue().is_none());
        assert_eq!(q.repeat(), RepeatMode::Off);
    }

    #[test]
    fn queue_list() {
        let mut q = Queue::new();
        q.add(track(1, "A", 100));
        q.add(track(2, "B", 200));

        let list = q.list();
        assert_eq!(list.len(), 2);

        q.dequeue();
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].tid, 2);
    }

    #[test]
    fn queue_starts_empty() {
        let q = Queue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.total_len(), 0);
        assert!(q.list().is_empty());
        assert!(q.current().is_none());
        assert_eq!(q.repeat(), RepeatMode::Off);
    }

    #[test]
    fn queue_default_matches_new() {
        let a = Queue::default();
        let b = Queue::new();
        assert_eq!(a.total_len(), b.total_len());
        assert!(a.is_empty());
    }

    #[test]
    fn total_len_keeps_history_len_counts_pending() {
        let mut q = Queue::new();
        q.add(track(1, "A", 100));
        q.add(track(2, "B", 200));
        q.add(track(3, "C", 300));

        q.dequeue(); // plays A, B and C remain pending
        assert_eq!(q.total_len(), 3);
        assert_eq!(q.len(), 2);
        assert!(!q.is_empty());

        q.skip(); // drop B
        assert_eq!(q.total_len(), 3);
        assert_eq!(q.len(), 1);
        assert_eq!(q.current().unwrap().tid, 2); // B still reported as last position
    }

    #[test]
    fn queue_drains_to_empty() {
        let mut q = Queue::new();
        q.add(track(1, "A", 100));
        q.dequeue();

        assert!(q.is_empty());
        assert!(q.list().is_empty());
        assert!(q.dequeue().is_none());
        assert!(!q.skip());
        assert_eq!(q.len(), 0);
        assert_eq!(q.total_len(), 1); // history is still visible
    }

    #[test]
    fn skip_on_empty_queue_is_noop() {
        let mut q = Queue::new();
        assert!(!q.skip());
        assert!(!q.skip());
        assert!(q.is_empty());
    }

    #[test]
    fn clear_resets_history_and_position() {
        let mut q = Queue::new();
        q.add(track(1, "A", 100));
        q.dequeue();
        q.add(track(2, "B", 200));
        q.clear();

        assert!(q.is_empty());
        assert_eq!(q.total_len(), 0);
        assert!(q.current().is_none());
        assert!(q.list().is_empty());

        // The queue is usable again after a clear.
        q.add(track(3, "C", 300));
        assert_eq!(q.dequeue().unwrap().tid, 3);
    }

    #[test]
    fn add_preserves_insertion_order() {
        let mut q = Queue::new();
        for i in 0..5 {
            q.add(track(i, "t", 1));
        }
        let list = q.list();
        let tids: Vec<u64> = list.iter().map(|t| t.tid).collect();
        assert_eq!(tids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn remove_pending_removes_nth_and_keeps_current() {
        let mut q = Queue::new();
        for i in 1..=3 {
            q.add(track(i, "t", 10));
        }
        q.dequeue(); // 1 is now the played history

        assert_eq!(q.remove_pending(2).unwrap().tid, 3);
        let tids: Vec<u64> = q.list().iter().map(|t| t.tid).collect();
        assert_eq!(tids, vec![2]);
        assert_eq!(q.current().unwrap().tid, 1);
    }

    #[test]
    fn remove_pending_out_of_range_is_none() {
        let mut q = Queue::new();
        q.add(track(1, "A", 1));
        assert!(q.remove_pending(0).is_none());
        assert!(q.remove_pending(2).is_none());
        assert_eq!(q.list().len(), 1);
    }

    #[test]
    fn insert_next_plays_before_the_rest() {
        let mut q = Queue::new();
        q.add(track(2, "B", 1));
        q.add(track(3, "C", 1));
        q.insert_next(track(1, "A", 1));

        assert_eq!(q.dequeue().unwrap().tid, 1);
        assert_eq!(q.dequeue().unwrap().tid, 2);
    }

    #[test]
    fn move_pending_reorders_within_pending() {
        let mut q = Queue::new();
        for i in 1..=3 {
            q.add(track(i, "t", 10));
        }

        assert!(q.move_pending(3, 1));
        let tids: Vec<u64> = q.list().iter().map(|t| t.tid).collect();
        assert_eq!(tids, vec![3, 1, 2]);

        assert!(!q.move_pending(3, 4)); // out of range
        assert!(!q.move_pending(0, 1)); // invalid index
    }

    #[test]
    fn shuffle_pending_keeps_history_and_multiset() {
        let mut q = Queue::new();
        q.add(track(1, "A", 10));
        q.add(track(2, "B", 10));
        q.add(track(3, "C", 10));
        q.dequeue(); // 1 becomes history

        q.shuffle_pending();

        assert_eq!(q.current().unwrap().tid, 1);
        let mut tids: Vec<u64> = q.list().iter().map(|t| t.tid).collect();
        assert_eq!(tids.len(), 2);
        tids.sort();
        assert_eq!(tids, vec![2, 3]);
    }

    #[test]
    fn repeat_all_requeues_at_tail() {
        let mut q = Queue::new();
        q.add(track(1, "A", 10));
        let ended = q.dequeue().unwrap();
        q.set_repeat(RepeatMode::All);
        q.on_track_ended(&ended);

        assert_eq!(q.list().len(), 1);
        assert_eq!(q.list()[0].tid, 1);
    }

    #[test]
    fn repeat_one_replays_next() {
        let mut q = Queue::new();
        q.add(track(1, "A", 10));
        q.add(track(2, "B", 10));
        let ended = q.dequeue().unwrap();
        q.set_repeat(RepeatMode::One);
        q.on_track_ended(&ended);

        // Track 1 plays again before the rest of the queue.
        assert_eq!(q.dequeue().unwrap().tid, 1);
        assert_eq!(q.dequeue().unwrap().tid, 2);
    }

    #[test]
    fn repeat_off_is_noop_and_modes_roundtrip() {
        let mut q = Queue::new();
        q.add(track(1, "A", 10));
        let ended = q.dequeue().unwrap();
        q.on_track_ended(&ended);
        assert!(q.is_empty());

        assert_eq!(RepeatMode::parse("OFF"), Some(RepeatMode::Off));
        assert_eq!(RepeatMode::parse(" one "), Some(RepeatMode::One));
        assert_eq!(RepeatMode::parse("queue"), Some(RepeatMode::All));
        assert_eq!(RepeatMode::parse("bogus"), None);
        assert_eq!(RepeatMode::All.label(), "all");
        assert_eq!(RepeatMode::Off.cycle(), RepeatMode::All);
        assert_eq!(RepeatMode::One.cycle(), RepeatMode::Off);
    }

    #[test]
    fn splitmix64_produces_distinct_values() {
        let mut state = 1u64;
        let a = splitmix64(&mut state);
        let b = splitmix64(&mut state);
        assert_ne!(a, b);
    }
}

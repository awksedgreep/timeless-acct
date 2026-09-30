//! What a listener has heard and the collector has not yet taken.
//!
//! A queue here is empty nearly all of the time and has to hold a fork
//! storm when one comes. It takes memory as it fills and no sooner, and
//! holds no more than its limit: what arrives at a full queue is refused,
//! for the listener to count as lost.
//!
//! What is taken from it is kept in maps for a sweep or two, and those
//! give back the room they were given: see `settle`.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

pub struct Sender<T> {
    items: mpsc::Sender<T>,
    waiting: Arc<AtomicUsize>,
    limit: usize,
}

pub struct Receiver<T> {
    items: mpsc::Receiver<T>,
    waiting: Arc<AtomicUsize>,
}

pub fn queue<T>(limit: usize) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = mpsc::channel();
    let waiting = Arc::new(AtomicUsize::new(0));
    (
        Sender {
            items: sender,
            waiting: Arc::clone(&waiting),
            limit,
        },
        Receiver {
            items: receiver,
            waiting,
        },
    )
}

impl<T> Sender<T> {
    /// False if the queue is full, or there is no one left to take from it.
    pub fn offer(&self, item: T) -> bool {
        // One sender: nothing else adds between the look and the send.
        if self.waiting.load(Ordering::Relaxed) >= self.limit {
            return false;
        }
        if self.items.send(item).is_err() {
            return false;
        }
        self.waiting.fetch_add(1, Ordering::Relaxed);
        true
    }
}

impl<T> Receiver<T> {
    /// Everything offered since the last call.
    pub fn drain(&self) -> Vec<T> {
        let taken: Vec<T> = self.items.try_iter().collect();
        // What is taken was sent, and may not have been counted yet: the
        // count can fall behind for a moment, and must not go below none.
        let _ = self
            .waiting
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |waiting| {
                Some(waiting.saturating_sub(taken.len()))
            });
        taken
    }
}

/// Let go of the room a map was given for a burst that is over.
///
/// A map grows to hold what is put in it and keeps that size when it is
/// emptied. After 120,000 processes had started and ended in six seconds,
/// two maps of what was known of them held 48 MB between them, and a
/// few hundred entries.
pub fn settle<K: Eq + Hash, V>(map: &mut HashMap<K, V>) {
    if map.capacity() > ROOM_KEPT && map.capacity() / 4 > map.len() {
        map.shrink_to(2 * map.len().max(ROOM_KEPT / 2));
    }
}

/// Places a map may keep whatever it holds: what a busy host fills.
const ROOM_KEPT: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_gives_back_the_room_of_a_burst_and_keeps_what_it_holds() {
        let mut map: HashMap<u32, [u8; 64]> = (0..100_000).map(|n| (n, [0; 64])).collect();
        map.retain(|n, _| *n < 300);
        // Emptied, it has the room it had: less what the places of the
        // removed take until the map is next rebuilt.
        let grown = map.capacity();
        assert!(grown > 50_000, "{grown}");

        settle(&mut map);
        assert!(map.capacity() < grown / 8, "{}", map.capacity());
        assert!(map.capacity() >= ROOM_KEPT);
        assert_eq!(map.len(), 300);
        assert!((0..300).all(|n| map.contains_key(&n)));

        // And is left alone where there was no burst.
        let settled = map.capacity();
        settle(&mut map);
        assert_eq!(map.capacity(), settled);
    }

    #[test]
    fn a_full_queue_refuses_and_an_emptied_one_takes_again() {
        let (sender, receiver) = queue(3);
        assert!((0..3).all(|n| sender.offer(n)));
        assert!(!sender.offer(3));
        assert_eq!(receiver.drain(), [0, 1, 2]);
        assert!(receiver.drain().is_empty());

        assert!(sender.offer(4));
        assert_eq!(receiver.drain(), [4]);
    }

    #[test]
    fn a_queue_no_one_takes_from_refuses() {
        let (sender, receiver) = queue(3);
        drop(receiver);
        assert!(!sender.offer(1));
    }

    #[test]
    fn what_is_offered_from_another_thread_is_all_taken() {
        let (sender, receiver) = queue(100_000);
        let offering = std::thread::spawn(move || (0..50_000).filter(|n| sender.offer(*n)).count());
        let mut taken = Vec::new();
        while !offering.is_finished() {
            taken.extend(receiver.drain());
        }
        assert_eq!(offering.join().unwrap(), 50_000);
        taken.extend(receiver.drain());
        assert_eq!(taken, (0..50_000).collect::<Vec<_>>());
    }
}

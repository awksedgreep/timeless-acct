//! What a listener has heard and the collector has not yet taken.
//!
//! A queue here is empty nearly all of the time and has to hold a fork
//! storm when one comes. It takes memory as it fills and no sooner, and
//! holds no more than its limit: what arrives at a full queue is refused,
//! for the listener to count as lost.

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

#[cfg(test)]
mod tests {
    use super::*;

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

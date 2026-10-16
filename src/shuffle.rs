//! Shuffled playlist ordering.

use rand::seq::SliceRandom;
use rand::Rng;
use std::collections::VecDeque;

/// Hands out items in a random order that keeps reshuffling without repeating items too soon.
///
/// The queue starts as a random permutation of the items. Played items go to a list of recently
/// played items, and once the queue is down to a third of the original item count, that list is
/// shuffled and appended to the queue. An item therefore can't come up again until at least a
/// third of the list has played in between, while over time the order still mixes completely.
pub struct ShuffleQueue<T, R> {
    queue: VecDeque<T>,
    recent: Vec<T>,
    /// Queue length at which the recently played items are added back.
    refill_at: usize,
    rng: R,
}

impl<T, R: Rng> ShuffleQueue<T, R> {
    pub fn new(mut items: Vec<T>, mut rng: R) -> Self {
        items.shuffle(&mut rng);
        ShuffleQueue {
            refill_at: items.len() / 3,
            queue: items.into(),
            recent: Vec::new(),
            rng,
        }
    }

    /// Takes the next item from the queue. It goes back into rotation once passed to
    /// [`Self::played`]; otherwise it is dropped. Returns `None` once every item was dropped.
    pub fn pop(&mut self) -> Option<T> {
        if self.queue.len() <= self.refill_at {
            self.recent.shuffle(&mut self.rng);
            self.queue.extend(self.recent.drain(..));
        }
        self.queue.pop_front()
    }

    /// Marks an item from [`Self::pop`] as recently played.
    pub fn played(&mut self, item: T) {
        self.recent.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::ShuffleQueue;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::{HashMap, HashSet};

    fn play(queue: &mut ShuffleQueue<u32, StdRng>, count: usize) -> Vec<u32> {
        (0..count)
            .map(|_| {
                let item = queue.pop().unwrap();
                queue.played(item);
                item
            })
            .collect()
    }

    #[test]
    fn first_pass_plays_every_item_once() {
        let mut queue = ShuffleQueue::new((0..30).collect(), StdRng::seed_from_u64(1));
        let order = play(&mut queue, 30);
        assert_eq!(order.iter().collect::<HashSet<_>>().len(), 30);
        assert_ne!(order, (0..30).collect::<Vec<_>>());
    }

    #[test]
    fn items_do_not_repeat_within_a_third() {
        for count in [1, 2, 3, 4, 5, 10, 31, 100] {
            let mut queue = ShuffleQueue::new((0..count).collect(), StdRng::seed_from_u64(2));
            let mut last_played = HashMap::new();
            let order = play(&mut queue, 50 * count as usize);
            for (position, item) in order.into_iter().enumerate() {
                if let Some(previous) = last_played.insert(item, position) {
                    assert!(
                        position - previous > count as usize / 3,
                        "{} items: {} repeated after {}",
                        count,
                        item,
                        position - previous
                    );
                }
            }
            assert_eq!(last_played.len(), count as usize);
        }
    }

    #[test]
    fn order_keeps_changing() {
        let mut queue = ShuffleQueue::new((0..30).collect(), StdRng::seed_from_u64(3));
        play(&mut queue, 30);
        let passes: HashSet<Vec<u32>> = (0..10).map(|_| play(&mut queue, 30)).collect();
        assert_eq!(passes.len(), 10);
    }

    #[test]
    fn dropped_items_leave_rotation() {
        let mut queue = ShuffleQueue::new((0..9).collect(), StdRng::seed_from_u64(4));
        for _ in 0..100 {
            let item = queue.pop().unwrap();
            if item % 3 != 0 {
                queue.played(item);
            }
        }
        let remaining: HashSet<u32> = play(&mut queue, 60).into_iter().collect();
        assert_eq!(remaining, [1, 2, 4, 5, 7, 8].into_iter().collect());

        let mut queue = ShuffleQueue::new(vec![1, 2], StdRng::seed_from_u64(5));
        assert!(queue.pop().is_some());
        assert!(queue.pop().is_some());
        assert!(queue.pop().is_none());
    }
}

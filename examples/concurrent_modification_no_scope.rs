use std::{collections::HashSet, thread};

use ribs::Queue;

const NUM_PRODUCERS: usize = 2;
const NUM_CONSUMERS: usize = 1;
const NUM_ELEMENTS_PER_PRODUCER: usize = 2;

fn main() {
    let queue = Queue::new(NUM_ELEMENTS_PER_PRODUCER);

    let mut producers = vec![];
    let mut consumers = vec![];

    for start in 0..NUM_PRODUCERS {
        let queue = queue.clone();
        producers.push(thread::spawn(move || {
            let mut current_value = start;
            for _ in 0..NUM_ELEMENTS_PER_PRODUCER {
                let _ = queue.try_push(current_value);

                current_value = current_value.wrapping_add(NUM_PRODUCERS);
            }
        }));
    }

    for _ in 0..NUM_CONSUMERS {
        let queue = queue.clone();
        consumers.push(thread::spawn(move || {
            let mut values = vec![];
            for _ in 0..NUM_ELEMENTS_PER_PRODUCER {
                let Ok(value) = queue.try_pop() else {
                    continue;
                };

                values.push(value);
            }
            values
        }));
    }

    let mut all_values = vec![];
    for consumer in consumers {
        let Ok(some_values) = consumer.join() else {
            continue;
        };

        all_values.extend(some_values);
    }

    let unique_values = all_values.iter().copied().collect::<HashSet<_>>();
    assert_eq!(unique_values.len(), all_values.len());
    assert!(unique_values.len() <= NUM_PRODUCERS * NUM_ELEMENTS_PER_PRODUCER);
}

use std::{collections::HashSet, thread};

use ribs::Queue;

const NUM_PRODUCERS: usize = 2;
const NUM_CONSUMERS: usize = 2;
const NUM_ELEMENTS_PER_PRODUCER: usize = 4;

fn main() {
    let queue = Queue::new(NUM_ELEMENTS_PER_PRODUCER);

    let all_values = thread::scope(|s| {
        let mut producers = vec![];
        let mut consumers = vec![];

        for start in 0..NUM_PRODUCERS {
            let queue = &queue;
            producers.push(s.spawn(move || {
                let mut current_value = start;
                for _ in 0..NUM_ELEMENTS_PER_PRODUCER {
                    let _ = queue.push(current_value);

                    current_value = current_value.wrapping_add(NUM_PRODUCERS);
                }
            }));
        }

        for _ in 0..NUM_CONSUMERS {
            let queue = &queue;
            consumers.push(s.spawn(move || {
                let mut values = vec![];
                for _ in 0..NUM_ELEMENTS_PER_PRODUCER {
                    let Some(value) = queue.pop() else {
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

        all_values
    });

    let unique_values = all_values.iter().copied().collect::<HashSet<_>>();
    assert_eq!(unique_values.len(), all_values.len());
    assert!(unique_values.len() <= NUM_PRODUCERS * NUM_ELEMENTS_PER_PRODUCER);

    println!("Got out {} unique values!", all_values.len());
}

//! Multi-producer multi-consumer queue that allows sending and receiving from
//! different threads

use std::{
    fmt,
    mem::{self, MaybeUninit},
};

use crossbeam_utils::Backoff;

use crate::{
    debug_with::DebugWith,
    stubs::{Arc, AtomicUsize, Ordering, UnsafeCell},
};

/// A multi-producer multi-consumer queue that allows sending and receiving from
/// different threads
#[derive(Debug, Clone)]
pub struct Queue<T>(Arc<QueueInner<T>>);

impl<T> Queue<T> {
    /// Create a new queue that will be able to hold at least the specified
    /// number of elements.
    pub fn new(capacity: usize) -> Self {
        let params = QueueParameters::from_block_heuristic(capacity).unwrap();
        let inner = QueueInner::new(params);

        Self(Arc::new(inner))
    }

    /// Create a new queue with the specified number of blocks and block size.
    ///
    /// The total capacity of the queue will be `num_blocks * block_size` items.
    pub fn with_block_params(num_blocks: usize, block_size: usize) -> Self {
        let params = QueueParameters::from_block_parameters(num_blocks, block_size).unwrap();
        let inner = QueueInner::new(params);

        Self(Arc::new(inner))
    }

    /// Return the capacity of this queue.
    ///
    /// See [`Capacity`] docs for more details.
    pub fn capacity(&self) -> &Capacity {
        self.0.capacity()
    }

    /// Attempts to push an element into the queue.
    ///
    /// If the queue is full, the element is returned back as an error.
    pub fn push(&self, mut elem: T) -> Result<(), T> {
        let backoff = Backoff::new();

        loop {
            match self.try_push(elem) {
                Ok(()) => break Ok(()),
                Err(TryPushError::Busy(rejected)) => {
                    backoff.spin();
                    elem = rejected;
                },
                Err(TryPushError::Full(rejected)) => break Err(rejected),
            }
        }
    }

    /// Attempt to push a new item to the queue, failing if the queue is
    /// full or there is a concurrent operation interfering.
    pub fn try_push(&self, elem: T) -> Result<(), TryPushError<T>> {
        self.0.try_push(elem)
    }

    /// Attempts to pop an element from the queue.
    ///
    /// If the queue is empty, None is returned.
    pub fn pop(&self) -> Option<T> {
        let backoff = Backoff::new();

        loop {
            match self.try_pop() {
                Ok(v) => break Some(v),
                Err(TryPopError::Busy) => {
                    backoff.spin(); // should wait a little while since another
                                    // thread made progress
                },
                Err(TryPopError::Empty) => break None,
            }
        }
    }

    /// Attempt to pop an item from the queue, failing if the queue is empty
    /// or another operation is interfering.
    pub fn try_pop(&self) -> Result<T, TryPopError> {
        self.0.try_pop()
    }
}

struct QueueInner<T> {
    // This is the queue level variable that controls which block is the current one for
    // consumers (callers of `pop` or `peek`).
    //
    // This is also a "cached" head, the actual head is a "phantom" and can be ahead of
    // this value. The caching is to improve performance, since determining the "phantom"
    // head would involve reading all `block.allocated` values and using that to determine
    // the real value.
    consumer: PackedHead,
    // This is the queue level variable that controls which block is the current one for
    // producers (callers of `push` or `push_multiple`).
    //
    // This is also a "cached" head, the actual head is a "phantom" and can be ahead of
    // this value. The caching is to improve performance, since determining the "phantom"
    // head would involve reading all `block.allocated` values and using that to determine
    // the real value.
    producer: PackedHead,

    blocks: Box<[Block<T>]>,

    params: QueueParameters,
}

impl<T: fmt::Debug> fmt::Debug for QueueInner<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueInner")
            .field("consumer", &self.consumer.debug_with(&self.params))
            .field("producer", &self.producer.debug_with(&self.params))
            .field("blocks", &self.blocks.debug_with(&self.params))
            .field("params", &self.params)
            .finish()
    }
}

impl<T> QueueInner<T> {
    /// Create a new queue with the specified number of blocks and block size.
    ///
    /// The total capacity of the queue will be `num_blocks * block_size` items.
    pub fn new(params: QueueParameters) -> Self {
        let consumer = PackedHead::new(&params, 0, 0);
        let producer = PackedHead::new(&params, 0, 0);
        let blocks = {
            let mut blocks = Vec::with_capacity(params.num_blocks);
            let mut is_first_block = true;
            blocks.resize_with(params.num_blocks, || {
                let block = Block::new(&params, is_first_block);

                if is_first_block {
                    is_first_block = false;
                }

                block
            });
            debug_assert_eq!(blocks.len(), params.num_blocks);

            blocks.into_boxed_slice()
        };

        Self {
            consumer,
            producer,
            blocks,
            params,
        }
    }

    /// Return the capacity of this queue.
    ///
    /// See [`Capacity`] docs for more details.
    pub fn capacity(&self) -> &Capacity {
        &self.params.capacity
    }

    /// Attempt to push a new item to the queue, failing if the queue is
    /// full or there is a concurrent operation interfering.
    pub fn try_push(&self, elem: T) -> Result<(), TryPushError<T>> {
        if self.capacity().is_empty() {
            return Err(TryPushError::Full(elem));
        }

        'retry: loop {
            let (producer_head, block) = self.get_producer_head_and_block();

            match self.allocate_entry(block) {
                AllocateEntryResult::Allocated(entry) => {
                    self.commit_entry(entry, elem);
                    return Ok(());
                },
                AllocateEntryResult::BlockDone => {
                    match self.advance_producer_head_retry_new(producer_head) {
                        Err(AdvanceProducerHeadError::NoEntry) => {
                            return Err(TryPushError::Full(elem))
                        },
                        Err(AdvanceProducerHeadError::NotAvailable) => {
                            return Err(TryPushError::Busy(elem))
                        },
                        Ok(()) => {
                            continue 'retry;
                        },
                    }
                },
            }
        }
    }

    /// Load the producer head and return it and the [`Block`] it points to
    fn get_producer_head_and_block(&self) -> (UnpackedHead, &Block<T>) {
        let unpacked = self.producer.load(&self.params, {
            // TODO: relax ordering
            Ordering::SeqCst
        });

        (unpacked, &self.blocks[unpacked.index])
    }

    /// Attempt to allocate a new slot in the block for the data, returning
    /// [`AllocateEntryResult::BlockDone`] if the [`Block`] is full.
    fn allocate_entry<'b>(&self, block: &'b Block<T>) -> AllocateEntryResult<'b, T> {
        // Make an initial check on the block to see if there is space available
        // if not, immediately return BLOCK_DONE so that we can advance the producer
        // head
        let allocated_cursor = block.allocated.load(&self.params, {
            // TODO: relax ordering
            Ordering::SeqCst
        });
        if allocated_cursor.offset >= self.params.block_size {
            return AllocateEntryResult::BlockDone;
        }

        // Fetch and increment the allocated cursor to allocate a space, returning the
        // old version if the old value has an offset greater than the block
        // size, that means a concurrent producer(s) has already claimed the
        // last space in the block, and the producer head needs to advance.
        let old_allocated = block.allocated.fetch_add(&self.params, 1, {
            // TODO: relax ordering
            Ordering::SeqCst
        });
        if old_allocated.offset >= self.params.block_size {
            return AllocateEntryResult::BlockDone;
        }

        AllocateEntryResult::Allocated(EntryDescriptor {
            offset: old_allocated.offset,
            block,
        })
    }

    /// Write the new data to the [`Block`] at the given offset, then
    /// incremented the commited [`Cursor`].
    ///
    /// This operations marks the data as ready to read.
    fn commit_entry(&self, entry: EntryDescriptor<T>, elem: T) {
        #[cfg(not(loom))]
        let slot_ptr = entry.block.data[entry.offset].get();
        #[cfg(loom)]
        let slot_ptr = entry.block.data[entry.offset].get_mut();

        // SAFETY: When the `allocate_entry` method successfully returns an entry, that
        // means that this thread is the only writer to this slot. Since this
        // thread is the only writer, we can acquire a mutable reference knowing
        // that no other thread will attempt to write or read from this same slot at the
        // same time.
        //
        // Consuming writers (from `try_pop`) are also blocked out, since they guard
        // using the position of the `committed` offset.
        //
        // The existing value in the `UnsafeCell` will be `MaybeUninit::uninit()`, since
        // that is what `try_pop` writes into the cell, and what the blocks are
        // initialized with. This means that it is safe to write without first
        // droppping the contents of the cell
        unsafe {
            #[cfg(not(loom))]
            let slot = &mut *slot_ptr;
            #[cfg(loom)]
            let slot = &mut *slot_ptr.deref();

            let _ = slot.write(elem);
        }

        entry.block.committed.fetch_add(&self.params, 1, {
            // TODO: relax ordering
            Ordering::SeqCst
        });
    }

    /// Attempt to advance the current producer head to the next block, failing
    /// if the next block is full or if there is contention when attempting
    /// to increment.
    fn advance_producer_head_retry_new(
        &self,
        producer_head: UnpackedHead,
    ) -> Result<(), AdvanceProducerHeadError> {
        let next_block = &self.blocks[(producer_head.index + 1) % self.params.num_blocks];
        let consumed = next_block.consumed.load(&self.params, {
            // TODO: relax ordering
            Ordering::SeqCst
        });

        if (consumed.version < producer_head.version)
            || (consumed.version == producer_head.version
                && consumed.offset != self.params.block_size)
        {
            let reserved = next_block.reserved.load(&self.params, {
                // TODO: relax ordering
                Ordering::SeqCst
            });
            if reserved.offset == consumed.offset {
                return Err(AdvanceProducerHeadError::NoEntry);
            } else {
                return Err(AdvanceProducerHeadError::NotAvailable);
            }
        }

        next_block.committed.fetch_max(
            &self.params,
            UnpackedCursor {
                version: producer_head.version + 1,
                offset: 0,
            },
            {
                // TODO: relax ordering
                Ordering::SeqCst
            },
        );
        next_block.allocated.fetch_max(
            &self.params,
            UnpackedCursor {
                version: producer_head.version + 1,
                offset: 0,
            },
            {
                // TODO: relax ordering
                Ordering::SeqCst
            },
        );

        self.producer.fetch_max(
            &self.params,
            producer_head.increment_index(&self.params, 1),
            {
                // TODO: relax ordering
                Ordering::SeqCst
            },
        );

        Ok(())
    }

    /// Attempt to pop an item from the queue, failing if the queue is empty
    /// or another operation is interfering.
    pub fn try_pop(&self) -> Result<T, TryPopError> {
        if self.capacity().is_empty() {
            return Err(TryPopError::Empty);
        }

        'retry: loop {
            let (consumer_head, block) = self.get_consumer_head_and_block();

            match self.reserve_entry(block) {
                Ok(entry) => {
                    let elem = self.consume_entry_retry_new(entry);

                    return Ok(elem);
                },
                Err(ReserveEntryError::NoEntry) => return Err(TryPopError::Empty),
                Err(ReserveEntryError::NotAvailable) => return Err(TryPopError::Busy),
                Err(ReserveEntryError::BlockDone { .. }) => {
                    match self.advance_consumer_head_retry_new(consumer_head) {
                        AdvanceConsumerHeadResult::Empty => return Err(TryPopError::Empty),
                        AdvanceConsumerHeadResult::Success => continue 'retry,
                    }
                },
            }
        }
    }

    /// Remove all values from the queue, and return the internals to their
    /// default state.
    pub fn clear(&mut self) {
        if self.blocks.is_empty() {
            return;
        }

        for (index, block) in self.blocks.iter_mut().enumerate() {
            block.reset(&self.params, index == 0, 0);
        }

        self.producer.store(
            &self.params,
            UnpackedHead {
                version: 0,
                index: 0,
            },
            Ordering::Relaxed,
        );
        self.consumer.store(
            &self.params,
            UnpackedHead {
                version: 0,
                index: 0,
            },
            Ordering::Relaxed,
        );
    }

    /// Load the consumer head and return it and the [`Block`] it points to
    fn get_consumer_head_and_block(&self) -> (UnpackedHead, &Block<T>) {
        let consumer = self.consumer.load(&self.params, {
            // TODO: relax ordering
            Ordering::SeqCst
        });
        (consumer, &self.blocks[consumer.index])
    }

    /// Attempt to reserve an entry in the given block for a consumer
    fn reserve_entry<'b>(
        &self,
        block: &'b Block<T>,
    ) -> Result<EntryDescriptor<'b, T>, ReserveEntryError> {
        'retry: loop {
            let reserved = block.reserved.load(&self.params, {
                // TODO: relax ordering
                Ordering::SeqCst
            });
            // If the reserved cursor still has some space in the block, continue attempting
            // to reserver otherwise, this block is empty
            if reserved.offset < self.params.block_size {
                let committed = block.committed.load(&self.params, {
                    // TODO: relax ordering
                    Ordering::SeqCst
                });
                // If the reserved and committed are equal, that means there are no items in
                // this block
                if reserved.offset == committed.offset {
                    return Err(ReserveEntryError::NoEntry);
                }

                // If the committed offset is before the end of the block and the allocated
                // offset is not equal that means there is a concurrent producer
                // operation and the consumer needs to retry
                if committed.offset != self.params.block_size {
                    let allocated = block.allocated.load(&self.params, {
                        // TODO: relax ordering
                        Ordering::SeqCst
                    });
                    if allocated.offset != committed.offset {
                        return Err(ReserveEntryError::NotAvailable);
                    }
                }

                let new_reserved = reserved.add(&self.params, 1);
                let old_reserved = block.reserved.fetch_max(&self.params, new_reserved, {
                    // TODO: relax ordering
                    Ordering::SeqCst
                });

                // If the reserved value is still the same after this entire method, that means
                // there was no concurrent consumers that interfered and the
                // reserved update completed successfully. otherwise, we need to
                // retry the whole process again
                if old_reserved == reserved {
                    return Ok(EntryDescriptor {
                        offset: reserved.offset,
                        block,
                    });
                } else {
                    continue 'retry;
                }
            }

            return Err(ReserveEntryError::BlockDone {
                _version: reserved.version,
            });
        }
    }

    fn consume_entry_retry_new(&self, entry: EntryDescriptor<T>) -> T {
        let data = unsafe {
            // SAFETY: When the `reserve_entry` method successfully returns an entry, that
            // means that this thread is the only writer to this slot. Since this
            // thread is the only writer, we can acquire a mutable reference knowing
            // that no other thread will attempt to write or read from this same slot at the
            // same time.
            //
            // The existing value in the `UnsafeCell` will be replaced with
            // `MaybeUninit::uninit()`, so that the cell is read for the next
            // `try_push`.

            #[cfg(not(loom))]
            let data = {
                let data = &mut *entry.block.data[entry.offset].get();
                mem::replace(data, MaybeUninit::uninit())
            };
            #[cfg(loom)]
            let data = {
                let data = entry.block.data[entry.offset].get_mut();
                mem::replace(data.deref(), MaybeUninit::uninit())
            };

            MaybeUninit::assume_init(data)
        };

        entry.block.consumed.fetch_add(&self.params, 1, {
            // TODO: relax ordering
            Ordering::SeqCst
        });

        data
    }

    fn advance_consumer_head_retry_new(
        &self,
        consumer_head: UnpackedHead,
    ) -> AdvanceConsumerHeadResult {
        let next_block = &self.blocks[(consumer_head.index + 1) % self.params.num_blocks];
        let committed = next_block.committed.load(&self.params, {
            // TODO: relax ordering
            Ordering::SeqCst
        });

        if committed.version != (consumer_head.version + 1) {
            return AdvanceConsumerHeadResult::Empty;
        }

        let _ = next_block.consumed.fetch_max(
            &self.params,
            UnpackedCursor {
                version: consumer_head.version + 1,
                offset: 0,
            },
            {
                // TODO: relax ordering
                Ordering::SeqCst
            },
        );
        let _ = next_block.reserved.fetch_max(
            &self.params,
            UnpackedCursor {
                version: consumer_head.version + 1,
                offset: 0,
            },
            {
                // TODO: relax ordering
                Ordering::SeqCst
            },
        );

        let _ = self.consumer.fetch_max(
            &self.params,
            consumer_head.increment_index(&self.params, 1),
            {
                // TODO: relax ordering
                Ordering::SeqCst
            },
        );

        AdvanceConsumerHeadResult::Success
    }
}

/// SAFETY: The queue is safe to share across threads even though it has
/// internal mutability because access to the internal data is synchronized by
/// use of atomics according the algorithm described in "{BBQ}: A Block-based
/// Bounded Queue for Exchanging Data and Profiling."
///
/// The `T: Send` bound is required because an item inserted in the queue on one
/// thread could be removed from the queue in a different thread.
unsafe impl<T> Sync for QueueInner<T> where T: Send {}

impl<T> Drop for QueueInner<T> {
    fn drop(&mut self) {
        self.clear()
    }
}

#[derive(Debug)]
enum AdvanceConsumerHeadResult {
    Empty,
    Success,
}

#[derive(Debug)]
enum ReserveEntryError {
    NoEntry,
    NotAvailable,
    BlockDone { _version: usize },
}

#[derive(Debug)]
enum AllocateEntryResult<'b, T> {
    Allocated(EntryDescriptor<'b, T>),
    BlockDone,
}

#[derive(Debug)]
enum AdvanceProducerHeadError {
    NoEntry,
    NotAvailable,
}

#[derive(Debug)]
struct EntryDescriptor<'b, T> {
    offset: usize,
    block: &'b Block<T>,
}

/// The result of a [`Queue::try_push`] method call.
#[derive(Debug, Copy, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TryPushError<T> {
    /// Error that comes when attempting to push when the queue is full.
    ///
    /// This error contains the value that was provided in the push so
    /// the caller can recover it.
    #[error("The queue was full when attempting to push a new element")]
    Full(T),
    /// Error that happens when there is a concurrent operation that interfers
    /// with the push, requiring the caller to retry or wait, then retry.
    #[error("There is a concurrent `try_pop` call that is interfering with the `try_push` call")]
    Busy(T),
}

/// The result of a [`Queue::try_pop`] method call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TryPopError {
    /// Error that comes when attempting to read from a queue when it is empty.
    #[error("The queue was empty when attempting to read")]
    Empty,
    /// Error that happens when there is a concurrent operation that interfers
    /// with the pop, requiring the caller to retry or wait, then retry.
    #[error("There is a concurrent `try_push` call that is interfering with the `try_pop` call")]
    Busy,
}

/// The capacity of the [`Queue`] is not a single number because the capacity
/// can change based on the order of `push` and `pop` calls. The reason
/// is an invariant of the queue internals (from the [BBQ paper]):
///
/// > Producers have to ensure they advance `phead` only if the next block
/// > that has no unconsumed data
///
/// This means that if there is a partially consumed block, it can't be
/// filled with new data. In the worst case, there would only be a single
/// element in this partial block and would mean a decrease in capacity of
/// `block_size - 1`. An extreme example of this is where a single block
/// queue would be unable to write at all while there is unconsumed
/// data:
///
/// ```
/// # use ribs::sync::{Queue, TryPushError};
/// let q = Queue::with_block_params(1, 2);
///
/// q.try_push(1).unwrap();
/// q.try_push(2).unwrap();
///
/// // The queue is now full
/// assert_eq!(q.try_push(3).unwrap_err(), TryPushError::Full(3));
///
/// // Remove 1 element
/// assert_eq!(q.try_pop().unwrap(), 1);
///
/// // At this point the queue is still full because we can't "advance" (wrap
/// // around) the writer/producer head while there is still unconsumed data.
/// assert_eq!(q.try_push(3).unwrap_err(), TryPushError::Full(3));
///
/// // Remove 1 element
/// assert_eq!(q.try_pop().unwrap(), 2);
///
/// // Now we can start adding elements again
/// q.try_push(3).unwrap();
/// q.try_push(4).unwrap();
/// ```
///
/// [BBQ paper]: https://www.usenix.org/system/files/atc22-wang-jiawei.pdf
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Capacity {
    /// Upper bound on the maximum number of items a [`Queue`] can hold.
    pub upper: usize,
    /// Lower bound on the maximum number of items a [`Queue`] can hold.
    pub lower: usize,
}

impl Capacity {
    /// Return a value describing a [`Queue`] with no capacity.
    const fn empty() -> Self {
        Self { upper: 0, lower: 0 }
    }

    /// Return true if this represents a [`Queue`] with no capacity.
    const fn is_empty(&self) -> bool {
        self.upper == 0
    }
}

impl PartialEq<(usize, usize)> for &Capacity {
    fn eq(&self, other: &(usize, usize)) -> bool {
        self.lower == other.0 && self.upper == other.1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueParameters {
    capacity: Capacity,
    num_blocks: usize,
    block_size: usize,
    version_mask: usize,
    version_shift: u32,
    offset_mask: usize,
    // offset_shift is 0
    index_mask: usize,
    // head_index_shift is 0
}

#[derive(Debug, thiserror::Error)]
#[error("Intermediate results overflowed")]
struct Overflow;

impl QueueParameters {
    /// Calculate the block parameters using a heuristic for number of blocks
    /// based on the desired maximum capacity of the queue
    fn from_block_heuristic(min_capacity: usize) -> Result<Self, Overflow> {
        let num_blocks_log = (min_capacity.ilog2().div_ceil(4)).max(1);
        let num_blocks = 2usize.pow(num_blocks_log);
        let block_size = (min_capacity.div_ceil(num_blocks)).max(1);

        assert!(
            (block_size * num_blocks) >= min_capacity,
            "Expected ({} * {}) >= {}",
            block_size,
            num_blocks,
            min_capacity
        );

        // We add 1 to the number of blocks so that the minimum capacity is accurate
        let params = Self::from_block_parameters(num_blocks + 1, block_size);

        debug_assert!(
            params
                .as_ref()
                .map(|p| p.capacity.lower >= min_capacity)
                .unwrap_or(true),
            "{:?} {}",
            params,
            min_capacity
        );

        params
    }

    /// Based on the number of blocks and the block size, determine all the mask
    /// and shift parameters for the queue variables.
    ///
    /// If any calculation overflows, this method returns an error.
    const fn from_block_parameters(num_blocks: usize, block_size: usize) -> Result<Self, Overflow> {
        if num_blocks == 0 || block_size == 0 {
            return Ok(Self {
                num_blocks,
                block_size,
                capacity: Capacity::empty(),
                version_mask: 0,
                version_shift: 0,
                offset_mask: 0,
                index_mask: 0,
            });
        }

        let max_capacity = match num_blocks.checked_mul(block_size) {
            Some(val) => val,
            None => return Err(Overflow),
        };
        let index_bit_len = match Self::index_bit_length(num_blocks) {
            Some(val) => val,
            None => return Err(Overflow),
        };
        let offset_bit_len = match Self::offset_bit_length(block_size) {
            Some(val) => val,
            None => return Err(Overflow),
        };
        let version_bit_len = match Self::version_bit_length(index_bit_len, offset_bit_len) {
            Some(val) => val,
            None => return Err(Overflow),
        };

        let version_mask = match usize::MAX.checked_shr(version_bit_len) {
            Some(val) => !val,
            None => return Err(Overflow),
        };
        let version_shift = match usize::BITS.checked_sub(version_bit_len) {
            Some(val) => val,
            None => return Err(Overflow),
        };

        let offset_mask = match usize::MAX.checked_shl(offset_bit_len) {
            Some(val) => !val,
            None => return Err(Overflow),
        };
        let index_mask = match usize::MAX.checked_shl(index_bit_len) {
            Some(val) => !val,
            None => return Err(Overflow),
        };

        let min_capacity = match num_blocks.saturating_sub(1).checked_mul(block_size) {
            Some(val) => val,
            None => return Err(Overflow),
        };

        Ok(Self {
            num_blocks,
            block_size,
            capacity: Capacity {
                upper: max_capacity,
                lower: min_capacity,
            },
            version_mask,
            version_shift,
            offset_mask,
            index_mask,
        })
    }

    /// This method calculates the bit length of the `index` bit-segment inside
    /// of [`Head`] values.
    ///
    /// The `index` bit length is determined by the number of blocks in the
    /// queue.
    const fn index_bit_length(num_blocks: usize) -> Option<u32> {
        // use next_power_of_two to ensure that we have enough bit length to entirely
        // represent the num blocks
        match num_blocks.checked_next_power_of_two() {
            Some(val) => val.checked_ilog2(),
            None => None,
        }
    }

    /// This method calculates the bit length of the `offset` bit-segment inside
    /// of [`Cursor`] values.
    ///
    /// The `offset` bit length is determined by the number of items in a block.
    const fn offset_bit_length(block_size: usize) -> Option<u32> {
        // use next_power_of_two to ensure that we have enough bit length to entirely
        // represent the block size
        match block_size.checked_next_power_of_two() {
            Some(val) => match val.checked_ilog2() {
                Some(val) => Some(val + 1),
                None => None,
            },
            None => None,
        }
    }

    /// This method calculates the bit length of the `version` bit-segment
    /// inside of [`Cursor`] and [`Head`] values.
    ///
    /// The `version` bit length is determined by the number of items in a block
    /// and the number of blocks.
    const fn version_bit_length(index_bit_len: u32, offset_bit_len: u32) -> Option<u32> {
        let max_bit_len = if index_bit_len > offset_bit_len {
            index_bit_len
        } else {
            offset_bit_len
        };

        usize::BITS.checked_sub(max_bit_len)
    }

    const fn check_offset_in_range(&self, offset: usize) -> bool {
        (offset & self.offset_mask) == offset
    }

    const fn check_index_in_range(&self, index: usize) -> bool {
        (index & self.index_mask) == index
    }

    const fn check_version_in_range(&self, version: usize) -> bool {
        let packed_version = (version << self.version_shift) & self.version_mask;
        let unpacked_version = (packed_version & self.version_mask) >> self.version_shift;

        unpacked_version == version
    }
}

#[derive(Debug)]
struct Block<T> {
    data: Box<[UnsafeCell<MaybeUninit<T>>]>,
    allocated: PackedCursor,
    committed: PackedCursor,
    reserved: PackedCursor,
    consumed: PackedCursor,
}

impl<T> Block<T> {
    fn new(params: &QueueParameters, is_first_block: bool) -> Self {
        let data = {
            let mut storage = Vec::<UnsafeCell<MaybeUninit<T>>>::with_capacity(params.block_size);
            storage.resize_with(params.block_size, || UnsafeCell::new(MaybeUninit::uninit()));
            debug_assert_eq!(storage.len(), params.block_size);
            storage.into_boxed_slice()
        };

        let offset = if is_first_block { 0 } else { params.block_size };

        Block {
            data,
            allocated: PackedCursor::new(params, 0, offset),
            committed: PackedCursor::new(params, 0, offset),
            reserved: PackedCursor::new(params, 0, offset),
            consumed: PackedCursor::new(params, 0, offset),
        }
    }

    /// Overwrite the indices of this block back to their initial states with
    /// the given version and drop all data present in this block.
    fn reset(&mut self, params: &QueueParameters, is_first_block: bool, new_version: usize) {
        // we can use relaxed loads here because no other thread may have a mutable
        // reference, so we ensure single-ownership here

        let committed = self.committed.load(params, Ordering::Relaxed);
        let consumed = self.consumed.load(params, Ordering::Relaxed);

        let range = if committed.version > consumed.version {
            // we've wrapped around on this block, and the writer (committed) is a higher
            // version than the reader.
            0..committed.offset
        } else {
            consumed.offset..committed.offset
        };

        for elem in &mut self.data[range] {
            let elem = elem.get_mut();

            unsafe {
                // SAFETY: The elements between `committed` and `consumed` are guaranteed to be
                // initialized by the setup of `try_push` and `try_pop`.
                #[cfg(not(loom))]
                MaybeUninit::assume_init_drop(elem);
                #[cfg(loom)]
                MaybeUninit::assume_init_drop(elem.deref());
            }
        }

        let new_offset = if is_first_block { 0 } else { params.block_size };

        // Ordering::Relaxed all work here since we're assuming a single writer via the
        // `&mut self`
        self.allocated.store(
            params,
            UnpackedCursor {
                version: new_version,
                offset: new_offset,
            },
            Ordering::Relaxed,
        );
        self.committed.store(
            params,
            UnpackedCursor {
                version: new_version,
                offset: new_offset,
            },
            Ordering::Relaxed,
        );
        self.committed.store(
            params,
            UnpackedCursor {
                version: new_version,
                offset: new_offset,
            },
            Ordering::Relaxed,
        );
        self.committed.store(
            params,
            UnpackedCursor {
                version: new_version,
                offset: new_offset,
            },
            Ordering::Relaxed,
        );
    }
}

impl<T> DebugWith<QueueParameters> for Block<T> {
    fn fmt_with(
        &self,
        cx: &QueueParameters,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        fmt.debug_struct("Block")
            .field("allocated", &self.allocated.debug_with(cx))
            .field("committed", &self.committed.debug_with(cx))
            .field("reserved", &self.reserved.debug_with(cx))
            .field("consumed", &self.consumed.debug_with(cx))
            .finish()
    }
}

/// A [`Block`]-level variable that contains both a version and an index
/// component.
///
/// The version is used to prevent ABA problems when updated concurrently. The
/// index component is used to index into the array of blocks.
///
/// Layout for a block with 4 elements, the `v`s are the version bits, and the
/// `i`s are the index bits:
/// ```text
/// xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxiii
/// ```
#[derive(Debug, Default)]
struct PackedHead(AtomicUsize);

impl PackedHead {
    /// Created a new packed representation of the index over blocks with an
    /// associated version number.
    ///
    /// The [`QueueParameters`] reference is used to determine how many bits are
    /// required for the index into the block array.
    fn new(params: &QueueParameters, version: usize, index: usize) -> Self {
        assert!(params.check_version_in_range(version) && params.check_index_in_range(index));
        let raw_value =
            ((version << params.version_shift) & params.version_mask) | (index & params.index_mask);

        Self(AtomicUsize::new(raw_value))
    }

    /// Loads the packed header index value and unpacks it.
    ///
    /// `load` takes an [`Ordering`] argument which describes the memory
    /// ordering of this operation. Possible values are [`SeqCst`],
    /// [`Acquire`] and [`Relaxed`].
    ///
    /// # Panics
    ///
    /// Panics if `order` is [`Release`] or [`AcqRel`].
    fn load(&self, params: &QueueParameters, ordering: Ordering) -> UnpackedHead {
        let raw_value = self.0.load(ordering);
        let index = raw_value & params.index_mask;
        let version = (raw_value & params.version_mask) >> params.version_shift;

        UnpackedHead { version, index }
    }

    /// Packs the given header index value and stores it.
    ///
    /// `store` takes an [`Ordering`] argument which describes the memory
    /// ordering of this operation.  Possible values are [`SeqCst`],
    /// [`Release`] and [`Relaxed`].
    ///
    /// # Panics
    ///
    /// Panics if `order` is [`Acquire`] or [`AcqRel`].
    fn store(&self, params: &QueueParameters, value: UnpackedHead, ordering: Ordering) {
        let raw_value = ((value.version << params.version_shift) & params.version_mask)
            | (value.index & params.index_mask);

        self.0.store(raw_value, ordering)
    }

    /// Packs the given header index value, then finds the maximum of the
    /// current packed value and the other packed value, and sets the new value
    /// to the result.
    ///
    /// `fetch_max` takes an [`Ordering`] argument which describes the memory
    /// ordering of this operation. All ordering modes are possible. Note
    /// that using [`Acquire`] makes the store part of this operation
    /// [`Relaxed`], and using [`Release`] makes the load part [`Relaxed`].
    fn fetch_max(
        &self,
        params: &QueueParameters,
        other: UnpackedHead,
        ordering: Ordering,
    ) -> UnpackedHead {
        let raw_value = ((other.version << params.version_shift) & params.version_mask)
            | (other.index & params.index_mask);

        let old_value = self.0.fetch_max(raw_value, ordering);

        let index = old_value & params.index_mask;
        let version = (old_value & params.version_mask) >> params.version_shift;

        UnpackedHead { version, index }
    }
}

impl fmt::Binary for PackedHead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw_value = self.0.load(Ordering::Relaxed);
        <usize as fmt::Binary>::fmt(&raw_value, f)
    }
}

impl DebugWith<QueueParameters> for PackedHead {
    fn fmt_with(
        &self,
        cx: &QueueParameters,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let unpacked = self.load(cx, Ordering::Relaxed);
        fmt.debug_struct("PackedHeader")
            .field("version", &unpacked.version)
            .field("index", &unpacked.index)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnpackedHead {
    version: usize,
    index: usize,
}

impl UnpackedHead {
    /// Increment the index value, and overflow it into the version value if the
    /// new index value is greater than or equal to the number of blocks
    fn increment_index(self, params: &QueueParameters, increment: usize) -> Self {
        let index = (self.index + increment) % params.num_blocks;
        let version = self.version + ((self.index + increment) / params.num_blocks);

        Self { version, index }
    }
}

/// A [`Block`]-level variable that contains both a version and an offset
/// component.
///
/// The version is used to prevent ABA problems when updated concurrently. The
/// offset component is used to index into the data array in the block.
///
/// Layout for a block with 4 elements, the `v`s are the version bits, and the
/// `o`s are the offset bits:
/// ```text
/// xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxooo
/// ```
#[derive(Debug, Default)]
struct PackedCursor(AtomicUsize);

impl PackedCursor {
    /// Created a new packed representation of the index inside of a block with
    /// an associated version number.
    ///
    /// The [`QueueParameters`] reference is used to determine how many bits are
    /// required for the index.
    fn new(params: &QueueParameters, version: usize, offset: usize) -> Self {
        assert!(params.check_version_in_range(version) && params.check_offset_in_range(offset));

        let raw_value = ((version << params.version_shift) & params.version_mask)
            | (offset & params.offset_mask);

        Self(AtomicUsize::new(raw_value))
    }

    /// Loads the packed cursor index and unpacks it.
    ///
    /// `load` takes an [`Ordering`] argument which describes the memory
    /// ordering of this operation. Possible values are [`SeqCst`],
    /// [`Acquire`] and [`Relaxed`].
    ///
    /// # Panics
    ///
    /// Panics if `order` is [`Release`] or [`AcqRel`].
    fn load(&self, params: &QueueParameters, ordering: Ordering) -> UnpackedCursor {
        let raw_value = self.0.load(ordering);
        let offset = raw_value & params.offset_mask;
        let version = (raw_value & params.version_mask) >> params.version_shift;

        UnpackedCursor { version, offset }
    }

    /// Packs the given cursor index value and stores it.
    ///
    /// `store` takes an [`Ordering`] argument which describes the memory
    /// ordering of this operation.  Possible values are [`SeqCst`],
    /// [`Release`] and [`Relaxed`].
    ///
    /// # Panics
    ///
    /// Panics if `order` is [`Acquire`] or [`AcqRel`].
    fn store(&self, params: &QueueParameters, value: UnpackedCursor, ordering: Ordering) {
        let raw_value = ((value.version << params.version_shift) & params.version_mask)
            | (value.offset & params.offset_mask);

        self.0.store(raw_value, ordering)
    }

    /// Increments the current packed cursor index by the given increment, and
    /// returns the unpacked previous cursor index value.
    ///
    /// This operation wraps around on overflow.
    ///
    /// `fetch_add` takes an [`Ordering`] argument which describes the memory
    /// ordering of this operation. All ordering modes are possible. Note
    /// that using [`Acquire`] makes the store part of this operation
    /// [`Relaxed`], and using [`Release`] makes the load part [`Relaxed`].
    fn fetch_add(
        &self,
        params: &QueueParameters,
        increment: usize,
        ordering: Ordering,
    ) -> UnpackedCursor {
        let old_value = self.0.fetch_add(increment, ordering);
        let offset = old_value & params.offset_mask;
        let version = (old_value & params.version_mask) >> params.version_shift;

        UnpackedCursor { version, offset }
    }

    /// Packs the given cursor index value, then finds the maximum of the
    /// current packed value and the other packed value, and sets the new value
    /// to the result.
    ///
    /// `fetch_max` takes an [`Ordering`] argument which describes the memory
    /// ordering of this operation. All ordering modes are possible. Note
    /// that using [`Acquire`] makes the store part of this operation
    /// [`Relaxed`], and using [`Release`] makes the load part [`Relaxed`].
    fn fetch_max(
        &self,
        params: &QueueParameters,
        other: UnpackedCursor,
        ordering: Ordering,
    ) -> UnpackedCursor {
        let raw_value = ((other.version << params.version_shift) & params.version_mask)
            | (other.offset & params.offset_mask);

        let old_value = self.0.fetch_max(raw_value, ordering);

        let offset = old_value & params.offset_mask;
        let version = (old_value & params.version_mask) >> params.version_shift;

        UnpackedCursor { version, offset }
    }
}

impl fmt::Binary for PackedCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw_value = self.0.load(Ordering::Relaxed);
        <usize as fmt::Binary>::fmt(&raw_value, f)
    }
}

impl DebugWith<QueueParameters> for PackedCursor {
    fn fmt_with(
        &self,
        cx: &QueueParameters,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let unpacked = self.load(cx, Ordering::Relaxed);
        fmt.debug_struct("PackedCursor")
            .field("version", &unpacked.version)
            .field("offset", &unpacked.offset)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnpackedCursor {
    version: usize,
    offset: usize,
}

impl UnpackedCursor {
    fn add(self, params: &QueueParameters, increment: usize) -> Self {
        let mut raw_value = ((self.version << params.version_shift) & params.version_mask)
            | (self.offset & params.offset_mask);

        raw_value += increment;

        let offset = raw_value & params.offset_mask;
        let version = (raw_value & params.version_mask) >> params.version_shift;

        Self { version, offset }
    }
}

#[cfg(any(test, kani))]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    #[test]
    fn create_queue() {
        assert_eq!(Queue::<u8>::with_block_params(5, 5).capacity(), (20, 25));
        assert_eq!(Queue::<u8>::with_block_params(0, 5).capacity(), (0, 0));
        assert_eq!(Queue::<u8>::with_block_params(0, 0).capacity(), (0, 0));
        assert_eq!(Queue::<u8>::with_block_params(5, 0).capacity(), (0, 0));
    }

    const PUSH_AND_POP_PARAMS: [(usize, usize); 7] =
        [(5, 5), (1, 25), (25, 1), (8, 4), (4, 8), (1, 8), (8, 1)];

    #[test]
    fn try_push_and_try_pop_items_into_queues() {
        for (num_blocks, block_size) in PUSH_AND_POP_PARAMS {
            let q = Queue::with_block_params(num_blocks, block_size);

            try_push_to_full_then_try_pop_to_empty(&q);
            try_push_to_full_then_try_pop_to_empty(&q);
        }
    }

    #[test]
    fn push_and_pop_items_into_queues() {
        for (num_blocks, block_size) in PUSH_AND_POP_PARAMS {
            let q = Queue::with_block_params(num_blocks, block_size);

            push_to_full_then_pop_to_empty(&q);
            push_to_full_then_pop_to_empty(&q);
        }
    }

    #[test]
    fn push_and_pop_zero_sized_items_into_queue() {
        let q = Queue::with_block_params(5, 5);

        for _ in 0..q.capacity().upper {
            q.try_push(()).unwrap();
        }

        assert_eq!(q.try_push(()), Err(TryPushError::Full(())));

        for _ in 0..q.capacity().upper {
            q.try_pop().unwrap();
        }

        assert_eq!(q.try_pop(), Err(TryPopError::Empty));
    }

    fn try_push_to_full_then_try_pop_to_empty(q: &Queue<usize>) {
        for idx in 0..q.capacity().upper {
            q.try_push(idx).unwrap();
        }

        assert_eq!(
            q.try_push(q.capacity().upper),
            Err(TryPushError::Full(q.capacity().upper))
        );

        for expected_idx in 0..q.capacity().upper {
            let idx = q.try_pop().unwrap();
            assert_eq!(expected_idx, idx);
        }

        assert_eq!(q.try_pop(), Err(TryPopError::Empty));
    }

    fn push_to_full_then_pop_to_empty(q: &Queue<usize>) {
        for idx in 0..q.capacity().upper {
            q.push(idx).unwrap();
        }

        assert_eq!(q.push(q.capacity().upper), Err(q.capacity().upper));

        for expected_idx in 0..q.capacity().upper {
            let idx = q.pop().unwrap();
            assert_eq!(expected_idx, idx);
        }

        assert_eq!(q.pop(), None);
    }

    #[test]
    fn should_drop_contents() {
        #[derive(Debug)]
        struct DropCounter(Rc<RefCell<usize>>);

        impl Drop for DropCounter {
            fn drop(&mut self) {
                let mut borrow = self.0.borrow_mut();
                *borrow += 1;
            }
        }

        let counter = Rc::new(RefCell::new(0));

        let queue = Queue::with_block_params(5, 5);

        for _ in 0..5 {
            queue.try_push(DropCounter(Rc::clone(&counter))).unwrap();
        }

        for _ in 0..2 {
            let counter = queue.try_pop().unwrap();
            drop(counter);
        }

        assert_eq!(*counter.borrow(), 2);

        drop(queue);

        assert_eq!(*counter.borrow(), 5);
    }

    #[test]
    fn fill_queue_and_empty_partial() {
        let counter = Arc::new(AtomicUsize::new(0));
        let queue = Queue::with_block_params(2, 2);
        let capacity = 2 * 2;

        // State of queue:
        // | _ | _ || _ | _ |

        // fill the queue so that later pushes will wrap around
        for _ in 0..capacity {
            queue.try_push(DropCounter::new(&counter)).unwrap();
        }

        // State of queue:
        // | a | b || c | d |

        // queue is at maximum capacity
        assert!(matches!(
            queue.try_push(DropCounter::new(&counter)),
            Err(TryPushError::Full(_))
        ));
        assert_eq!(counter.load(Ordering::Relaxed), 4);

        // empty part of the queue so that we can push more
        for _ in 0..(3 * capacity / 4) {
            queue.try_pop().unwrap();
        }

        // State of queue:
        // | _ | _ || _ | d |

        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // fill the queue again, but so that the producer head is behind the consumer
        // head (in index space)
        //
        // The `- 1` on the end of the range is here because we can't entirely fill the
        // last block of the queue, while there are unconsumed items in that
        // block
        for _ in capacity..(capacity + (3 * capacity / 4) - 1) {
            queue.try_push(DropCounter::new(&counter)).unwrap();
        }

        // queue is at maximum capacity, which is less than expected
        assert!(matches!(
            queue.try_push(DropCounter::new(&counter)),
            Err(TryPushError::Full(_))
        ));

        // State of queue:
        // | e | f || _ | d |

        assert_eq!(counter.load(Ordering::Relaxed), 3);

        // remove 1/4 of the queue
        for _ in 0..(capacity / 4) {
            queue.try_pop().unwrap();
        }

        // State of queue:
        // | e | f || _ | _ |

        assert_eq!(counter.load(Ordering::Relaxed), 2);

        // fill now empty block of the queue
        for _ in 0..(capacity / 2) {
            queue.try_push(DropCounter::new(&counter)).unwrap();
        }

        // queue is at maximum capacity
        assert!(matches!(
            queue.try_push(DropCounter::new(&counter)),
            Err(TryPushError::Full(_))
        ));

        // State of queue:
        // | e | f || g | h |

        assert_eq!(counter.load(Ordering::Relaxed), 4);

        // make drop do cleanup
        drop(queue);

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[derive(Debug)]
    pub(crate) struct DropCounter(Arc<AtomicUsize>);

    impl DropCounter {
        pub(crate) fn new(counter: &Arc<AtomicUsize>) -> Self {
            let value = Self(Arc::clone(&counter));
            value.0.fetch_add(1, Ordering::Relaxed);
            value
        }
    }

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn drop_one_full_block_one_partial_block() {
        let counter = Arc::new(AtomicUsize::new(0));

        let q = Queue::with_block_params(2, 2);

        q.try_push(DropCounter::new(&counter)).unwrap();
        q.try_push(DropCounter::new(&counter)).unwrap();
        q.try_push(DropCounter::new(&counter)).unwrap();

        drop(q);

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}

#[cfg(test)]
mod loom_verification {
    use std::collections::HashSet;

    use super::*;

    use crate::stubs::{model, thread};

    fn test_producer<const N: usize, T>(queue: Queue<T>, elements: [T; N]) -> usize {
        let mut success_count = 0;
        for (index, elem) in elements.into_iter().enumerate() {
            let p1 = queue.try_push(elem);
            if index < queue.capacity().upper {
                assert!(!matches!(p1, Err(TryPushError::Full(_))));
            }

            if p1.is_ok() {
                success_count += 1;
            }
        }
        success_count
    }

    #[test]
    fn single_producer_single_consumer_observe_same_order() {
        model(|| {
            let q = Queue::with_block_params(4, 5);
            let t1 = thread::spawn({
                let q = q.clone();
                move || test_producer(q, [1, 2, 3])
            });

            let t2 = thread::spawn({
                let q = q.clone();
                move || {
                    let p1 = q.try_pop();
                    let p2 = q.try_pop();
                    let p3 = q.try_pop();

                    match (p1, p2, p3) {
                        (Ok(a), Ok(b), Ok(c)) => {
                            assert_eq!(a, 1);
                            assert_eq!(b, 2);
                            assert_eq!(c, 3);
                        },
                        (Ok(a), Ok(b), Err(_))
                        | (Ok(a), Err(_), Ok(b))
                        | (Err(_), Ok(a), Ok(b)) => {
                            assert!((a == 1 && b == 2) || (a == 2 && b == 3));
                        },
                        (Ok(a), Err(_), Err(_))
                        | (Err(_), Ok(a), Err(_))
                        | (Err(_), Err(_), Ok(a)) => {
                            assert!(a == 1 || a == 2 || a == 3);
                        },
                        (Err(_), Err(_), Err(_)) => {},
                    }
                }
            });

            t1.join().unwrap();
            t2.join().unwrap();
        })
    }

    fn test_multiple_consumer<const N: usize, T>(queue: Queue<T>, expected_elems: [T; N]) -> Vec<T>
    where
        T: PartialOrd + PartialEq,
    {
        let mut successes = Vec::with_capacity(N);
        for _ in 0..N {
            if let Ok(elem) = queue.try_pop() {
                successes.push(elem);
            }
        }

        // the received elements are in order
        assert!(successes.windows(2).all(|w| w[0] < w[1]));
        // every element received is from the expected list, and only occurs once from
        // the list NOTE: this requires that there are no duplicates in the
        // `expected_elems` array
        assert!(successes.iter().all(|elem| {
            expected_elems
                .iter()
                .filter(|expected_elem| *expected_elem == elem)
                .count()
                == 1
        }));

        successes
    }

    #[test]
    fn single_producer_multiple_consumer_observe_same_order() {
        model(|| {
            let q = Queue::with_block_params(4, 5);

            let t2 = thread::spawn({
                let q = q.clone();
                move || test_multiple_consumer(q, [1, 2, 3])
            });

            let t3 = thread::spawn({
                let q = q.clone();
                move || test_multiple_consumer(q, [1, 2, 3])
            });

            let t1 = thread::spawn({
                let q = q.clone();
                move || test_producer(q, [1, 2, 3])
            });

            t1.join().unwrap();
            let t2_elems = t2.join().unwrap().into_iter().collect::<HashSet<_>>();
            let t3_elems = t3.join().unwrap().into_iter().collect::<HashSet<_>>();

            assert!(t2_elems.is_disjoint(&t3_elems));
            let received_elems = t2_elems.union(&t3_elems).copied().collect::<HashSet<_>>();
            assert!(received_elems.is_subset(&[1, 2, 3].into_iter().collect::<HashSet<_>>()))
        })
    }

    #[test]
    fn multiple_producers_concurrent_with_multiple_consumers() {
        model(|| {
            let q = Queue::with_block_params(2, 1);
            let mut producer_threads = vec![];
            let mut consumer_threads = vec![];

            for i in 0..q.capacity().upper {
                let q = q.clone();
                producer_threads.push(thread::spawn(move || match q.try_push(i) {
                    Ok(_) => {},
                    Err(TryPushError::Busy(_)) => {},
                    Err(TryPushError::Full(i)) => {
                        panic!("Queue should not be full, failed to push [{i}]");
                    },
                }))
            }

            for _ in 0..q.capacity().upper {
                let q = q.clone();
                consumer_threads.push(thread::spawn(move || match q.try_pop() {
                    Ok(v) => Some(v),
                    Err(TryPopError::Busy) => q.try_pop().ok(),
                    Err(TryPopError::Empty) => None,
                }))
            }

            producer_threads.into_iter().for_each(|t| t.join().unwrap());

            let mut values = vec![];
            for t in consumer_threads {
                if let Some(v) = t.join().unwrap() {
                    values.push(v);
                }
            }

            values.sort();
            assert!(
                values == &[0, 1] || values == &[0] || values == &[1] || values == &[],
                "Values did not match: {:?}",
                values
            );
        })
    }

    #[test]
    fn multiple_producer_single_consumer_observe_same_order() {
        model(|| {
            let q = Queue::with_block_params(4, 5);
            let t1 = thread::spawn({
                let q = q.clone();
                move || test_producer(q, [('a', 1), ('a', 2), ('a', 3)])
            });

            let t2 = thread::spawn({
                let q = q.clone();
                move || test_producer(q, [('b', 1), ('b', 2), ('b', 3)])
            });

            let t3 = thread::spawn({
                let q = q.clone();
                move || {
                    let mut a_successes = Vec::with_capacity(3);
                    let mut b_successes = Vec::with_capacity(3);
                    for _ in 0..6 {
                        if let Ok((ident, elem)) = q.try_pop() {
                            if ident == 'a' {
                                a_successes.push(elem);
                            }

                            if ident == 'b' {
                                b_successes.push(elem);
                            }
                        }
                    }

                    // the received elements are in order
                    assert!(a_successes.windows(2).all(|w| w[0] < w[1]));
                    // every element received is from the expected list, and only occurs once from
                    // the list NOTE: this requires that there are no duplicates in the
                    // `expected_elems` array
                    assert!(a_successes.into_iter().all(|elem| {
                        [1, 2, 3]
                            .into_iter()
                            .filter(|expected_elem| *expected_elem == elem)
                            .count()
                            == 1
                    }));

                    // the received elements are in order
                    assert!(b_successes.windows(2).all(|w| w[0] < w[1]));
                    // every element received is from the expected list, and only occurs once from
                    // the list NOTE: this requires that there are no duplicates in the
                    // `expected_elems` array
                    assert!(b_successes.into_iter().all(|elem| {
                        [1, 2, 3]
                            .into_iter()
                            .filter(|expected_elem| *expected_elem == elem)
                            .count()
                            == 1
                    }));
                }
            });

            t1.join().unwrap();
            t2.join().unwrap();
            t3.join().unwrap();
        })
    }
}

#[cfg(kani)]
mod kani_verification {
    use super::*;

    #[kani::proof]
    fn round_trip_cursor_values() {
        let version: usize = kani::any();
        let offset: usize = kani::any();

        let params = QueueParameters::from_block_parameters(5, 5).unwrap();

        kani::assume(
            params.check_version_in_range(version) && params.check_offset_in_range(offset),
        );

        let packed = PackedCursor::new(&params, version, offset);
        let values = packed.load(&params, Ordering::Relaxed);

        assert_eq!(values.version, version);
        assert_eq!(values.offset, offset);
    }

    #[kani::proof]
    fn round_trip_head_values() {
        let version: usize = kani::any();
        let index: usize = kani::any();

        let params = QueueParameters::from_block_parameters(5, 5).unwrap();

        kani::assume(params.check_version_in_range(version) && params.check_index_in_range(index));

        let packed = PackedHead::new(&params, version, index);
        let values = packed.load(&params, Ordering::Relaxed);

        assert_eq!(values.version, version);
        assert_eq!(values.index, index);
    }

    #[kani::proof]
    fn construct_block_queue_parameters() {
        // this proof shows that we can construct a `QueueParameters` value or return an
        // error for any inputs (no panics)
        let num_blocks: usize = kani::any();
        let block_size: usize = kani::any();

        let _params = QueueParameters::from_block_parameters(num_blocks, block_size);
    }

    #[kani::proof]
    fn construct_block_queue_parameters_no_error_up_to_half_usize() {
        // this proof shows that for any values of `num_blocks` and `block_size` up to
        // the size of `usize::BITS / 2` we can construct a `QueueParameters` value
        // without errors
        let num_blocks: usize = kani::any();
        let block_size: usize = kani::any();

        kani::assume(
            num_blocks < (1 << usize::BITS - (usize::BITS / 2))
                && block_size < (1 << usize::BITS - (usize::BITS / 2)),
        );

        let _params = QueueParameters::from_block_parameters(num_blocks, block_size).unwrap();
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn drop_catches_all_data() {
        let num_push: usize = kani::any();
        let num_pop: usize = kani::any();

        kani::assume(num_push >= num_pop && num_push <= 4);

        let counter = Arc::new(AtomicUsize::new(0));

        let q = Queue::with_block_params(2, 2);

        for _ in 0..num_push {
            q.try_push(tests::DropCounter::new(&counter)).unwrap();
        }

        for _ in 0..num_pop {
            let _ = q.try_pop().unwrap();
        }

        assert_eq!(counter.load(Ordering::Relaxed), num_push - num_pop);

        drop(q);

        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}

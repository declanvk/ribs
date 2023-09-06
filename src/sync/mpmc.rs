//! Multi-producer multi-consumer queue that allows sending and receiving from different threads

use std::{
    cell::UnsafeCell,
    fmt,
    mem::{self, MaybeUninit},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crate::debug_with::DebugWith;

/// A multi-producer multi-consumer queue that allows sending and receiving from different threads
#[derive(Debug)]
pub struct Queue<T>(Arc<QueueInner<T>>);

impl<T> Queue<T> {
    /// Create a new queue that will be able to hold at least the specified number of
    /// elements.
    pub fn new(capacity: usize) -> Self {
        let params = QueueParameters::from_block_heuristic(capacity);
        let inner = QueueInner::new(params);

        Self(Arc::new(inner))
    }

    /// Create a new queue with the specified number of blocks and block size.
    ///
    /// The total capacity of the queue will be `num_blocks * block_size` items.
    pub fn with_block_params(num_blocks: usize, block_size: usize) -> Self {
        let params = QueueParameters::from_block_parameters(num_blocks, block_size);
        let inner = QueueInner::new(params);

        Self(Arc::new(inner))
    }

    /// Returns the maximum number of items that this queue can hold
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// Attempt to push a new item to the queue, failing if the queue is
    /// full or there is a concurrent operation interfering.
    pub fn try_push(&self, elem: T) -> Result<(), TryPushError<T>> {
        self.0.try_push(elem)
    }

    /// Attempt to pop an item from the queue, failing if the queue is empty
    /// or another operation is interfering.
    pub fn try_pop(&self) -> Result<T, TryPopError> {
        self.0.try_pop()
    }
}

#[derive(Debug)]
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

    /// Returns the maximum number of items that this queue can hold
    pub fn capacity(&self) -> usize {
        self.params.capacity
    }

    /// Attempt to push a new item to the queue, failing if the queue is
    /// full or there is a concurrent operation interfering.
    pub fn try_push(&self, elem: T) -> Result<(), TryPushError<T>> {
        if self.params.capacity == 0 {
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
                            return Err(TryPushError::Busy)
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
        let unpacked = self.producer.load(&self.params, Ordering::SeqCst);

        (unpacked, &self.blocks[unpacked.index])
    }

    /// Attempt to allocate a new slot in the block for the data, returning [`AllocateEntryResult::BlockDone`]
    /// if the [`Block`] is full.
    fn allocate_entry<'b>(&self, block: &'b Block<T>) -> AllocateEntryResult<'b, T> {
        // Make an initial check on the block to see if there is space available
        // if not, immediately return BLOCK_DONE so that we can advance the producer head
        let allocated_cursor = block.allocated.load(&self.params, Ordering::SeqCst);
        if allocated_cursor.offset >= self.params.block_size {
            return AllocateEntryResult::BlockDone;
        }

        // Fetch and increment the allocated cursor to allocate a space, returning the old version
        // if the old value has an offset greater than the block size, that means a concurrent producer(s)
        // has already claimed the last space in the block, and the producer head needs to advance.
        let old_allocated = block.allocated.fetch_add(&self.params, 1, Ordering::SeqCst);
        if old_allocated.offset >= self.params.block_size {
            return AllocateEntryResult::BlockDone;
        }

        AllocateEntryResult::Allocated(EntryDescriptor {
            offset: old_allocated.offset,
            block,
        })
    }

    /// Write the new data to the [`Block`] at the given offset, then incremented the commited [`Cursor`].
    ///
    /// This operations marks the data as ready to read.
    fn commit_entry(&self, entry: EntryDescriptor<T>, elem: T) {
        let slot_ptr = entry.block.data[entry.offset].get();
        // SAFETY: When the `allocate_entry` method successfully returns an entry, that means that this thread is the
        // only writer to this slot. Since this thread is the only writer, we can acquire a mutable reference knowing
        // that no other thread will attempt to write or read from this same slot at the same time.
        //
        // Consuming writers (from `try_pop`) are also blocked out, since they guard using the position of the
        // `committed` offset.
        //
        // The existing value in the `UnsafeCell` will be `MaybeUninit::uninit()`, since that is what `try_pop` writes
        // into the cell, and what the blocks are initialized with. This means that it is safe to write without first
        // droppping the contents of the cell
        unsafe {
            let slot = &mut *slot_ptr;
            let _ = slot.write(elem);
        }

        entry
            .block
            .committed
            .fetch_add(&self.params, 1, Ordering::SeqCst);
    }

    /// Attempt to advance the current producer head to the next block, failing if the next block
    /// is full or if there is contention when attempting to increment.
    fn advance_producer_head_retry_new(
        &self,
        producer_head: UnpackedHead,
    ) -> Result<(), AdvanceProducerHeadError> {
        let next_block = &self.blocks[(producer_head.index + 1) % self.params.num_blocks];
        let consumed = next_block.consumed.load(&self.params, Ordering::SeqCst);

        if (consumed.version < producer_head.version)
            || (consumed.version == producer_head.version
                && consumed.offset != self.params.block_size)
        {
            let reserved = next_block.reserved.load(&self.params, Ordering::SeqCst);
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
            Ordering::SeqCst,
        );
        next_block.allocated.fetch_max(
            &self.params,
            UnpackedCursor {
                version: producer_head.version + 1,
                offset: 0,
            },
            Ordering::SeqCst,
        );

        self.producer.fetch_max(
            &self.params,
            producer_head.increment_index(&self.params, 1),
            Ordering::SeqCst,
        );

        Ok(())
    }

    /// Attempt to pop an item from the queue, failing if the queue is empty
    /// or another operation is interfering.
    pub fn try_pop(&self) -> Result<T, TryPopError> {
        if self.params.capacity == 0 {
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

    /// Load the consumer head and return it and the [`Block`] it points to
    fn get_consumer_head_and_block(&self) -> (UnpackedHead, &Block<T>) {
        let consumer = self.consumer.load(&self.params, Ordering::SeqCst);
        (consumer, &self.blocks[consumer.index])
    }

    /// Attempt to reserve an entry in the given block for a consumer
    fn reserve_entry<'b>(
        &self,
        block: &'b Block<T>,
    ) -> Result<EntryDescriptor<'b, T>, ReserveEntryError> {
        'retry: loop {
            let reserved = block.reserved.load(&self.params, Ordering::SeqCst);
            // If the reserved cursor still has some space in the block, continue attempting to reserver
            // otherwise, this block is empty
            if reserved.offset < self.params.block_size {
                let committed = block.committed.load(&self.params, Ordering::SeqCst);
                // If the reserved and committed are equal, that means there are no items in this block
                if reserved.offset == committed.offset {
                    return Err(ReserveEntryError::NoEntry);
                }

                // If the committed offset is before the end of the block and the allocated offset is not equal
                // that means there is a concurrent producer operation and the consumer needs to retry
                if committed.offset != self.params.block_size {
                    let allocated = block.allocated.load(&self.params, Ordering::SeqCst);
                    if allocated.offset != committed.offset {
                        return Err(ReserveEntryError::NotAvailable);
                    }
                }

                let new_reserved = reserved.add(&self.params, 1);
                let old_reserved =
                    block
                        .reserved
                        .fetch_max(&self.params, new_reserved, Ordering::SeqCst);

                // If the reserved value is still the same after this entire method, that means there was no
                // concurrent consumers that interfered and the reserved update completed successfully.
                // otherwise, we need to retry the whole process again
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
                version: reserved.version,
            });
        }
    }

    fn consume_entry_retry_new(&self, entry: EntryDescriptor<T>) -> T {
        // SAFETY: When the `reserve_entry` method successfully returns an entry, that means that this thread is the
        // only writer to this slot. Since this thread is the only writer, we can acquire a mutable reference knowing
        // that no other thread will attempt to write or read from this same slot at the same time.
        //
        // The existing value in the `UnsafeCell` will be replaced with `MaybeUninit::uninit()`, so that the cell is
        // read for the next `try_push`.
        let data = unsafe {
            let data = &mut *entry.block.data[entry.offset].get();
            let data = mem::replace(data, MaybeUninit::uninit());
            MaybeUninit::assume_init(data)
        };

        entry
            .block
            .consumed
            .fetch_add(&self.params, 1, Ordering::SeqCst);

        data
    }

    fn advance_consumer_head_retry_new(
        &self,
        consumer_head: UnpackedHead,
    ) -> AdvanceConsumerHeadResult {
        let next_block = &self.blocks[(consumer_head.index + 1) % self.params.num_blocks];
        let committed = next_block.committed.load(&self.params, Ordering::SeqCst);

        if committed.version != (consumer_head.version + 1) {
            return AdvanceConsumerHeadResult::Empty;
        }

        let _ = next_block.consumed.fetch_max(
            &self.params,
            UnpackedCursor {
                version: consumer_head.version + 1,
                offset: 0,
            },
            Ordering::SeqCst,
        );
        let _ = next_block.reserved.fetch_max(
            &self.params,
            UnpackedCursor {
                version: consumer_head.version + 1,
                offset: 0,
            },
            Ordering::SeqCst,
        );

        let _ = self.consumer.fetch_max(
            &self.params,
            consumer_head.increment_index(&self.params, 1),
            Ordering::SeqCst,
        );

        AdvanceConsumerHeadResult::Success
    }
}

impl<T> DebugWith<QueueParameters> for QueueInner<T> {
    fn fmt_with(
        &self,
        cx: &QueueParameters,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        fmt.debug_struct("Queue")
            .field("consumer", &self.consumer.debug_with(cx))
            .field("producer", &self.producer.debug_with(cx))
            .field("blocks", &self.blocks.debug_with(cx))
            .finish()
    }
}

impl<T> Drop for QueueInner<T> {
    fn drop(&mut self) {
        // the &mut self guarantees that we have single-ownership of the `QueueInner`, meaning that we
        // should be able to call `try_pop` without any concurrent interference

        // OPTIMIZATION: For each block between producer and consumer, read out the entire
        // set of initialized values and drop them at once, rather than call `try_pop` for every single one

        loop {
            match self.try_pop() {
                Ok(val) => drop(val),
                Err(TryPopError::Busy) => panic!(
                    "Should not get an Busy result from `try_pop` when having &mut ownership"
                ),
                Err(TryPopError::Empty) => break,
            }
        }
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
    BlockDone { version: usize },
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
    /// Error that happens when there is a concurrent operation that interfers with
    /// the push, requiring the caller to retry or wait, then retry.
    #[error("There is a concurrent `try_pop` call that is interfering with the `try_push` call")]
    Busy,
}

/// The result of a [`Queue::try_pop`] method call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TryPopError {
    /// Error that comes when attempting to read from a queue when it is empty.
    #[error("The queue was empty when attempting to read")]
    Empty,
    /// Error that happens when there is a concurrent operation that interfers with
    /// the pop, requiring the caller to retry or wait, then retry.
    #[error("There is a concurrent `try_push` call that is interfering with the `try_pop` call")]
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueParameters {
    num_blocks: usize,
    block_size: usize,
    capacity: usize,
    version_mask: usize,
    version_shift: u32,
    offset_mask: usize,
    // offset_shift is 0
    index_mask: usize,
    // head_index_shift is 0
}

impl QueueParameters {
    /// Calculate the block parameters using a heuristic for number of blocks based on
    /// the desired minimum capacity of the queue
    fn from_block_heuristic(capacity: usize) -> Self {
        let num_blocks_log = (capacity.ilog2() / 4).max(1);
        let num_blocks = 2usize.pow(num_blocks_log);
        let block_size = (capacity / num_blocks).max(1);

        assert!((block_size * num_blocks) >= capacity);

        Self::from_block_parameters(num_blocks, block_size)
    }

    const fn from_block_parameters(num_blocks: usize, block_size: usize) -> Self {
        if num_blocks == 0 || block_size == 0 {
            return Self {
                num_blocks,
                block_size,
                capacity: 0,
                version_mask: 0,
                version_shift: 0,
                offset_mask: 0,
                index_mask: 0,
            };
        }

        let capacity = num_blocks * block_size;
        let index_bit_len = Self::index_bit_length(num_blocks);
        let offset_bit_len = Self::offset_bit_length(block_size);
        let version_bit_len = Self::version_bit_length(index_bit_len, offset_bit_len);

        let version_mask = !(usize::MAX >> version_bit_len);
        let version_shift = usize::BITS - version_bit_len;

        let offset_mask = !(usize::MAX << offset_bit_len);
        let index_mask = !(usize::MAX << index_bit_len);

        Self {
            num_blocks,
            block_size,
            capacity,
            version_mask,
            version_shift,
            offset_mask,
            index_mask,
        }
    }

    /// This method calculates the bit length of the `index` bit-segment inside of [`Head`] values.
    ///
    /// The `index` bit length is determined by the number of blocks in the queue.
    const fn index_bit_length(num_blocks: usize) -> u32 {
        // use next_power_of_two to ensure that we have enough bit length to entirely represent the num blocks
        num_blocks.next_power_of_two().ilog2()
    }

    /// This method calculates the bit length of the `offset` bit-segment inside of [`Cursor`] values.
    ///
    /// The `offset` bit length is determined by the number of items in a block.
    const fn offset_bit_length(block_size: usize) -> u32 {
        // use next_power_of_two to ensure that we have enough bit length to entirely represent the block size
        block_size.next_power_of_two().ilog2() + 1
    }

    /// This method calculates the bit length of the `version` bit-segment inside of [`Cursor`] and [`Head`] values.
    ///
    /// The `version` bit length is determined by the number of items in a block and the number of blocks.
    const fn version_bit_length(index_bit_len: u32, offset_bit_len: u32) -> u32 {
        let max_bit_len = if index_bit_len > offset_bit_len {
            index_bit_len
        } else {
            offset_bit_len
        };

        usize::BITS - max_bit_len
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

/// A [`Block`]-level variable that contains both a version and an index component.
///
/// The version is used to prevent ABA problems when updated concurrently. The index
/// component is used to index into the array of blocks.
///
/// Layout for a block with 4 elements, the `v`s are the version bits, and the `i`s
/// are the index bits:
/// ```text
/// xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxiii
/// ```
#[derive(Debug, Default)]
struct PackedHead(AtomicUsize);

impl PackedHead {
    fn new(params: &QueueParameters, version: usize, index: usize) -> Self {
        assert!(params.check_version_in_range(version) && params.check_index_in_range(index));
        let raw_value =
            ((version << params.version_shift) & params.version_mask) | (index & params.index_mask);

        Self(AtomicUsize::new(raw_value))
    }

    fn load(&self, params: &QueueParameters, ordering: Ordering) -> UnpackedHead {
        let raw_value = self.0.load(ordering);
        let index = raw_value & params.index_mask;
        let version = (raw_value & params.version_mask) >> params.version_shift;

        UnpackedHead { version, index }
    }

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
    /// Increment the index value, and overflow it into the version value if the new index value is
    /// greater than or equal to the number of blocks
    fn increment_index(self, params: &QueueParameters, increment: usize) -> Self {
        let index = (self.index + increment) % params.num_blocks;
        let version = self.version + ((self.index + increment) / params.num_blocks);

        Self { version, index }
    }
}

/// A [`Block`]-level variable that contains both a version and an offset component.
///
/// The version is used to prevent ABA problems when updated concurrently. The offset
/// component is used to index into the data array in the block.
///
/// Layout for a block with 4 elements, the `v`s are the version bits, and the `o`s
/// are the offset bits:
/// ```text
/// xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxooo
/// ```
#[derive(Debug, Default)]
struct PackedCursor(AtomicUsize);

impl PackedCursor {
    fn new(params: &QueueParameters, version: usize, offset: usize) -> Self {
        assert!(params.check_version_in_range(version) && params.check_offset_in_range(offset));

        let raw_value = ((version << params.version_shift) & params.version_mask)
            | (offset & params.offset_mask);

        Self(AtomicUsize::new(raw_value))
    }

    fn load(&self, params: &QueueParameters, ordering: Ordering) -> UnpackedCursor {
        let raw_value = self.0.load(ordering);
        let offset = raw_value & params.offset_mask;
        let version = (raw_value & params.version_mask) >> params.version_shift;

        UnpackedCursor { version, offset }
    }

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

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    #[test]
    fn create_queue() {
        assert_eq!(Queue::<u8>::with_block_params(5, 5).capacity(), 25);
        assert_eq!(Queue::<u8>::with_block_params(0, 5).capacity(), 0);
        assert_eq!(Queue::<u8>::with_block_params(0, 0).capacity(), 0);
        assert_eq!(Queue::<u8>::with_block_params(5, 0).capacity(), 0);
    }

    const PUSH_AND_POP_PARAMS: &[(usize, usize)] =
        &[(5, 5), (1, 25), (25, 1), (8, 4), (4, 8), (1, 8), (8, 1)];

    #[test]
    fn push_and_pop_items_into_queus() {
        for (num_blocks, block_size) in PUSH_AND_POP_PARAMS {
            let q = Queue::with_block_params(*num_blocks, *block_size);

            push_to_full_then_pop_to_empty(&q);
            push_to_full_then_pop_to_empty(&q);
        }
    }

    #[test]
    fn push_and_pop_zero_sized_items_into_queue() {
        let q = Queue::with_block_params(5, 5);

        for _ in 0..q.capacity() {
            q.try_push(()).unwrap();
        }

        assert_eq!(q.try_push(()), Err(TryPushError::Full(())));

        for _ in 0..q.capacity() {
            q.try_pop().unwrap();
        }

        assert_eq!(q.try_pop(), Err(TryPopError::Empty));
    }

    fn push_to_full_then_pop_to_empty(q: &Queue<usize>) {
        for idx in 0..q.capacity() {
            q.try_push(idx).unwrap();
        }

        assert_eq!(
            q.try_push(q.capacity()),
            Err(TryPushError::Full(q.capacity()))
        );

        for expected_idx in 0..q.capacity() {
            let idx = q.try_pop().unwrap();
            assert_eq!(expected_idx, idx);
        }

        assert_eq!(q.try_pop(), Err(TryPopError::Empty));
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
}

#[cfg(kani)]
mod kani_verification {
    use super::*;

    #[kani::proof]
    fn round_trip_cursor_values() {
        let version: usize = kani::any();
        let offset: usize = kani::any();

        let params = QueueParameters::from_block_parameters(5, 5);

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

        let params = QueueParameters::from_block_parameters(5, 5);

        kani::assume(params.check_version_in_range(version) && params.check_index_in_range(index));

        let packed = PackedHead::new(&params, version, index);
        let values = packed.load(&params, Ordering::Relaxed);

        assert_eq!(values.version, version);
        assert_eq!(values.index, index);
    }

    #[kani::proof]
    fn construct_heuristic_queue_parameters() {
        let capacity: usize = kani::any();

        let _params = QueueParameters::from_block_heuristic(capacity);
    }
}

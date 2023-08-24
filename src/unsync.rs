//! Single-threaded versions of queues

use std::{
    hash::Hash,
    iter::FusedIterator,
    mem::{self, MaybeUninit},
    slice,
};

/// Error that comes when attempting to [`Queue::push`] when the queue is full.
///
/// This error contains the value that was provided in the [`Queue::push`] so
/// the caller can recover it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The queue was full when attempting to push a new element")]
pub struct Full<T>(pub T);

/// The result of a [`Queue::push_multiple`] method call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PushMultipleError<E, IT> {
    /// The queue was full, so the container was returned
    #[error("The queue was full when attempting to push multiple new element")]
    Full(E),
    /// The queue did not have enough capacity to add all elements
    ///
    /// This is distinct from the `Full` case because the `push_multiple` method
    /// doesn't know how many elements there are to be added until it turn the
    /// `E` argument into an `ExactSizeIterator`. The `Full` case allows for
    /// returning the original value without having converted it to an iterator.
    #[error("The queue had insufficient capacity to add all the items")]
    InsufficientCapacity(IT),
}

/// Error that comes when attempting to read from a queue when it is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The queue was empty when attempting to read")]
pub struct Empty;

/// A fixed-capacity queue implemented using a ringbuffer as storage
#[derive(Debug)]
pub struct Queue<T> {
    storage: Box<[MaybeUninit<T>]>,
    // This field points at the slot in the `storage` array that will hold the next pushed item
    producer: usize,
    // This field points at the slot in the `storage` array that contains the next item to pop
    consumer: usize,
    // ---------------------------------------
    //                producer
    //                   ▲
    //               ┌───────┬───────┬───────┐
    //               │       │       │       │
    // empty queue   │ empty │ empty │ empty │
    //               │       │       │       │
    //               └───────┴───────┴───────┘
    //                   ▼
    //                consumer
    //
    //
    //                        producer
    //                           ▲
    //               ┌───────┬───────┬───────┐
    //               │       │       │       │
    // push 'H' to   │   H   │ empty │ empty │
    // queue         │       │       │       │
    //               └───────┴───────┴───────┘
    //                   ▼
    //                consumer
    //
    //
    //                                producer
    //                                   ▲
    //               ┌───────┬───────┬───────┐
    //               │       │       │       │
    // push 'E' to   │   H   │   E   │ empty │
    // queue         │       │       │       │
    // the queue     └───────┴───────┴───────┘
    // is now full       ▼
    //                consumer
    // ---------------------------------------
    //
    // We keep the extra space in the queue so that we can differentiate between empty and full,
    // otherwise the conditions would be the same (i.e `producer == consumer`).
    //
    // Empty condition is `producer == consumer`
    // Full condition is `(producer + 1) % (storage length) == consumer`
    //
    // The capacity of the queue is `(storage length) - 1`
}

impl<T> Queue<T> {
    /// Allocate a new [`Queue`] with the given fixed capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::Queue;
    /// let q = Queue::<u8>::new(10);
    ///
    /// assert_eq!(q.capacity(), 10);
    /// assert_eq!(q.len(), 0);
    /// assert!(q.is_empty());
    /// assert!(!q.is_full());
    /// ```
    pub fn new(capacity: usize) -> Self {
        let mut storage = Vec::<MaybeUninit<T>>::with_capacity(capacity + 1);
        storage.resize_with(capacity + 1, MaybeUninit::uninit);
        debug_assert_eq!(storage.len(), capacity + 1);
        let storage = storage.into_boxed_slice();

        Queue {
            storage,
            producer: 0,
            consumer: 0,
        }
    }

    /// Return the maximum number of elements this queue can hold
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::Queue;
    /// let q = Queue::<u8>::new(10);
    ///
    /// assert_eq!(q.capacity(), 10);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.storage.len() - 1
    }

    /// Return the current number of elements in this queue
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::Queue;
    /// let mut q = Queue::new(4);
    ///
    /// assert_eq!(q.len(), 0);
    ///
    /// q.push(10)?;
    /// q.push(20)?;
    ///
    /// assert_eq!(q.len(), 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn len(&self) -> usize {
        if self.producer >= self.consumer {
            self.producer - self.consumer
        } else {
            self.producer + (self.storage.len() - self.consumer)
        }
    }

    /// Return true if there are no elements in this queue
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::Queue;
    /// let q = Queue::<u8>::new(4);
    ///
    /// assert!(q.is_empty());
    /// ```
    ///
    /// The [`is_empty`] and [`is_full`] methods may behavior
    /// counter-intuitively when the queue is 0-length.
    ///
    /// ```
    /// use ribs::unsync::Queue;
    /// let q = Queue::<u8>::new(0);
    ///
    /// assert!(q.is_full());
    /// assert!(q.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.producer == self.consumer
    }

    /// Return true if there are the maximum number of elements in the queue
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::Queue;
    /// let mut q = Queue::<u8>::new(5);
    ///
    /// q.push_multiple([0, 1, 2, 3, 4])?;
    ///
    /// assert!(!q.is_empty());
    /// assert!(q.is_full());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// The [`is_empty`] and [`is_full`] methods may behavior
    /// counter-intuitively when the queue is 0-length.
    ///
    /// ```
    /// use ribs::unsync::Queue;
    /// let q = Queue::<u8>::new(0);
    ///
    /// assert!(q.is_full());
    /// assert!(q.is_empty());
    /// ```
    pub fn is_full(&self) -> bool {
        (self.producer + 1) % self.storage.len() == self.consumer
    }

    /// Attempt to push a new element into the queue, returning it back to
    /// the caller if the queue is full.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue, Full};
    /// let mut q = Queue::<u8>::new(3);
    ///
    /// q.push(0)?;
    /// q.push(1)?;
    /// q.push(2)?;
    /// assert!(matches!(q.push(3), Err(Full(3))));
    ///
    /// assert!(q.is_full());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn push(&mut self, elem: T) -> Result<(), Full<T>> {
        if self.is_full() {
            return Result::Err(Full(elem));
        }

        let _ = mem::replace(&mut self.storage[self.producer], MaybeUninit::new(elem));
        self.producer = self.next_producer();

        Ok(())
    }

    /// Push a collection of elements into the queue, returning the iterator
    /// back if the elements would not fit into the maximum queue capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue, PushMultipleError};
    /// let mut q = Queue::new(4);
    /// assert_eq!(q.len(), 0);
    ///
    /// q.push_multiple([1, 2, 3, 4])?;
    /// assert_eq!(q.len(), 4);
    /// assert!(q.is_full());
    ///
    /// assert!(matches!(q.push_multiple([5, 6]), Err(PushMultipleError::Full([5, 6]))));
    ///
    /// q.clear();
    /// assert!(q.is_empty());
    /// assert!(matches!(q.push_multiple([7, 8, 9, 10, 11]), Err(PushMultipleError::InsufficientCapacity(_))));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn push_multiple<E, IT>(&mut self, elems: E) -> Result<(), PushMultipleError<E, IT>>
    where
        E: IntoIterator<Item = T, IntoIter = IT>,
        IT: ExactSizeIterator<Item = T>,
    {
        if self.is_full() {
            return Err(PushMultipleError::Full(elems));
        }

        let iter = elems.into_iter();
        let required_capacity = iter.len();
        let capacity = self.capacity();
        let storage_len = self.storage.len();

        if self.len() + required_capacity > capacity {
            return Err(PushMultipleError::InsufficientCapacity(iter));
        }

        let iter = iter.enumerate().map(|(idx, elem)| {
            let idx = (idx + self.producer) % storage_len;

            (idx, elem)
        });

        for (idx, new_elem) in iter {
            let _ = mem::replace(&mut self.storage[idx], MaybeUninit::new(new_elem));
        }
        self.producer = (self.producer + required_capacity) % storage_len;

        Ok(())
    }

    /// Attempt to pop an element from the queue, returning nothing if the
    /// queue is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue, Empty};
    /// let mut q = Queue::new(4);
    /// assert_eq!(q.len(), 0);
    ///
    /// q.push_multiple([1, 2, 3, 4])?;
    ///
    /// assert_eq!(q.pop()?, 1);
    /// assert_eq!(q.pop()?, 2);
    /// assert_eq!(q.pop()?, 3);
    /// assert_eq!(q.pop()?, 4);
    /// assert_eq!(q.pop(), Err(Empty));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn pop(&mut self) -> Result<T, Empty> {
        if self.is_empty() {
            return Err(Empty);
        }

        let elem = mem::replace(&mut self.storage[self.consumer], MaybeUninit::uninit());
        // SAFETY: This is safe to assume initialized because of the invariants of the
        // ringbuffer. The `consumer` index should always point to an initialized value
        // (except if the queue is empty)
        let elem = unsafe { MaybeUninit::assume_init(elem) };
        self.consumer = self.next_consumer();

        Ok(elem)
    }

    /// Attempt to return an immutable reference to the first element in the
    /// queue, without popping it out. The method returns nothing if the
    /// queue is empty.
    ///
    /// Attempt to pop an element from the queue, returning nothing if the
    /// queue is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue, Empty};
    /// let mut q = Queue::new(4);
    /// assert_eq!(q.len(), 0);
    ///
    /// q.push_multiple([1, 2, 3])?;
    ///
    /// assert_eq!(q.peek()?, &1);
    /// assert_eq!(q.pop()?, 1);
    /// assert_eq!(q.pop()?, 2);
    /// assert_eq!(q.peek()?, &3);
    /// assert_eq!(q.pop()?, 3);
    /// assert_eq!(q.peek(), Err(Empty));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn peek(&self) -> Result<&T, Empty> {
        if self.is_empty() {
            return Err(Empty);
        }

        // SAFETY: This is safe to assume initialized because of the invariants of the
        // ringbuffer. The `consumer` index should always point to an initialized value
        // (except if the queue is empty)
        let elem = unsafe { MaybeUninit::assume_init_ref(&self.storage[self.consumer]) };

        Ok(elem)
    }

    /// Attempt to return an mutable reference to the first element in the
    /// queue, without popping it out. The method returns nothing if the
    /// queue is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue, Empty};
    /// let mut q = Queue::new(3);
    /// assert_eq!(q.len(), 0);
    ///
    /// q.push_multiple([1, 2, 3])?;
    ///
    /// assert_eq!(q.peek_mut()?, &mut 1);
    /// assert_eq!(q.pop()?, 1);
    /// assert_eq!(q.pop()?, 2);
    /// assert_eq!(q.peek_mut()?, &mut 3);
    /// assert_eq!(q.pop()?, 3);
    /// assert_eq!(q.peek_mut(), Err(Empty));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn peek_mut(&mut self) -> Result<&mut T, Empty> {
        if self.is_empty() {
            return Err(Empty);
        }

        // SAFETY: This is safe to assume initialized because of the invariants of the
        // ringbuffer. The `consumer` index should always point to an initialized value
        // (except if the queue is empty)
        let elem = unsafe { MaybeUninit::assume_init_mut(&mut self.storage[self.consumer]) };

        Ok(elem)
    }

    /// Clears the queue, removing all values.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue};
    /// let mut q = Queue::new(3);
    /// assert_eq!(q.len(), 0);
    ///
    /// assert!(matches!(q.push_multiple([1, 2, 3]), Ok(())));
    /// assert_eq!(q.len(), 3);
    /// assert!(q.is_full() && !q.is_empty());
    ///
    /// q.clear();
    /// assert_eq!(q.len(), 0);
    /// assert!(!q.is_full() && q.is_empty());
    /// ```
    pub fn clear(&mut self) {
        if self.is_empty() {
            return;
        }

        let index_iter = if self.producer > self.consumer {
            (self.consumer..self.producer).chain(0..0)
        } else {
            (self.consumer..self.storage.len()).chain(0..self.producer)
        };

        for index in index_iter {
            // SAFETY: The range of indices are all already initialized because we only read
            // between the bounds of `consumer..producer` (accounting for the wrap around
            // the array end)
            unsafe { MaybeUninit::assume_init_drop(&mut self.storage[index]) }
        }

        self.producer = 0;
        self.consumer = 0;
    }

    /// Gets an immutable iterator over the elements in the queue starting from
    /// the beginning and going to the end.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue};
    /// let mut q = Queue::new(3);
    /// assert_eq!(q.len(), 0);
    ///
    /// assert!(matches!(q.push_multiple([1, 2, 3]), Ok(())));
    ///
    /// let mut iter = q.iter();
    ///
    /// assert_eq!(iter.next(), Some(&1));
    /// assert_eq!(iter.next(), Some(&2));
    /// assert_eq!(iter.next(), Some(&3));
    /// assert_eq!(iter.next(), None);
    /// ```
    pub fn iter(&self) -> Iter<'_, T> {
        <&Queue<T> as IntoIterator>::into_iter(self)
    }

    /// Gets a mutable iterator over the elements in the queue starting from
    /// the beginning and going to the end.
    ///
    /// # Examples
    ///
    /// ```
    /// use ribs::unsync::{Queue};
    /// let mut q = Queue::new(3);
    /// assert_eq!(q.len(), 0);
    ///
    /// assert!(matches!(q.push_multiple([1, 2, 3]), Ok(())));
    ///
    /// let mut iter = q.iter_mut();
    ///
    /// assert_eq!(iter.next(), Some(&mut 1));
    /// assert_eq!(iter.next(), Some(&mut 2));
    /// assert_eq!(iter.next(), Some(&mut 3));
    /// assert_eq!(iter.next(), None);
    /// ```
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        <&mut Queue<T> as IntoIterator>::into_iter(self)
    }

    #[inline]
    fn next_producer(&self) -> usize {
        (self.producer + 1) % self.storage.len()
    }

    #[inline]
    fn next_consumer(&self) -> usize {
        (self.consumer + 1) % self.storage.len()
    }

    #[inline]
    fn as_slices(&self) -> (&[T], &[T]) {
        if self.is_empty() {
            (&[], &[])
        } else if self.producer > self.consumer {
            let left = &self.storage[self.consumer..self.producer];
            (
                // SAFETY: This is safe to assume initialized because of the invariants of the
                // ringbuffer.
                //
                // The `consumer` index should always point to an initialized value (except if the
                // queue is empty) and the `producer` index always points to where the next `push`
                // value will go.
                //
                // In this case, the ringbuffer occupies a continuous range and doesn't wrap around
                // the end of the array (since `producer` > `consumer` and we know the queue is not
                // empty)
                unsafe { crate::polyfill::maybe_uninit_slice_assume_init_ref(left) },
                &[],
            )
        } else {
            let left = &self.storage[0..self.producer];
            let right = &self.storage[self.consumer..];

            // SAFETY: This is safe to assume initialized because of the invariants of the
            // ringbuffer.
            //
            // The `consumer` index should always point to an initialized value (except if
            // the queue is empty) and the `producer` index always points to
            // where the next `push` value will go.
            //
            // In this case, the ringbuffer is wrapped around the end of the array (since
            // `producer` < `consumer`). We know that the start of the array is initialize
            // (`[0, producer)`) and the end of the array is also initialized (`[consumer,
            // array.len())`).
            unsafe {
                (
                    crate::polyfill::maybe_uninit_slice_assume_init_ref(left),
                    crate::polyfill::maybe_uninit_slice_assume_init_ref(right),
                )
            }
        }
    }

    #[inline]
    fn as_slices_mut(&mut self) -> (&mut [T], &mut [T]) {
        if self.is_empty() {
            (&mut [], &mut [])
        } else if self.producer > self.consumer {
            let left = &mut self.storage[self.consumer..self.producer];
            (
                // SAFETY: See `as_slices` for the explanation for this branch
                unsafe { crate::polyfill::maybe_uninit_slice_assume_init_mut(left) },
                &mut [],
            )
        } else {
            let (left, rest) = self.storage.split_at_mut(self.producer);
            let (_, right) = rest.split_at_mut(self.consumer);

            // SAFETY: See `as_slices` for the explanation for this branch
            unsafe {
                (
                    crate::polyfill::maybe_uninit_slice_assume_init_mut(left),
                    crate::polyfill::maybe_uninit_slice_assume_init_mut(right),
                )
            }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        self.clear()
    }
}

impl<T> From<Vec<T>> for Queue<T> {
    fn from(value: Vec<T>) -> Self {
        let capacity = value.len();
        let mut q = Queue::<T>::new(capacity);

        debug_assert!(
            matches!(q.push_multiple(value), Ok(())),
            "The queue was created with enough capacity, it should succeed in pushing all values"
        );

        q
    }
}

impl<const N: usize, T> From<[T; N]> for Queue<T> {
    fn from(value: [T; N]) -> Self {
        let mut q = Queue::<T>::new(N);

        debug_assert!(
            matches!(q.push_multiple(value), Ok(())),
            "The queue was created with enough capacity, it should succeed in pushing all values"
        );

        q
    }
}

impl<T> Extend<T> for Queue<T> {
    /// Extends a collection with the contents of an iterator.
    ///
    /// # Panics
    ///
    /// Panics if the queue does not have enough capacity to hold all elements
    /// of the iterator
    fn extend<IT: IntoIterator<Item = T>>(&mut self, iter: IT) {
        let original_len = self.len();
        let capacity = self.capacity();
        for (idx, elem) in iter.into_iter().enumerate() {
            let num_elements = idx + 1;
            match self.push(elem) {
                Ok(()) => {},
                Err(Full(_)) => {
                    panic!(
                        "Unable to push [{num_elements}] into the queue, the original length was \
                         [{original_len}] and the capacity was [{capacity}]"
                    )
                },
            }
        }
    }
}

impl<A, B> PartialEq<Queue<B>> for Queue<A>
where
    A: PartialEq<B>,
{
    fn eq(&self, other: &Queue<B>) -> bool {
        self.iter().eq(other.iter())
    }
}

impl<T> Eq for Queue<T> where T: Eq {}

impl<A, B> PartialOrd<Queue<B>> for Queue<A>
where
    A: PartialOrd<B>,
{
    fn partial_cmp(&self, other: &Queue<B>) -> Option<std::cmp::Ordering> {
        self.iter().partial_cmp(other.iter())
    }
}

impl<T> Ord for Queue<T>
where
    T: Ord,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.iter().cmp(other.iter())
    }
}

impl<T> Hash for Queue<T>
where
    T: Hash,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_usize(self.len());
        for elem in self {
            elem.hash(state)
        }
    }
}

impl<T> Clone for Queue<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        let mut q = Self::new(self.capacity());
        debug_assert!(
            matches!(q.push_multiple(self.into_iter().cloned()), Ok(())),
            "The queue was created with enough capacity, it should succeed in pushing all values"
        );

        q
    }
}

/// Iterator over immutable contents of a [`Queue`].
#[derive(Debug)]
pub struct Iter<'a, T> {
    left: slice::Iter<'a, T>,
    right: slice::Iter<'a, T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        self.left.next().or_else(|| self.right.next())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.len()
    }
}

impl<'a, T> FusedIterator for Iter<'a, T> {}

impl<'a, T> ExactSizeIterator for Iter<'a, T> {
    fn len(&self) -> usize {
        self.left.len() + self.right.len()
    }
}

impl<'a, T> IntoIterator for &'a Queue<T> {
    type IntoIter = Iter<'a, T>;
    type Item = &'a T;

    fn into_iter(self) -> Self::IntoIter {
        let (left, right) = self.as_slices();

        Iter {
            left: left.iter(),
            right: right.iter(),
        }
    }
}

/// Iterator over mutable contents of a [`Queue`].
#[derive(Debug)]
pub struct IterMut<'a, T> {
    left: slice::IterMut<'a, T>,
    right: slice::IterMut<'a, T>,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        self.left.next().or_else(|| self.right.next())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.len()
    }
}

impl<'a, T> FusedIterator for IterMut<'a, T> {}

impl<'a, T> ExactSizeIterator for IterMut<'a, T> {
    fn len(&self) -> usize {
        self.left.len() + self.right.len()
    }
}

impl<'a, T> IntoIterator for &'a mut Queue<T> {
    type IntoIter = IterMut<'a, T>;
    type Item = &'a mut T;

    fn into_iter(self) -> Self::IntoIter {
        let (left, right) = self.as_slices_mut();

        IterMut {
            left: left.iter_mut(),
            right: right.iter_mut(),
        }
    }
}

/// Iterator over owned contents of a [`Queue`].
#[derive(Debug)]
pub struct IntoIter<T> {
    queue: Queue<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop().ok()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.queue.len();
        (len, Some(len))
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.len()
    }
}

impl<T> FusedIterator for IntoIter<T> {}

impl<T> ExactSizeIterator for IntoIter<T> {
    fn len(&self) -> usize {
        self.queue.len()
    }
}

impl<T> IntoIterator for Queue<T> {
    type IntoIter = IntoIter<T>;
    type Item = T;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { queue: self }
    }
}

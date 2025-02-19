use std::borrow::Borrow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::Bound;
use std::fmt::{Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::iter::FusedIterator;
use std::marker::PhantomData;
use std::mem::{forget, MaybeUninit};
use std::ops::RangeBounds;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::ptr::{drop_in_place, NonNull};
use std::thread::panicking;

use crate::cursor::Cursor;
use crate::node::{address_after, address_before, normalize_address, Node, NodePtr, M};
use crate::utils::PtrEq;
use crate::BTreeStore;

/// A b-tree map.
///
/// See [std::collections::BTreeMap] for more info.
// TODO: impl Clone
pub struct BTreeMap<'store, K, V> {
    store: &'store BTreeStore<K, V>,
    root: Option<NodePtr<K, V>>,
    length: usize,
    height: usize,
    /// For dropck; the `Box` avoids making the `Unpin` impl more strict than before
    _p: PhantomData<Box<(K, V)>>,
}

/// The result of looking up an address to retrieve or insert an entry
enum Find<K, V> {
    /// The tree is empty
    NoRoot,
    /// The entry would be before this address
    Before { node: NodePtr<K, V>, idx: u16 },
    /// The entry is at this address
    At { node: NodePtr<K, V>, idx: u16 },
}

/// Pointer and index to the start and end entry for a range within a tree.
///
/// These bounds are always inclusive. Use `Option<NodeBounds<'a, K, V>>` to represent a
/// potentially-empty range.
struct NodeBounds<K, V> {
    /// Start node (inclusive)
    start_node: NodePtr<K, V>,
    /// End node (inclusive)
    end_node: NodePtr<K, V>,
    /// Index in start node (inclusive)
    start_index: u16,
    /// Index in end node (inclusive)
    end_index: u16,
}

impl<'store, K, V> BTreeMap<'store, K, V> {
    /// Creates an empty `BTreeMap`.
    ///
    /// # Examples
    ///
    /// ```
    /// use btree_plus_store::{BTreeMap, BTreeStore};
    /// let store = BTreeStore::<&str, i32>::new();
    /// let mut map = BTreeMap::new_in(&store);
    /// ```
    #[inline]
    pub const fn new_in(store: &'store BTreeStore<K, V>) -> Self {
        Self {
            store,
            root: None,
            length: 0,
            height: 0,
            _p: PhantomData,
        }
    }

    // region length
    /// Returns the number of elements in the map.
    #[inline]
    pub fn len(&self) -> usize {
        self.length
    }

    /// Returns `true` if the map contains no elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
    // endregion

    // region retrieval
    /// Whether the map contains the key
    #[inline]
    pub fn contains_key<Q: Ord + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        matches!(self.find(key), Find::At { .. })
    }

    /// Returns a reference to the value corresponding to the key.
    #[inline]
    pub fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        match self.find(key) {
            Find::At { node, idx } => unsafe { Some(node.as_ref().val(idx)) },
            _ => None,
        }
    }

    /// Returns a mutable reference to the value corresponding to the key.
    #[inline]
    pub fn get_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        match self.find(key) {
            Find::At { mut node, idx } => unsafe { Some(node.as_mut().val_mut(idx)) },
            _ => None,
        }
    }

    /// Returns a reference to the equivalent key
    ///
    /// This is (only) useful when `Q` is a different type than `K`.
    #[inline]
    pub fn get_key<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&K>
    where
        K: Borrow<Q>,
    {
        match self.find(key) {
            Find::At { node, idx } => unsafe { Some(node.as_ref().key(idx)) },
            _ => None,
        }
    }

    /// Returns a reference to the equivalent key and associated value
    ///
    /// This is (only) useful when `Q` is a different type than `K`.
    #[inline]
    pub fn get_key_value<Q: Ord + ?Sized>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
    {
        match self.find(key) {
            Find::At { node, idx } => unsafe { Some(node.as_ref().key_val(idx)) },
            _ => None,
        }
    }

    /// Returns a reference to the equivalent key and mutable associated value
    ///
    /// This is (only) useful when `Q` is a different type than `K`.
    #[inline]
    pub fn get_key_value_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<(&K, &mut V)>
    where
        K: Borrow<Q>,
    {
        match self.find(key) {
            Find::At { mut node, idx } => unsafe { Some(node.as_mut().key_val_mut(idx)) },
            _ => None,
        }
    }

    /// Returns the first key and value
    #[inline]
    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        self.first_leaf()
            .map(|node| unsafe { node.as_ref().first_key_value() })
    }

    /// Returns the first key and mutable value
    #[inline]
    pub fn first_key_value_mut(&mut self) -> Option<(&K, &mut V)> {
        self.first_leaf()
            .map(|mut node| unsafe { node.as_mut().first_key_value_mut() })
    }

    /// Returns the last key and value
    #[inline]
    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        self.last_leaf()
            .map(|node| unsafe { node.as_ref().last_key_value() })
    }

    /// Returns the last key and mutable value
    #[inline]
    pub fn last_key_value_mut(&mut self) -> Option<(&K, &mut V)> {
        self.last_leaf()
            .map(|mut node| unsafe { node.as_mut().last_key_value_mut() })
    }
    // endregion

    // region insertion and removal
    /// Inserts a key-value pair into the map.
    #[inline]
    pub fn insert(&mut self, key: K, val: V) -> Option<V>
    where
        K: Clone + Ord,
    {
        match self.find(&key) {
            Find::NoRoot => {
                self.insert_root(key, val);
                None
            }
            Find::Before { node, idx } => unsafe {
                self.insert_before(key, val, node, idx);
                None
            },
            Find::At { mut node, idx } => unsafe { Some(node.as_mut().replace_val(idx, val)) },
        }
    }

    /// Get a reference to the value at the given key, or insert a new value if the key is not
    /// present.
    #[inline]
    pub fn get_or_insert(&mut self, key: K, val: V) -> &mut V
    where
        K: Clone + Ord,
    {
        match self.find(&key) {
            Find::NoRoot => unsafe {
                self.insert_root(key, val);
                self.root.unwrap().as_mut().val_mut(0)
            },
            Find::Before { node, idx } => unsafe {
                // Maybe could optimize into a single lookup...
                self.insert_before(key.clone(), val, node, idx);
                self.get_mut(&key).unwrap()
            },
            Find::At { mut node, idx } => unsafe { node.as_mut().val_mut(idx) },
        }
    }

    /// Removes the equivalent key and returns the actual key and value, if present.
    #[inline]
    pub fn remove_key_value<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Clone + Borrow<Q>,
    {
        match self.find(key) {
            Find::NoRoot | Find::Before { .. } => None,
            Find::At { mut node, idx } => unsafe {
                let (key, val) = node.as_mut().remove_val(idx);
                self.post_removal(node);
                Some((key, val))
            },
        }
    }

    /// Removes the equivalent key and returns the value if present.
    #[inline]
    pub fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Clone + Borrow<Q>,
    {
        self.remove_key_value(key).map(|(_, val)| val)
    }

    /// Removes the first key and value as long as the map isn't empty
    #[inline]
    pub fn pop_first(&mut self) -> Option<(K, V)>
    where
        K: Clone,
    {
        self.first_leaf().map(|mut node| unsafe {
            let (key, val) = node.as_mut().remove_val(0);
            self.post_removal(node);
            (key, val)
        })
    }

    /// Removes the last key and value as long as the map isn't empty
    #[inline]
    pub fn pop_last(&mut self) -> Option<(K, V)>
    where
        K: Clone,
    {
        self.last_leaf().map(|mut node| unsafe {
            let idx = node.as_ref().len - 1;
            let (key, val) = node.as_mut().remove_val(idx);
            self.post_removal(node);
            (key, val)
        })
    }

    /// Clears the map, removing all key-value pairs.
    #[inline]
    pub fn clear(&mut self) {
        if let Some(root) = self.root.take() {
            unsafe {
                drop_node_ptr(root, self.height, &mut |n| self.store.dealloc(n));
            }
        }
        self.length = 0;
    }
    // endregion

    // region advanced
    /// Transforms the value at the given key, inserting if we go from `None` to `Some` and removing
    /// if we go from `Some` to `None`. Also returns a value.
    ///
    /// Also, if the function `panic`s we always remove the key, so this is effectively a
    /// special-case of [`replace_with`](https://docs.rs/replace_with/latest/replace_with/) for the
    /// map.
    #[inline]
    pub fn update_and_return<R>(
        &mut self,
        key: K,
        update: impl FnOnce(Option<V>) -> (Option<V>, R),
    ) -> R
    where
        K: Clone + Ord,
    {
        match self.find(&key) {
            Find::NoRoot => match update(None) {
                (None, r) => r,
                (Some(val), r) => {
                    self.insert_root(key, val);
                    r
                }
            },
            Find::At { mut node, idx } => unsafe {
                match catch_unwind(AssertUnwindSafe(|| {
                    let val = node.as_mut().read_val(idx);
                    update(Some(val))
                })) {
                    Err(err) => {
                        let (_key, value) = node.as_mut().remove_val(idx);
                        forget(value);
                        self.post_removal(node);
                        resume_unwind(err);
                    }
                    Ok((None, r)) => {
                        let (_key, value) = node.as_mut().remove_val(idx);
                        forget(value);
                        self.post_removal(node);
                        r
                    }
                    Ok((Some(val), r)) => {
                        node.as_mut().write_val(idx, val);
                        r
                    }
                }
            },
            Find::Before { node, idx } => match update(None) {
                (None, r) => r,
                (Some(val), r) => unsafe {
                    self.insert_before(key, val, node, idx);
                    r
                },
            },
        }
    }

    /// Transforms the value at the given key, inserting if we go from `None` to `Some` and removing
    /// if we go from `Some` to `None`.
    ///
    /// Also, if the function `panic`s we always remove the key, so this is effectively a
    /// special-case of [`replace_with`](https://docs.rs/replace_with/latest/replace_with/) for the
    /// map.
    #[inline]
    pub fn update(&mut self, key: K, update: impl FnOnce(Option<V>) -> Option<V>)
    where
        K: Clone + Ord,
    {
        self.update_and_return(key, |val| (update(val), ()))
    }

    /// Validates the map, *panic*ing if it is invalid. Specifically, we check that the number of
    /// entries in each node is within the b-tree invariant bounds, and that the keys are in order.
    ///
    /// Ideally, this should always be a no-op.
    #[inline]
    pub fn validate(&self)
    where
        K: Debug + Ord,
        V: Debug,
    {
        unsafe fn validate_node<K: Debug + Ord, V: Debug>(
            errors: &mut Vec<String>,
            node: NodePtr<K, V>,
            parent: Option<(NodePtr<K, V>, u16)>,
            height: usize,
            (mut prev_key, mut prev_leaf): (Option<NonNull<K>>, Option<NodePtr<K, V>>),
        ) -> (usize, (NonNull<K>, NodePtr<K, V>)) {
            let errors = RefCell::new(errors);
            let assert2 = |node: NodePtr<K, V>, cond: bool, msg: &str| {
                if !cond {
                    (*errors.borrow_mut()).push(format!("{:X?} {}", node.as_ptr(), msg))
                }
            };
            let assert = |cond: bool, msg: &str| {
                if !cond {
                    assert2(node, cond, msg)
                }
            };

            let is_leaf = height == 0;
            let node_ptr = node;
            let node = node.as_ref();

            assert(
                node.parent().map(|p| p.0).ptr_eq(&parent.map(|p| p.0)),
                "parent pointer is incorrect",
            );
            assert(
                node.parent().map(|p| p.1).ptr_eq(&parent.map(|p| p.1)),
                "parent index is incorrect",
            );

            let min_len = match parent {
                None => 1,
                Some(_) => M / 2,
            } as u16;
            let max_len = M as u16;
            assert(node.len >= min_len, "has too few entries");
            assert(node.len <= max_len, "has too many entries");

            if is_leaf {
                assert(node.prev().ptr_eq(&prev_leaf), "prev leaf is incorrect");
                for i in 0..node.len {
                    let key = node.key(i);

                    if let Some(prev_key) = prev_key {
                        let prev_key = prev_key.as_ref();
                        assert(
                            match i {
                                0 => key >= prev_key,
                                _ => key > prev_key,
                            },
                            &format!("key {} is out of order", i),
                        );
                    }

                    prev_key = Some(NonNull::from(key));
                }
                (node.len as usize, (prev_key.unwrap(), node_ptr))
            } else {
                let mut len = 0;

                for i in 0..node.len + 1 {
                    if let Some(ki) = i.checked_sub(1) {
                        let key = node.key(ki);

                        if let Some(prev_key) = prev_key {
                            let prev_key = prev_key.as_ref();
                            assert(key > prev_key, &format!("key {} is out of order", i));
                        }

                        prev_key = Some(NonNull::from(key));
                    }

                    let child = node.edge(i);

                    if height == 1 {
                        if let Some(prev_leaf) = prev_leaf {
                            let prev_leaf_ptr = prev_leaf;
                            let prev_leaf = prev_leaf.as_ref();
                            assert2(
                                prev_leaf_ptr,
                                prev_leaf.next().ptr_eq(&Some(child)),
                                &format!(
                                    "next leaf is incorrect (expected {:X?} got {:X?})",
                                    child.as_ptr(),
                                    as_nullable_ptr(prev_leaf.next())
                                ),
                            )
                        }
                    }

                    let (child_len, (last_key, last_leaf)) = validate_node(
                        *errors.borrow_mut(),
                        child,
                        Some((node_ptr, i)),
                        height - 1,
                        (prev_key, prev_leaf),
                    );
                    len += child_len;
                    prev_key = Some(last_key);
                    prev_leaf = Some(last_leaf);
                }
                (len, (prev_key.unwrap(), prev_leaf.unwrap()))
            }
        }
        let mut errors = Vec::new();
        if let Some(root) = self.root {
            let (len, (_last_key, last_leaf)) =
                unsafe { validate_node(&mut errors, root, None, self.height, (None, None)) };
            if len != self.length {
                errors.push(String::from("tree length isn't correct"))
            };
            if !unsafe { last_leaf.as_ref().next() }.ptr_eq(&None) {
                errors.push(format!("{:X?} next leaf is incorrect", unsafe {
                    last_leaf.as_ptr()
                }))
            }
        }
        if !errors.is_empty() {
            panic!("invalid b-tree:\n{:?}\n- {}", self, errors.join("\n- "));
        }
    }

    /// Prints the b-tree in ascii
    #[inline]
    pub fn print(&self, f: &mut Formatter<'_>) -> std::fmt::Result
    where
        K: Debug,
        V: Debug,
    {
        unsafe fn print_node<K: Debug, V: Debug>(
            f: &mut Formatter<'_>,
            node: NodePtr<K, V>,
            max_height: usize,
            height: usize,
        ) -> std::fmt::Result {
            let is_leaf = height == 0;
            let node_ptr = node;
            let node = node.as_ref();
            let indent = "│ ".repeat(max_height - height);
            write!(f, "{}• {:X?}, parent = ", indent, node_ptr.as_ptr())?;
            match node.parent() {
                Some((parent, parent_idx)) => {
                    write!(f, "({:X?}, {})", parent.as_ptr(), parent_idx)?
                }
                None => write!(f, "None")?,
            }
            if is_leaf {
                writeln!(
                    f,
                    ", prev = {:X?}, next = {:X?}",
                    as_nullable_ptr(node.prev()),
                    as_nullable_ptr(node.next())
                )?;
                for i in 0..node.len {
                    let bullet = if i == node.len - 1 { "└" } else { "├" };
                    writeln!(
                        f,
                        "{}{} {:?} = {:?}",
                        indent,
                        bullet,
                        node.key(i),
                        node.val(i)
                    )?;
                }
            } else {
                writeln!(f)?;
                for i in 0..node.len + 1 {
                    if let Some(ki) = i.checked_sub(1) {
                        let key = node.key(ki);
                        writeln!(f, "{}├ {:?}", indent, key)?;
                    }

                    let child = node.edge(i);
                    print_node(f, child, max_height, height - 1)?;
                }
                writeln!(f, "{}└", indent)?;
            }
            Ok(())
        }
        if let Some(root) = self.root {
            unsafe { print_node(f, root, self.height, self.height) }
        } else {
            writeln!(f, "empty")
        }
    }
    // endregion

    // region iteration
    /// Iterates over the map's key-value pairs in order.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter::new(self)
    }

    /// Iterates over the map's key-value pairs in order. Values are mutable
    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut::new(self)
    }

    /// Iterates over the map's keys in order.
    #[inline]
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys(self.iter())
    }

    /// Iterates over the map's values in order.
    #[inline]
    pub fn values(&self) -> Values<'_, K, V> {
        Values(self.iter())
    }

    /// Iterates over the map's values in order. Values are mutable
    #[inline]
    pub fn values_mut(&mut self) -> ValuesMut<'_, K, V> {
        ValuesMut(self.iter_mut())
    }

    /// Iterates over the map's key-value pairs in order, within the given range.
    #[inline]
    pub fn range<Q: Ord + ?Sized>(&self, bounds: impl RangeBounds<Q>) -> Range<'_, K, V>
    where
        K: Borrow<Q>,
    {
        Range::new(self, bounds)
    }

    /// Iterates over the map's key-value pairs in order, within the given range.. Values are mutable
    #[inline]
    pub fn range_mut<Q: Ord + ?Sized>(&mut self, bounds: impl RangeBounds<Q>) -> RangeMut<'_, K, V>
    where
        K: Borrow<Q>,
    {
        RangeMut::new(self, bounds)
    }

    /// Iterates over the map's keys in order, within the given range.
    #[inline]
    pub fn range_keys<Q: Ord + ?Sized>(
        &self,
        bounds: impl RangeBounds<Q>,
    ) -> impl Iterator<Item = &K> + '_
    where
        K: Borrow<Q>,
    {
        self.range(bounds).map(|(k, _)| k)
    }

    /// Iterates over the map's values in order, within the given range.
    #[inline]
    pub fn range_values<Q: Ord + ?Sized>(
        &self,
        bounds: impl RangeBounds<Q>,
    ) -> impl Iterator<Item = &V> + '_
    where
        K: Borrow<Q>,
    {
        self.range(bounds).map(|(_, v)| v)
    }

    /// Iterates over the map's values in order, within the given range. Values are mutable
    #[inline]
    pub fn range_values_mut<Q: Ord + ?Sized>(
        &mut self,
        bounds: impl RangeBounds<Q>,
    ) -> impl Iterator<Item = &mut V> + '_
    where
        K: Borrow<Q>,
    {
        self.range_mut(bounds).map(|(_, v)| v)
    }

    // /// Drains elements.
    // #[inline]
    // pub fn drain(&mut self) -> Drain<'_, K, V> {
    //     Drain::new(self)
    // }

    // /// Removes elements which don't pass the predicate
    // #[inline]
    // pub fn retain<F: FnMut(&K, &mut V) -> bool>(&mut self, mut f: F) {
    //     self.drain_filter(|k, v| !f(k, v));
    // }

    // /// Drains elements according to the filter.
    // #[inline]
    // pub fn drain_filter<F: FnMut(&K, &mut V) -> bool>(&mut self, filter: F) -> DrainFilter<'_, K, V, F> {
    //     DrainFilter::new(self, filter)
    // }

    // /// Drains elements within the given range
    // #[inline]
    // pub fn drain_range<Q: Ord + ?Sized>(&mut self, bounds: impl RangeBounds<Q>) -> DrainRange<'_, K, V> where K: Borrow<Q> {
    //     DrainRange::new(self, bounds)
    // }

    // /// Removes elements within the range which don't pass the predicate
    // #[inline]
    // pub fn retain_range<Q: Ord, F: FnMut(&K, &mut V) -> bool>(&mut self, bounds: impl RangeBounds<Q>, mut f: F) where K: Borrow<Q> {
    //     self.drain_filter_range(bounds, |k, v| !f(k, v));
    // }

    // /// Drains elements within the given range according to the filter
    // #[inline]
    // pub fn drain_filter_range<Q: Ord, F: FnMut(&K, &mut V) -> bool>(&mut self, bounds: impl RangeBounds<Q>, mut filter: F) -> DrainFilterRange<'_, K, V, F> where K: Borrow<Q> {
    //     DrainFilterRange::new(self, bounds, filter)
    // }
    // endregion

    // region b-tree misc
    #[inline]
    fn first_leaf(&self) -> Option<NodePtr<K, V>> {
        let mut node = self.root?;
        for _ in 0..self.height {
            node = unsafe { node.as_ref().edge(0) };
        }
        Some(node)
    }

    #[inline]
    fn last_leaf(&self) -> Option<NodePtr<K, V>> {
        let mut node = self.root?;
        for _ in 0..self.height {
            node = unsafe { node.as_ref().edge(node.as_ref().len) };
        }
        Some(node)
    }

    #[inline]
    fn find<Q: Ord + ?Sized>(&self, key: &Q) -> Find<K, V>
    where
        K: Borrow<Q>,
    {
        let Some(mut node) = self.root else {
            return Find::NoRoot
        };
        let mut height = self.height;
        loop {
            match unsafe { node.as_ref().keys() }.binary_search_by(|k| k.borrow().cmp(key)) {
                Ok(idx) => {
                    let idx = idx as u16;
                    if height == 0 {
                        break Find::At { node, idx };
                    }
                    height -= 1;
                    node = unsafe { node.as_ref().edge(idx + 1) }
                }
                Err(idx) => {
                    let idx = idx as u16;
                    if height == 0 {
                        break Find::Before { node, idx };
                    }
                    height -= 1;
                    node = unsafe { node.as_ref().edge(idx) }
                }
            }
        }
    }

    #[inline]
    fn node_bounds<Q: Ord + ?Sized>(&self, bounds: impl RangeBounds<Q>) -> Option<NodeBounds<K, V>>
    where
        K: Borrow<Q>,
    {
        let (start_node, start_index) = match bounds.start_bound() {
            Bound::Included(bound) => match self.find(bound) {
                Find::NoRoot => return None,
                Find::Before { node, idx } => unsafe { normalize_address(node, idx) }?,
                Find::At { node, idx } => (node, idx),
            },
            Bound::Excluded(bound) => match self.find(bound) {
                Find::NoRoot => return None,
                // normalize_address handles if idx == len, which means we are past this node and
                // may be at the end.
                Find::Before { node, idx } => unsafe { normalize_address(node, idx) }?,
                Find::At { node, idx } => unsafe { address_after(node, idx) }?,
            },
            Bound::Unbounded => (self.first_leaf()?, 0),
        };
        let (end_node, end_index) = match bounds.end_bound() {
            Bound::Included(bound) => match self.find(bound) {
                Find::NoRoot => return None,
                Find::Before { node, idx } => unsafe { address_before(node, idx) }?,
                Find::At { node, idx } => (node, idx),
            },
            Bound::Excluded(bound) => match self.find(bound) {
                Find::NoRoot => return None,
                Find::Before { node, idx } | Find::At { node, idx } => {
                    unsafe { address_before(node, idx) }?
                }
            },
            Bound::Unbounded => self
                .last_leaf()
                .map(|leaf| unsafe { (leaf, leaf.as_ref().len - 1) })?,
        };

        // Check for overlap (only need to check if address_after(start) == end)
        if (start_node.ptr_eq(&end_node) && start_index == end_index + 1)
            || (start_index == 0 && unsafe { start_node.as_ref().prev() }.ptr_eq(&Some(end_node)))
        {
            return None;
        }

        // Actually create
        Some(NodeBounds {
            start_node,
            end_node,
            start_index,
            end_index,
        })
    }

    #[inline]
    fn insert_root(&mut self, key: K, val: V) {
        debug_assert_eq!(self.length, 0);
        let mut root = Node::leaf();
        unsafe {
            root.insert_val(0, key, val);
        }
        self.root = Some(self.store.alloc(root));
        self.length += 1;
    }

    #[inline]
    unsafe fn insert_before(&mut self, mut key: K, val: V, mut node: NodePtr<K, V>, idx: u16)
    where
        K: Clone,
    {
        if (node.as_ref().len as usize) < M {
            node.as_mut().insert_val(idx, key, val);
        } else {
            // Rebalance (overflow)

            // First split
            // `key` gets replaced with the "split" (median) key, and `node` gets replaced with the
            // left node
            let mut right = self
                .store
                .alloc(node.as_mut().split_leaf(idx, &mut key, val));
            node.as_mut().set_next(Some(right));
            right.as_mut().set_prev(Some(node));
            if let Some(mut right_next) = right.as_ref().next() {
                right_next.as_mut().set_prev(Some(right));
            }

            loop {
                let Some((mut parent, idx)) = node.as_ref().parent() else {
                    // At root: create a new root with the split key, left, and right nodes
                    self.height += 1;
                    let mut left = node;
                    let mut root = self.store.alloc(Node::internal());
                    left.as_mut().set_parent(root, 0);
                    // Has to be before insert_edge, otherwise we try to modify a deallocated edge,
                    // because the tree has 0 edges but insert_edge always expects at least 1.
                    // Furthermore, we need correct parent_idx, which is why we set both to 0.
                    right.as_mut().set_parent(root, 0);
                    root.as_mut().set_last_edge(right);
                    root.as_mut().insert_edge(0, false, key, left);
                    self.root = Some(root);
                    break
                };

                // Insert split key and right into parent. left is already in parent at idx, so
                // insert key at idx and right at idx + 1. We must handle the case where the parent
                // overflows too...
                right.as_mut().set_parent(parent, idx + 1);
                if (parent.as_ref().len as usize) < M {
                    // The parent won't overflow, actually insert into parent
                    parent.as_mut().insert_edge(idx, true, key, right);
                    break;
                }
                // The parent will overflow too, so we split the parent when inserting idx/key/right
                // split_internal will replace key with the split key and node with the left node,
                // and we re-assign right to the right node (we don't just pass as a &mut like we do
                // with key because it must be allocated). Then insert the new internal parent-right
                // node in its parent, and so on, until we either find a suitable parent or reach
                // the root.
                node = parent;
                right = self
                    .store
                    .alloc(node.as_mut().split_internal(idx, &mut key, right));
                for right_child in right.as_mut().edges_mut() {
                    right_child.as_mut().parent = Some(right);
                }
            }
        }
        self.length += 1;
    }

    #[inline]
    unsafe fn post_removal(&mut self, mut node: NodePtr<K, V>)
    where
        K: Clone,
    {
        self.length -= 1;

        // Rebalance (underflow)
        let mut is_leaf = true;
        while (node.as_ref().len as usize) < M / 2 {
            let Some((mut parent, idx)) = node.as_ref().parent() else {
                // Node is root. Root node can have less than M < 2 children
                if is_leaf {
                    // If the root is a leaf, it can have min 1 child. Otherwise, the tree
                    // is empty.
                    if node.as_ref().len == 0 {
                        self.root = None;
                    }
                } else if node.as_ref().len < 1 {
                    // If the root is internal, it can have min 1 child (= 2 edges). Otherwise, the
                    // remaining edge becomes the new root.
                    self.height -= 1;
                    self.root = Some(node.as_ref().edge(0));
                    self.store.dealloc(node);
                    self.root.as_mut().unwrap().as_mut().clear_parent();
                }
                break
            };

            // Try to redistribute with prev sibling
            if idx > 0 {
                let mut prev = parent.as_ref().edge(idx - 1);
                if (prev.as_ref().len as usize) > M / 2 {
                    if is_leaf {
                        let (key, val) = prev.as_mut().remove_val(prev.as_ref().len - 1);
                        node.as_mut().insert_val(0, key.clone(), val);
                        parent.as_mut().replace_key(idx - 1, key);
                    } else {
                        let (key, mut edge) = prev.as_mut().remove_last_edge();
                        let key = parent.as_mut().replace_key(idx - 1, key);
                        edge.as_mut().set_parent(node, 0);
                        node.as_mut().insert_edge(0, false, key, edge);
                    }
                    break;
                }
            }

            // Try to redistribute with next sibling
            if idx < parent.as_ref().len {
                let mut next = parent.as_ref().edge(idx + 1);
                if (next.as_ref().len as usize) > M / 2 {
                    if is_leaf {
                        parent
                            .as_mut()
                            .replace_key(idx, next.as_ref().key(1).clone());
                        let (key, val) = next.as_mut().remove_val(0);
                        node.as_mut().insert_val(node.as_ref().len, key, val);
                    } else {
                        let (key, mut edge) = next.as_mut().remove_edge(0, false);
                        let key = parent.as_mut().replace_key(idx, key);
                        let len = node.as_ref().len;
                        edge.as_mut().set_parent(node, len + 1);
                        node.as_mut().insert_edge(len, true, key, edge);
                    }
                    break;
                }
            }

            // Merge with prev sibling or next sibling. We prioritize prev just because, but
            // must choose next if idx == 0
            if idx > 0 {
                let mut prev = parent.as_mut().edge(idx - 1);
                if is_leaf {
                    node.as_mut().merge_prev_leaf(prev.as_mut());
                    if let Some(mut new_prev) = node.as_ref().prev() {
                        new_prev.as_mut().set_next(Some(node));
                    }
                } else {
                    let key = parent.as_ref().key(idx - 1).clone();
                    for child in prev.as_mut().edges_mut() {
                        child.as_mut().parent = Some(node);
                    }
                    node.as_mut().merge_prev_internal(key, prev.as_mut());
                }

                // Dealloc and remove absorbed (empty) node and fix indices of the nodes
                // after
                let (_key, edge) = parent.as_mut().remove_edge(idx - 1, false);
                debug_assert!(edge.ptr_eq(&prev));
                self.store.dealloc(prev);
            } else {
                let mut next = parent.as_mut().edge(idx + 1);
                if is_leaf {
                    node.as_mut().merge_next_leaf(next.as_mut());
                    if let Some(mut new_next) = node.as_ref().next() {
                        new_next.as_mut().set_prev(Some(node));
                    }
                } else {
                    let key = parent.as_ref().key(idx).clone();
                    for child in next.as_mut().edges_mut() {
                        child.as_mut().parent = Some(node);
                    }
                    node.as_mut().merge_next_internal(key, next.as_mut());
                }

                // Dealloc and remove absorbed (empty) node and fix indices of the nodes
                // after
                let (_key, edge) = parent.as_mut().remove_edge(idx, true);
                debug_assert!(edge.ptr_eq(&next));
                self.store.dealloc(next);
            }

            // Since we merged, we may now have to redistribute or merge the parent since it
            // has 1 less child
            node = parent;
            is_leaf = false;
        }
    }
    // endregion
}

impl<K, V> NodeBounds<K, V> {
    #[inline]
    fn start(&self) -> (NodePtr<K, V>, u16) {
        (self.start_node, self.start_index)
    }

    #[inline]
    fn end(&self) -> (NodePtr<K, V>, u16) {
        (self.end_node, self.end_index)
    }
}

// region common trait impls
impl<'store, K: Debug, V: Debug> Debug for BTreeMap<'store, K, V> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.print(f)
    }
}

impl<'store, K: PartialEq, V: PartialEq> PartialEq for BTreeMap<'store, K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl<'store, K: Eq, V: Eq> Eq for BTreeMap<'store, K, V> {}

impl<'store, K: PartialOrd, V: PartialOrd> PartialOrd for BTreeMap<'store, K, V> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.iter().partial_cmp(other.iter())
    }
}

impl<'store, K: Ord, V: Ord> Ord for BTreeMap<'store, K, V> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.iter().cmp(other.iter())
    }
}

impl<'store, K: Hash, V: Hash> Hash for BTreeMap<'store, K, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for (k, v) in self.iter() {
            k.hash(state);
            v.hash(state);
        }
    }
}

impl<'store, K: Ord + Clone, V> Extend<(K, V)> for BTreeMap<'store, K, V> {
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}
// endregion

// region drop and dealloc
impl<'store, K, V> Drop for BTreeMap<'store, K, V> {
    #[inline]
    fn drop(&mut self) {
        if panicking() {
            // TODO: Drop when panicking without causing UB (need to reorder some operations)
            return;
        }

        if let Some(root) = self.root.take() {
            unsafe { drop_node_ptr(root, self.height, &mut |n| self.store.dealloc(n)) }
        }
    }
}

unsafe fn drop_node_ptr<K, V>(
    mut node: NodePtr<K, V>,
    height: usize,
    dealloc: &mut impl FnMut(NodePtr<K, V>),
) {
    let node_ref = node.as_mut();

    for key in node_ref.keys_mut() {
        drop_in_place(key as *mut _);
    }
    if height > 0 {
        for &child in node_ref.edges() {
            drop_node_ptr(child, height - 1, dealloc);
        }
    } else {
        for val in node_ref.vals_mut() {
            drop_in_place(val as *mut _);
        }
    }

    dealloc(node);
}

/// If this address is at the start of the node, deallocates the node, then checks if it's at the
/// start of its parent, if so deallocates its parent, and so on.
///
/// Doesn't drop any of the nodes' contents
unsafe fn dealloc_up_firsts<K, V>(
    mut address: (NodePtr<K, V>, u16),
    mut dealloc: impl FnMut(NodePtr<K, V>),
) {
    loop {
        let (node, idx) = address;

        debug_assert!(
            idx <= node.as_ref().len,
            "sanity check failed: address.idx > address.node.len (invariant broke BEFORE this call)"
        );
        if idx != 0 {
            break;
        }

        let parent = node.as_ref().parent();
        dealloc(node);

        let Some(parent) = parent else {
            break
        };
        address = parent;
    }
}

/// If this address is at the end of the node, deallocates the node, then checks if it's at the end
/// of its parent, if so deallocates its parent, and so on.
///
/// Doesn't drop any of the nodes' contents
#[inline]
unsafe fn dealloc_up_lasts<K, V>(
    (mut node, mut idx): (NodePtr<K, V>, u16),
    mut dealloc: impl FnMut(NodePtr<K, V>),
) {
    debug_assert!(
        idx < node.as_ref().len,
        "sanity check failed: address.idx >= address.node.len (invariant broke BEFORE this call)"
    );
    if idx != node.as_ref().len - 1 {
        return;
    }

    while let Some(parent) = {
        let parent = node.as_ref().parent();
        dealloc(node);
        parent
    } {
        node = parent.0;
        idx = parent.1;

        debug_assert!(
            idx <= node.as_ref().len,
            "sanity check failed: address.idx > address.node.len (invariant broke BEFORE this call)"
        );
        if idx != node.as_ref().len {
            break;
        }
    }
}
// endregion

// region iterators (almost all boilerplate)
//noinspection DuplicatedCode
// region iterator impls
impl<'store: 'a, 'a, K, V> IntoIterator for &'a BTreeMap<'store, K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'store: 'a, 'a, K, V> IntoIterator for &'a mut BTreeMap<'store, K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'store, K, V> IntoIterator for BTreeMap<'store, K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<'store, K, V>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        IntoIter::new(self)
    }
}
// endregion

// region Iter
pub struct Iter<'a, K, V> {
    cursor: Cursor<'a, K, V>,
    back_cursor: Cursor<'a, K, V>,
    length: usize,
    _p: PhantomData<(&'a K, &'a V)>,
}

//noinspection DuplicatedCode
impl<'a, K, V> Iter<'a, K, V> {
    #[inline]
    fn new(tree: &'a BTreeMap<K, V>) -> Self {
        Self {
            cursor: unsafe { Cursor::new(tree.first_leaf(), 0) },
            back_cursor: unsafe { Cursor::new_at_end(tree.last_leaf()) },
            length: tree.length,
            _p: PhantomData,
        }
    }

    /// Get the next element without advancing the iterator
    #[inline]
    pub fn peek(&self) -> Option<(&'a K, &'a V)> {
        if self.length == 0 {
            return None;
        }
        self.cursor.key_value()
    }

    /// Get the next back element without advancing the back iterator
    #[inline]
    pub fn peek_back(&self) -> Option<(&'a K, &'a V)> {
        if self.length == 0 {
            return None;
        }
        self.back_cursor.key_value()
    }

    /// Equivalent to `next` except *panics* if iteration is done.
    #[inline]
    pub fn advance(&mut self) {
        if self.length == 0 {
            panic!("iteration is done");
        }
        self.cursor.advance();
        self.length -= 1;
    }

    /// Equivalent to `next_back` except *panics* if iteration is done.
    #[inline]
    pub fn advance_back(&mut self) {
        if self.length == 0 {
            panic!("iteration is done");
        }
        self.back_cursor.advance_back();
        self.length -= 1;
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let key_value = self.peek()?;
        self.advance();
        Some(key_value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.length, Some(self.length))
    }
}

impl<'a, K, V> DoubleEndedIterator for Iter<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        let key_value = self.peek_back()?;
        self.advance_back();
        Some(key_value)
    }
}

impl<'a, K, V> ExactSizeIterator for Iter<'a, K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.length
    }
}

impl<'a, K, V> FusedIterator for Iter<'a, K, V> {}
// endregion

// region IterMut
pub struct IterMut<'a, K, V> {
    cursor: Cursor<'a, K, V>,
    back_cursor: Cursor<'a, K, V>,
    length: usize,
    /// Unlike in [Cursor], reference to `V` is mutable
    _p: PhantomData<(&'a K, &'a mut V)>,
}

//noinspection DuplicatedCode
impl<'a, K, V> IterMut<'a, K, V> {
    #[inline]
    fn new(tree: &'a BTreeMap<K, V>) -> Self {
        Self {
            cursor: unsafe { Cursor::new(tree.first_leaf(), 0) },
            back_cursor: unsafe { Cursor::new_at_end(tree.last_leaf()) },
            length: tree.length,
            _p: PhantomData,
        }
    }

    /// Get the next element without advancing the iterator
    #[inline]
    pub fn peek(&self) -> Option<(&'a K, &'a V)> {
        if self.length == 0 {
            return None;
        }
        self.cursor.key_value()
    }

    /// Get the next back element without advancing the back iterator
    #[inline]
    pub fn peek_back(&self) -> Option<(&'a K, &'a V)> {
        if self.length == 0 {
            return None;
        }
        self.back_cursor.key_value()
    }

    /// Get the next element without advancing the iterator
    #[inline]
    pub fn peek_mut(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.length == 0 {
            return None;
        }
        unsafe { self.cursor.key_value_mut() }
    }

    /// Get the next back element without advancing the back iterator
    #[inline]
    pub fn peek_back_mut(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.length == 0 {
            return None;
        }
        unsafe { self.back_cursor.key_value_mut() }
    }

    /// Equivalent to `next` except *panics* if iteration is done.
    #[inline]
    pub fn advance(&mut self) {
        if self.length == 0 {
            panic!("iteration is done");
        }
        self.cursor.advance();
        self.length -= 1;
    }

    /// Equivalent to `next_back` except *panics* if iteration is done.
    #[inline]
    pub fn advance_back(&mut self) {
        if self.length == 0 {
            panic!("iteration is done");
        }
        self.back_cursor.advance_back();
        self.length -= 1;
    }
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let key_value = self.peek_mut()?;
        self.advance();
        Some(key_value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.length, Some(self.length))
    }
}

impl<'a, K, V> DoubleEndedIterator for IterMut<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        let key_value = self.peek_back_mut()?;
        self.advance_back();
        Some(key_value)
    }
}

impl<'a, K, V> ExactSizeIterator for IterMut<'a, K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.length
    }
}

impl<'a, K, V> FusedIterator for IterMut<'a, K, V> {}
// endregion

// region IntoIter
pub struct IntoIter<'store, K, V> {
    store: &'store BTreeStore<K, V>,
    cursor: Cursor<'store, K, V>,
    back_cursor: Cursor<'store, K, V>,
    length: usize,
    /// Unlike in [Cursor], `K` and `V` are owned
    _p: PhantomData<(K, V)>,
}

impl<'store, K, V> IntoIter<'store, K, V> {
    #[inline]
    fn new(tree: BTreeMap<'store, K, V>) -> Self {
        let result = Self {
            store: tree.store,
            cursor: unsafe { Cursor::new(tree.first_leaf(), 0) },
            back_cursor: unsafe { Cursor::new_at_end(tree.last_leaf()) },
            length: tree.length,
            _p: PhantomData,
        };
        // We drop the tree's nodes, so prevent it drop dropping when it drops itself.
        // This should be equivalent to `tree.root = None`
        forget(tree);
        result
    }
}

impl<'store, K, V> Iterator for IntoIter<'store, K, V> {
    type Item = (K, V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.length == 0 {
            return None;
        }
        unsafe {
            let key_value = self.cursor.read_key_value().unwrap();
            let address = self.cursor.address().unwrap();
            self.cursor.advance();
            dealloc_up_lasts(address, |n| self.store.dealloc(n));
            self.length -= 1;
            Some(key_value)
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.length, Some(self.length))
    }
}

impl<'a, K, V> DoubleEndedIterator for IntoIter<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.length == 0 {
            return None;
        }
        unsafe {
            let key_value = self.back_cursor.read_key_value().unwrap();
            let address = self.back_cursor.address().unwrap();
            self.back_cursor.advance_back();
            dealloc_up_firsts(address, |n| self.store.dealloc(n));
            self.length -= 1;
            Some(key_value)
        }
    }
}

impl<'store, K, V> ExactSizeIterator for IntoIter<'store, K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.length
    }
}

impl<'store, K, V> FusedIterator for IntoIter<'store, K, V> {}
// endregion

// region Keys
pub struct Keys<'a, K, V>(Iter<'a, K, V>);

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, _)| k)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'a, K, V> DoubleEndedIterator for Keys<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|(k, _)| k)
    }
}

impl<'a, K, V> ExactSizeIterator for Keys<'a, K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'a, K, V> FusedIterator for Keys<'a, K, V> {}
// endregion

// region Values
pub struct Values<'a, K, V>(Iter<'a, K, V>);

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(_, v)| v)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'a, K, V> DoubleEndedIterator for Values<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|(_, v)| v)
    }
}

impl<'a, K, V> ExactSizeIterator for Values<'a, K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'a, K, V> FusedIterator for Values<'a, K, V> {}
// endregion

// region ValuesMut
pub struct ValuesMut<'a, K, V>(IterMut<'a, K, V>);

impl<'a, K, V> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(_, v)| v)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'a, K, V> DoubleEndedIterator for ValuesMut<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|(_, v)| v)
    }
}

impl<'a, K, V> ExactSizeIterator for ValuesMut<'a, K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl<'a, K, V> FusedIterator for ValuesMut<'a, K, V> {}
// endregion

// region Range
pub struct Range<'a, K, V> {
    cursor: Cursor<'a, K, V>,
    back_cursor: Cursor<'a, K, V>,
    bounds: MaybeUninit<NodeBounds<K, V>>,
    _p: PhantomData<(&'a K, &'a V)>,
}

//noinspection DuplicatedCode
impl<'a, K, V> Range<'a, K, V> {
    #[inline]
    fn new<Q: Ord + ?Sized>(tree: &'a BTreeMap<K, V>, bounds: impl RangeBounds<Q>) -> Self
    where
        K: Borrow<Q>,
    {
        let bounds = tree.node_bounds(bounds);
        let cursor = match bounds.as_ref().map(|b| b.start()) {
            None => Cursor::new_detached(),
            Some((start_node, start_idx)) => unsafe { Cursor::new(Some(start_node), start_idx) },
        };
        let back_cursor = match bounds.as_ref().map(|b| b.end()) {
            None => Cursor::new_detached(),
            Some((end_node, end_idx)) => unsafe { Cursor::new(Some(end_node), end_idx) },
        };
        let bounds = match bounds {
            None => MaybeUninit::uninit(),
            Some(bounds) => MaybeUninit::new(bounds),
        };
        Self {
            cursor,
            back_cursor,
            bounds,
            _p: PhantomData,
        }
    }

    /// Get the next element without advancing the iterator
    #[inline]
    pub fn peek(&self) -> Option<(&'a K, &'a V)> {
        self.cursor.key_value()
    }

    /// Get the next back element without advancing the back iterator
    #[inline]
    pub fn peek_back(&self) -> Option<(&'a K, &'a V)> {
        self.back_cursor.key_value()
    }

    /// Equivalent to `next` except *panics* if iteration is done.
    #[inline]
    pub fn advance(&mut self) {
        self.cursor.advance();
        if !self.cursor.is_attached() {
            self.back_cursor.detach();
        } else if self
            .cursor
            .address()
            .ptr_eq(&Some(unsafe { self.bounds.assume_init_ref() }.end()))
        {
            self.cursor.detach();
            self.back_cursor.detach()
        }
    }

    /// Equivalent to `next_back` except *panics* if iteration is done.
    #[inline]
    pub fn advance_back(&mut self) {
        self.back_cursor.advance_back();
        if !self.back_cursor.is_attached() {
            self.cursor.detach();
        } else if self
            .back_cursor
            .address()
            .ptr_eq(&Some(unsafe { self.bounds.assume_init_ref() }.start()))
        {
            self.cursor.detach();
            self.back_cursor.detach()
        }
    }
}

impl<'a, K, V> Iterator for Range<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let key_value = self.peek()?;
        self.advance();
        Some(key_value)
    }
}

impl<'a, K, V> DoubleEndedIterator for Range<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        let key_value = self.peek_back()?;
        self.advance_back();
        Some(key_value)
    }
}

impl<'a, K, V> FusedIterator for Range<'a, K, V> {}
// endregion

// region RangeMut
pub struct RangeMut<'a, K, V> {
    cursor: Cursor<'a, K, V>,
    back_cursor: Cursor<'a, K, V>,
    bounds: MaybeUninit<NodeBounds<K, V>>,
    /// Unlike [Cursor], the reference to `V` is mutable
    _p: PhantomData<(&'a K, &'a mut V)>,
}

//noinspection DuplicatedCode
impl<'a, K, V> RangeMut<'a, K, V> {
    #[inline]
    fn new<Q: Ord + ?Sized>(tree: &'a BTreeMap<K, V>, bounds: impl RangeBounds<Q>) -> Self
    where
        K: Borrow<Q>,
    {
        let bounds = tree.node_bounds(bounds);
        let cursor = match bounds.as_ref().map(|b| b.start()) {
            None => Cursor::new_detached(),
            Some((start_node, start_idx)) => unsafe { Cursor::new(Some(start_node), start_idx) },
        };
        let back_cursor = match bounds.as_ref().map(|b| b.end()) {
            None => Cursor::new_detached(),
            Some((end_node, end_idx)) => unsafe { Cursor::new(Some(end_node), end_idx) },
        };
        let bounds = match bounds {
            None => MaybeUninit::uninit(),
            Some(bounds) => MaybeUninit::new(bounds),
        };
        Self {
            cursor,
            back_cursor,
            bounds,
            _p: PhantomData,
        }
    }

    /// Get the next element without advancing the iterator
    #[inline]
    pub fn peek(&self) -> Option<(&'a K, &'a V)> {
        self.cursor.key_value()
    }

    /// Get the next back element without advancing the back iterator
    #[inline]
    pub fn peek_back(&self) -> Option<(&'a K, &'a V)> {
        self.back_cursor.key_value()
    }

    /// Get the next element without advancing the iterator, with the value reference mutable
    #[inline]
    pub fn peek_mut(&mut self) -> Option<(&'a K, &'a mut V)> {
        unsafe { self.cursor.key_value_mut() }
    }

    /// Get the next back element without advancing the back iterator, with the value reference
    /// mutable
    #[inline]
    pub fn peek_back_mut(&mut self) -> Option<(&'a K, &'a mut V)> {
        unsafe { self.back_cursor.key_value_mut() }
    }

    /// Equivalent to `next` except *panics* if iteration is done.
    #[inline]
    pub fn advance(&mut self) {
        self.cursor.advance();
        if !self.cursor.is_attached() {
            self.back_cursor.detach();
        } else if self
            .cursor
            .address()
            .ptr_eq(&Some(unsafe { self.bounds.assume_init_ref() }.end()))
        {
            self.cursor.detach();
            self.back_cursor.detach()
        }
    }

    /// Equivalent to `next_back` except *panics* if iteration is done.
    #[inline]
    pub fn advance_back(&mut self) {
        self.back_cursor.advance_back();
        if !self.back_cursor.is_attached() {
            self.cursor.detach();
        } else if self
            .back_cursor
            .address()
            .ptr_eq(&Some(unsafe { self.bounds.assume_init_ref() }.start()))
        {
            self.cursor.detach();
            self.back_cursor.detach()
        }
    }
}

impl<'a, K, V> Iterator for RangeMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let key_value = self.peek_mut()?;
        self.advance();
        Some(key_value)
    }
}

impl<'a, K, V> DoubleEndedIterator for RangeMut<'a, K, V> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        let key_value = self.peek_back_mut()?;
        self.advance_back();
        Some(key_value)
    }
}

impl<'a, K, V> FusedIterator for RangeMut<'a, K, V> {}
// endregion
// endregion

#[cfg(feature = "copyable")]
impl<'store, K, V> crate::copyable::sealed::BTree<'store, K, V> for BTreeMap<'store, K, V> {
    #[inline]
    fn assert_store(&self, store: &BTreeStore<K, V>) {
        assert_eq!(
            NonNull::from(self.store),
            NonNull::from(store),
            "b-tree is not from this store"
        );
    }

    #[inline]
    fn nodes(&self) -> crate::copyable::sealed::NodeIter<'store, K, V> {
        crate::copyable::sealed::NodeIter::new(self.root, self.height)
    }
}

unsafe fn as_nullable_ptr<K, V>(ptr: Option<NodePtr<K, V>>) -> *const Node<K, V> {
    match ptr {
        Some(ptr) => ptr.as_ptr().as_ptr(),
        None => std::ptr::null(),
    }
}

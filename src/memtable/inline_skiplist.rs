use crate::common::format::pack_sequence_and_type;
use crate::common::ValueType;
use crate::memtable::concurrent_arena::Arena;
use crate::memtable::MemTableContext;
use crate::util::{encode_var_uint32, get_var_uint32, varint_length};
use rand::{thread_rng, RngCore};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

const MAX_HEIGHT: usize = 12;
const MAX_POSSIBLE_HEIGHT: usize = 32;
const BRANCHING_FACTOR: usize = 4;
const SCALED_INVERSE_BRANCHING: usize = (2147483647usize + 1) / BRANCHING_FACTOR;

#[repr(C)]
struct Node {
    next: [AtomicPtr<Node>; 1],
}

impl Node {
    unsafe fn key(&self) -> *const u8 {
        (self.next.as_ptr() as *const u8).add(std::mem::size_of::<AtomicPtr<Node>>())
    }

    unsafe fn get_next(&self, level: usize) -> *mut Node {
        (*(self.next.as_ptr().sub(level))).load(Ordering::Acquire)
    }

    unsafe fn set_next(&self, level: usize, x: *mut Node) {
        (*(self.next.as_ptr().sub(level))).store(x, Ordering::Release)
    }

    unsafe fn no_barrier_set_next(&self, level: usize, x: *mut Node) {
        (*(self.next.as_ptr().sub(level))).store(x, Ordering::Relaxed)
    }

    unsafe fn cas_next(&self, level: usize, old: *mut Node, x: *mut Node) -> bool {
        (*(self.next.as_ptr().sub(level)))
            .compare_exchange(old, x, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    unsafe fn insert_after(&mut self, level: usize, prev: *mut Node) {
        self.no_barrier_set_next(level, (*prev).get_next(level));
        (*prev).set_next(level, self)
    }
}

pub struct Splice {
    height: usize,
    prev: [*mut Node; MAX_POSSIBLE_HEIGHT],
    next: [*mut Node; MAX_POSSIBLE_HEIGHT],
}

impl Default for Splice {
    fn default() -> Self {
        Self {
            height: 0,
            prev: [null_mut(); MAX_POSSIBLE_HEIGHT],
            next: [null_mut(); MAX_POSSIBLE_HEIGHT],
        }
    }
}

impl Clone for Splice {
    fn clone(&self) -> Self {
        Self {
            height: 0,
            prev: [null_mut(); MAX_POSSIBLE_HEIGHT],
            next: [null_mut(); MAX_POSSIBLE_HEIGHT],
        }
    }
}

pub trait Comparator: Sync {
    fn compare(&self, k1: &[u8], k2: &[u8]) -> std::cmp::Ordering;
    unsafe fn compare_raw_key(&self, k1: *const u8, k2: *const u8) -> std::cmp::Ordering;
    unsafe fn compare_key(&self, k1: *const u8, k2: &[u8]) -> std::cmp::Ordering;
}

pub struct InlineSkipList<C: Comparator, A: Arena> {
    arena: A,
    head: *mut Node,
    max_height: AtomicUsize,
    cmp: C,
}

impl<C: Comparator, A: Arena> InlineSkipList<C, A> {
    pub fn new(arena: A, cmp: C) -> Self {
        let head = unsafe {
            let head = Self::allocate_key_value(&arena, 0, MAX_HEIGHT);
            for i in 0..MAX_HEIGHT {
                (*head).set_next(i, null_mut());
            }
            head
        };

        Self {
            head,
            arena,
            max_height: AtomicUsize::new(1),
            cmp,
        }
    }

    pub fn random_height(&self) -> usize {
        let mut height = 1;
        let mut rng = thread_rng();
        while height < MAX_HEIGHT
            && height < MAX_POSSIBLE_HEIGHT
            && (rng.next_u32() as usize) < SCALED_INVERSE_BRANCHING
        {
            height += 1;
        }
        height
    }

    pub fn add(&self, ctx: &mut MemTableContext, key: &[u8], value: &[u8], sequence: u64) {
        unsafe {
            let (height, node) = self.encode_key_value(
                ctx.get_thread_id(),
                key,
                value,
                sequence,
                ValueType::TypeValue,
            );
            ctx.splice.height = height;
            self.insert(&mut ctx.splice, node);
        }
    }

    pub fn delete(&self, ctx: &mut MemTableContext, key: &[u8], sequence: u64) {
        unsafe {
            let (height, node) = self.encode_key_value(
                ctx.get_thread_id(),
                key,
                &[],
                sequence,
                ValueType::TypeDeletion,
            );
            ctx.splice.height = height;
            self.insert(&mut ctx.splice, node);
        }
    }

    pub fn mem_size(&self) -> usize {
        self.arena.mem_size()
    }

    unsafe fn insert(&self, splice: &mut Splice, x: *mut Node) {
        let mut max_height = self.max_height.load(Ordering::Acquire);
        while splice.height > max_height {
            match self.max_height.compare_exchange_weak(
                max_height,
                splice.height,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    max_height = splice.height;
                    break;
                }
                Err(v) => {
                    max_height = v;
                }
            }
        }
        assert!(max_height <= MAX_POSSIBLE_HEIGHT);
        let key = self.decode_key((*x).key());
        splice.next[max_height] = null_mut();
        splice.prev[max_height] = self.head;
        for i in 0..max_height {
            let idx = max_height - i - 1;
            let (prev, next) =
                self.find_splice_for_level(key, splice.prev[idx + 1], splice.next[idx + 1], idx);
            splice.prev[idx] = prev;
            splice.next[idx] = next;
        }
        for i in 0..splice.height {
            loop {
                (*x).no_barrier_set_next(i, splice.next[i]);
                if (*splice.prev[i]).cas_next(i, splice.next[i], x) {
                    break;
                }
                let (prev, next) = self.find_splice_for_level(key, splice.prev[i], null_mut(), i);
                splice.prev[i] = prev;
                splice.next[i] = next;
            }
        }
    }

    unsafe fn decode_key(&self, k: *const u8) -> &[u8] {
        let mut offset = 0;
        let key = std::slice::from_raw_parts(k, 5);
        let l = get_var_uint32(key, &mut offset).unwrap();
        std::slice::from_raw_parts(k.add(offset), l as usize)
    }

    unsafe fn find_splice_for_level(
        &self,
        key: &[u8],
        mut before: *mut Node,
        after: *mut Node,
        level: usize,
    ) -> (*mut Node, *mut Node) {
        loop {
            let next = (*before).get_next(level);
            if std::ptr::eq(next, after) || !self.key_is_after_node(key, next) {
                return (before, next);
            }
            before = next;
        }
    }

    unsafe fn key_is_after_node(&self, key: &[u8], x: *const Node) -> bool {
        !std::ptr::eq(x, null_mut())
            && self.cmp.compare_key((*x).key(), key) == std::cmp::Ordering::Less
    }
    unsafe fn raw_key_is_after_node(&self, key: *const u8, x: *const Node) -> bool {
        !std::ptr::eq(x, null_mut())
            && self.cmp.compare_raw_key((*x).key(), key) == std::cmp::Ordering::Less
    }

    #[inline(always)]
    unsafe fn allocate_key_value(arena: &A, key_size: usize, height: usize) -> *mut Node {
        let prefix = std::mem::size_of::<AtomicPtr<Node>>() * (height - 1);
        let addr = arena.allocate(prefix + std::mem::size_of::<Node>() + key_size);
        addr.add(prefix) as _
    }

    #[inline(always)]
    unsafe fn encode_key_value(
        &self,
        thread_id: usize,
        key: &[u8],
        value: &[u8],
        sequence: u64,
        tp: ValueType,
    ) -> (usize, *mut Node) {
        let internal_key_size = key.len() + 8;
        let encoded_len = varint_length(internal_key_size)
            + internal_key_size
            + varint_length(value.len())
            + value.len();
        let h = self.random_height();
        let prefix = std::mem::size_of::<AtomicPtr<Node>>() * (h - 1);
        let addr = self.arena.allocate_in_thread(
            thread_id,
            prefix + std::mem::size_of::<Node>() + encoded_len,
        );
        let key_addr = addr.add(prefix + std::mem::size_of::<Node>());
        let data = std::slice::from_raw_parts_mut(key_addr, encoded_len);
        let offset = encode_var_uint32(data, internal_key_size as u32);
        let nxt_offset = offset + key.len();
        data[offset..nxt_offset].copy_from_slice(key);
        data[nxt_offset..(nxt_offset + 8)]
            .copy_from_slice(&pack_sequence_and_type(sequence, tp as u8).to_le_bytes());
        let offset = nxt_offset + 8;
        let offset = encode_var_uint32(&mut data[offset..], value.len() as u32) + offset;
        let nxt_offset = offset + value.len();
        data[offset..nxt_offset].copy_from_slice(value);
        (h, addr.add(prefix) as _)
    }

    unsafe fn find_greater_or_equal(&self, key: *const u8) -> *mut Node {
        let mut level = self.max_height.load(Ordering::Acquire) - 1;
        let key_decoded = self.decode_key(key);
        let mut x = self.head;
        let mut last_bigger = null_mut();
        loop {
            let next = (*x).get_next(level);
            let cmp = if std::ptr::eq(next, null_mut()) || std::ptr::eq(next, last_bigger) {
                std::cmp::Ordering::Greater
            } else {
                self.cmp.compare_key((*next).key(), key_decoded)
            };
            if cmp.is_eq() || (cmp.is_gt() && level == 0) {
                return next;
            } else if cmp.is_lt() {
                x = next;
            } else {
                last_bigger = next;
                level -= 1;
            }
        }
    }

    fn less_than(&self, left: &[u8], right: &[u8]) -> bool {
        self.cmp.compare(left, right) == std::cmp::Ordering::Less
    }

    unsafe fn find_less_than(&self, key: *const u8) -> *mut Node {
        let max_height = self.max_height.load(Ordering::Acquire);
        let mut level = max_height - 1;
        let key_decoded = self.decode_key(key);
        let mut x = self.head;
        let mut last_not_after = null_mut();
        loop {
            let next = (*x).get_next(level);
            if next != last_not_after && self.key_is_after_node(key_decoded, next) {
                x = next;
            } else {
                if level == 0 {
                    return x;
                } else {
                    last_not_after = next;
                    level -= 1;
                }
            }
        }
    }
}

pub struct SkipListIterator<C: Comparator, A: Arena> {
    list: *const InlineSkipList<C, A>,
    node: *mut Node,
    current_offset: usize,
    current_key_size: usize,
}

pub fn encode_key<'a>(buf: &'a mut Vec<u8>, target: &[u8]) -> &'a [u8] {
    buf.clear();
    let mut tmp: [u8; 5] = [0u8; 5];
    let offset = encode_var_uint32(&mut tmp, target.len() as u32);
    buf.extend_from_slice(&tmp[..offset]);
    buf.extend_from_slice(target);
    buf.as_slice()
}

impl<C: Comparator, A: Arena> SkipListIterator<C, A> {
    pub fn new(list: *const InlineSkipList<C, A>) -> Self {
        Self {
            list,
            node: null_mut(),
            current_key_size: 0,
            current_offset: 0,
        }
    }
    pub fn key(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                (*self.node).key().add(self.current_offset),
                self.current_key_size,
            )
        }
    }

    pub fn seek(&mut self, buf: &mut Vec<u8>, key: &[u8]) {
        unsafe {
            let target = encode_key(buf, key);
            self.node = (*self.list).find_greater_or_equal(target.as_ptr());
            self.init_offset();
        }
    }

    pub fn seek_for_prev(&mut self, buf: &mut Vec<u8>, key: &[u8]) {
        self.seek(buf, key);
        if !self.valid() {
            self.seek_to_last();
        }
        unsafe {
            self.init_offset();
            while self.valid() && (*self.list).less_than(key, self.key()) {
                self.prev();
            }
        }
    }

    pub fn valid(&self) -> bool {
        !std::ptr::eq(self.node, null_mut())
    }

    pub fn next(&mut self) {
        unsafe {
            self.node = (*self.node).get_next(0);
            self.init_offset();
        }
    }

    pub fn prev(&mut self) {
        unsafe {
            self.node = (*self.list).find_less_than((*self.node).key());
            if self.node == (*self.list).head {
                self.node = null_mut();
            }
            self.init_offset();
        }
    }

    pub fn seek_to_first(&mut self) {
        unsafe {
            self.node = (*(*self.list).head).get_next(0);
            self.init_offset();
        }
    }

    pub fn seek_to_last(&mut self) {
        unsafe {
            let mut x = (*self.list).head;
            let mut level = (*self.list).max_height.load(Ordering::Acquire) - 1;
            loop {
                let nxt = (*x).get_next(level);
                if nxt.is_null() {
                    if level == 0 {
                        break;
                    } else {
                        level -= 1;
                    }
                } else {
                    x = nxt;
                }
            }
            if !std::ptr::eq(x, (*self.list).head) {
                self.node = x;
            } else {
                self.node = null_mut();
            }
            self.init_offset();
        }
    }

    pub fn value(&self) -> &[u8] {
        unsafe {
            let mut current_value_offset = 0;
            let current_value_size = get_var_uint32(
                std::slice::from_raw_parts(
                    (*self.node)
                        .key()
                        .add(self.current_offset + self.current_key_size),
                    5,
                ),
                &mut current_value_offset,
            )
            .unwrap() as usize;
            std::slice::from_raw_parts(
                (*self.node)
                    .key()
                    .add(self.current_offset + self.current_key_size + current_value_offset),
                current_value_size,
            )
        }
    }

    #[inline(always)]
    unsafe fn init_offset(&mut self) {
        if self.valid() {
            self.current_key_size = *((*self.node).key()) as usize;
            if self.current_key_size < 128 {
                self.current_offset = 1;
            } else {
                self.current_offset = 0;
                self.current_key_size = get_var_uint32(
                    std::slice::from_raw_parts((*self.node).key(), 5),
                    &mut self.current_offset,
                )
                .unwrap() as usize;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{extract_user_key, InternalKeyComparator, VALUE_TYPE_FOR_SEEK};
    use crate::memtable::concurrent_arena::SharedArena;
    use crate::memtable::skiplist_rep::DefaultComparator;

    #[test]
    fn test_find_near() {
        let comp = InternalKeyComparator::default();
        let list = InlineSkipList::new(SharedArena::new(), DefaultComparator::new(comp.clone()));
        let mut ctx = MemTableContext::default();
        let v = vec![1u8; 100];
        for i in 0..10000 {
            let k = i.to_string().into_bytes();
            list.add(&mut ctx, &k, &v, i);
        }
        let mut buf = vec![];
        let mut tmp = Vec::with_capacity(20);
        let mut keys = vec![];
        for i in 0..10000 {
            let k = i.to_string().into_bytes();
            buf.clear();
            buf.extend_from_slice(&k);
            buf.extend_from_slice(
                &pack_sequence_and_type(10000, VALUE_TYPE_FOR_SEEK).to_le_bytes(),
            );
            let mut iter = SkipListIterator::new(&list);
            iter.seek(&mut tmp, &buf);
            assert!(iter.valid());
            let key = iter.key();
            assert_eq!(key[..(key.len() - 8)].to_vec(), k);
            keys.push(k);
        }
        keys.sort();
        let mut iter = SkipListIterator::new(&list);
        iter.seek_to_last();
        assert!(iter.valid());
        let mut count = 0;
        for k in keys.iter().rev() {
            let key = iter.key();
            let user_key = extract_user_key(key);
            let a = String::from_utf8(user_key.to_vec()).unwrap();
            let b = String::from_utf8(k.to_vec()).unwrap();
            assert_eq!(user_key, k, "the {}th failed, {} compare {}", count, a, b);
            iter.prev();
            count += 1;
        }
    }
}

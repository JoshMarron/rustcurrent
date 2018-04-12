use memory::HPBRManager;
use std::sync::atomic::{AtomicPtr, AtomicBool, AtomicUsize, Ordering};
use std::fmt::Debug;
use std::fmt;
use std::ptr;
use std::mem;
use std::marker::PhantomData;
use rand;
use rand::Rng;

/// A lock-free k-FIFO segmented queue.
///
/// This is an implementation of a k-FIFO queue as described in [Fast and Scalable k-FIFO Queues]
/// (https://link.springer.com/chapter/10.1007/978-3-642-39958-9_18). The idea behind the k-FIFO
/// queue is to relax the consistency requirement of the queue so that any of the `k` first
/// enqueued elements can be the first to be dequeued. This allows for greater scalability since
/// there is less contention, and the drift from normal queue semantics is bounded by `k`.
///
/// The queue is implemented as a linked-list of nodes, each containing an array of elements. 
/// Once a node is full, a new one is enqueued. Once a node is empty, it is dequeued and freed.
/// 
/// If relaxed consistency is undesirable, do not set `k` to 1. Instead, use the Queue structure
/// from the `rustcurrent` library as it is far better optimised for that scenario.
pub struct SegQueueOld<T: Send> {
    head: AtomicPtr<Segment<T>>,
    tail: AtomicPtr<Segment<T>>,
    manager: HPBRManager<Segment<T>>,
    k: usize
}

impl<T: Send> SegQueueOld<T> {
    /// Create a new SegQueueOld with a given node size.
    /// # Examples
    /// ```
    /// let queue: SegQueueOld<u8> = SegQueueOld::new(8);
    /// ```
    pub fn new(k: usize) -> Self {
        let init_node: *mut Segment<T> = Box::into_raw(Box::new(Segment::new(k)));
        SegQueueOld {
            head: AtomicPtr::new(init_node),
            tail: AtomicPtr::new(init_node),
            manager: HPBRManager::new(100, 3),
            k
        }
    }

    /// Enqueue the given data.
    /// # Examples
    /// ```
    /// let queue: SegQueueOld<u8> = SegQueueOld::new(8);
    /// queue.enqueue(8);
    /// ``` 
    pub fn enqueue(&self, data: T) {
        let mut vec: Vec<usize> = (0..self.k).collect();
        let vals = vec.as_mut_slice();
        let mut data_box = Box::new(Some(data));
        loop {
            data_box = match self.try_enqueue(data_box, vals) {
                Ok(()) => { return; },
                Err(val) => val
            };    
        }
    }

    fn try_enqueue(&self, data: Box<Option<T>>, vals: &mut[usize]) -> Result<(), Box<Option<T>>> {
        let tail = self.tail.load(Ordering::Acquire);
        self.manager.protect(tail, 0);

        if !ptr::eq(tail, self.tail.load(Ordering::Acquire)) {
            self.manager.unprotect(0);
            return Err(data);
        }

        let mut rng = rand::thread_rng();
        rng.shuffle(vals);
        
        if let Ok((index, old_ptr)) = self.find_empty_slot(tail, vals) {
            if ptr::eq(tail, self.tail.load(Ordering::Acquire)) {
                let data_ptr = Box::into_raw(data);
                unsafe {
                    match (*tail).data[index].compare_exchange_weak(old_ptr, data_ptr, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(old) => {
                            // Use the committed function to check the addition or reverse it
                            // This needs to be done because of a data race with dequeuing advancing the head
                            // Free the old data
                            return match self.commit(tail, data_ptr, index) {
                                true => {
                                    Box::from_raw(old);
                                    Ok(())
                                },
                                false => Err(Box::from_raw(data_ptr)) 
                            }
                        },
                        Err(_) => {
                            return Err(Box::from_raw(data_ptr))
                        }
                    }
                }
            } else {
                // The tail has changed so we should not try an insertion
                return Err(data)
            }
        } else {
            // Advance the tail, either by adding the new block or adjusting the tail
            self.advance_tail(tail);
            return Err(data)
        }
    }

    unsafe fn commit(&self, tail_old: *mut Segment<T>, item_ptr: *mut Option<T>, index: usize) -> bool {
        if !ptr::eq((*tail_old).data[index].load(Ordering::Acquire), item_ptr) {
            // Already dequeued
            return true;
        }
        let head = self.head.load(Ordering::Acquire);
        let new_none_ptr: *mut Option<T> = Box::into_raw(Box::new(None));

        if (*tail_old).deleted.load(Ordering::Acquire) {
            return match (*tail_old).data[index].compare_exchange(item_ptr, new_none_ptr, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => false,
                Err(_) => {
                    Box::from_raw(new_none_ptr);
                    true
                } 
            }
        } else if ptr::eq(head, tail_old) {
            return match self.head.compare_exchange(head, head, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    Box::from_raw(new_none_ptr);
                    true
                },
                Err(_) => {
                    return match (*tail_old).data[index].compare_exchange(item_ptr, new_none_ptr, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => {
                            false
                        },
                        Err(_) => {
                            Box::from_raw(new_none_ptr);
                            true
                        }
                    }  
                }
            }
        } else if !(*tail_old).deleted.load(Ordering::Acquire) {
            return true
        } else {
            return match (*tail_old).data[index].compare_exchange(item_ptr, new_none_ptr, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => false,
                Err(_) => {
                    Box::from_raw(new_none_ptr);
                    true
                }
            }
        }
    }

    /// Attempt to dequeue a piece of data, returning None if the queue is empty. If
    /// the front segment is empty, it will be dequeued.
    /// # Examples
    /// ```
    /// let queue: SegQueueOld<u8> = SegQueueOld::new(8);
    /// queue.enqueue(8);
    /// assert_eq!(queue.dequeue(), Some(8));
    /// ```
    pub fn dequeue(&self) -> Option<T> {
        let mut vec: Vec<usize> = (0..self.k).collect();
        let vals = vec.as_mut_slice();
        loop {
            if let Ok(val) = self.try_dequeue(vals) {
                return val
            }
        }
    }

    fn try_dequeue(&self, vals: &mut[usize]) -> Result<Option<T>, ()> {
        let head = self.head.load(Ordering::Acquire);
        self.manager.protect(head, 0);
        if !ptr::eq(head, self.head.load(Ordering::Acquire)) {
            return Err(())
        }
        
        let mut rng = rand::thread_rng();
        rng.shuffle(vals);
        let found = self.find_item(head, vals);
        let tail = self.tail.load(Ordering::Acquire);

        if ptr::eq(head, self.head.load(Ordering::Acquire)) {
            match found {
                Ok((index, item_ptr)) => {
                    if ptr::eq(head, tail) {
                        self.advance_tail(tail);
                    };
                    let new_none_ptr: *mut Option<T> = Box::into_raw(Box::new(None));
                    unsafe {
                        return match (*head).data[index].compare_exchange(item_ptr, new_none_ptr, Ordering::AcqRel, Ordering::Acquire) {
                            Ok(_) => {
                                let data = ptr::replace(item_ptr, None);
                                Box::from_raw(item_ptr);
                                Ok(data)
                            },
                            Err(_) => {
                                Box::from_raw(new_none_ptr);
                                Err(())
                            }
                        }
                    }
                },
                Err(()) => {
                    /* if ptr::eq(head, tail) && ptr::eq(tail, self.tail.load(Ordering::Acquire)) {
                        return Ok(None)
                    } */
                    if unsafe{ (*head).next.load(Ordering::Acquire).is_null() } {
                        return Ok(None)
                    }
                    self.advance_head(head);
                    return Err(())
                }
            }
        }
        Err(())
    }

    fn find_empty_slot(&self, node_ptr: *mut Segment<T>, order: &[usize]) -> Result<(usize, *mut Option<T>), ()> {
        unsafe {
            let node = &*node_ptr;
            for i in order {
                let old_ptr = node.data[*i].load(Ordering::Acquire);
                match *old_ptr {
                    Some(_) => {},
                    None => {return Ok((*i, old_ptr));}
                }
            }
        }
        
        Err(())
    }

    fn find_item(&self, node_ptr: *mut Segment<T>, order: &[usize]) -> Result<(usize, *mut Option<T>), ()> {
        unsafe {
            let node = &*node_ptr;
            for i in order {
                let old_ptr = node.data[*i].load(Ordering::Acquire);
                match *old_ptr {
                    Some(_) => { return Ok((*i, old_ptr))},
                    None => {}
                }
            }
        }
        
        Err(())
    }

    fn advance_tail(&self, old_tail: *mut Segment<T>) {
        let tail_current = self.tail.load(Ordering::Acquire);
        if ptr::eq(tail_current, old_tail) {
            unsafe {
                let next = (*old_tail).next.load(Ordering::Acquire);
                if ptr::eq(old_tail, self.tail.load(Ordering::Acquire)) {
                    if next.is_null() {
                        // Create a new tail segment and advance if possible
                        let new_seg_ptr: *mut Segment<T> = Box::into_raw(Box::new(Segment::new(self.k)));
                        match (*old_tail).next.compare_exchange(next, new_seg_ptr, Ordering::AcqRel, Ordering::Acquire) {
                            Ok(_) => { let _ = self.tail.compare_exchange(old_tail, new_seg_ptr, Ordering::AcqRel, Ordering::Acquire); },
                            Err(_) => { Box::from_raw(new_seg_ptr); } // Delete the unused new segment if we can't swap in
                        }
                    } else {
                        // Advance tail, because it is out of sync somehow
                        let _ = self.tail.compare_exchange(old_tail, next, Ordering::AcqRel, Ordering::Acquire);
                    }
                }
            }
        }
    }

    fn advance_head(&self, old_head: *mut Segment<T>) {
        let head = self.head.load(Ordering::Acquire);
        // Head doesn't need protecting, we ONLY use it if it's equal to old_head, which should be protected already
        if ptr::eq(head, old_head) {
            let tail = self.tail.load(Ordering::Acquire);
            unsafe {
                let tail_next = (*tail).next.load(Ordering::Acquire);
                let head_next = (*head).next.load(Ordering::Acquire);
                if ptr::eq(head, self.head.load(Ordering::Acquire)) {
                    if ptr::eq(tail, head) {
                        if tail_next.is_null() {
                            // Queue only has one segment, so we don't remove it
                            return;
                        } 
                        if ptr::eq(tail, self.tail.load(Ordering::Acquire)) {
                            // Set the tail to point to the next block, so the queue has two segments
                            let _ = self.tail.compare_exchange(tail, tail_next, Ordering::AcqRel, Ordering::Acquire);
                        }
                    }
                    // TODO: Set the head to be deleted, might need for the commit function
                    // Advance the head and retire old_head
                    match self.head.compare_exchange(head, head_next, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => {
                            (*head).deleted.store(true, Ordering::Release);
                            self.manager.retire(head, 0);
                        },
                        Err(_) => {}
                    }
                }
            }
        }
    }
}

impl<T: Send> Drop for SegQueueOld<T> {
    fn drop(&mut self) {
        let mut current = self.head.load(Ordering::Relaxed);
        while !current.is_null() {
            unsafe {
                let next = (*current).next.load(Ordering::Relaxed);
                Box::from_raw(current);
                current = next;
            }
        }
    }
}

impl<T: Send + Debug> Debug for SegQueueOld<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut start_ptr = self.head.load(Ordering::Relaxed);
        let mut node_string = "[".to_owned();
        unsafe {
            while !start_ptr.is_null() {
                node_string.push_str(&format!("\n\t{:?}", *start_ptr));
                start_ptr = (*start_ptr).next.load(Ordering::Relaxed);
            }
        }
        node_string += "]";
        write!(f, "SegQueueOld{{ {} }}", node_string)
    }
}

struct Segment<T: Send> {
    data: Vec<AtomicPtr<Option<T>>>,
    next: AtomicPtr<Segment<T>>,
    deleted: AtomicBool
}   

impl<T: Send> Segment<T> {
    fn new(k: usize) -> Self {
        let mut data = Vec::new();
        for _ in 0..k {
            data.push(AtomicPtr::new(Box::into_raw(Box::new(None))));
        }
        Segment {
            data,
            next: AtomicPtr::default(),
            deleted: AtomicBool::new(false)
        }
    }
}

impl<T: Send + Debug> Debug for Segment<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut vals_string = "[".to_owned();
        unsafe {
            for atom_ptr in &self.data {
                let ptr = atom_ptr.load(Ordering::Relaxed);
                if !ptr.is_null() {
                    vals_string.push_str(&format!("({:?}: {:?})", atom_ptr, *ptr));
                }
            }
        }
        vals_string += "]";
        write!(f, "Node {{ Values: {}, Next: {:?} }}", &vals_string, self.next)
    }
}

impl<T: Send> Drop for Segment<T> {
    fn drop(&mut self) {
        let vec = mem::replace(&mut self.data, Vec::new());
        for a_ptr in vec {
            let ptr = a_ptr.load(Ordering::Relaxed);
            unsafe {
                Box::from_raw(ptr);
            }
        }
    }
}

struct AtomicMarkablePtr<T> {
    ptr: AtomicUsize,
    _marker: PhantomData<T>
}

impl<T: Send> AtomicMarkablePtr<T> {
    fn compare_and_mark(&self, old: *mut T) -> Result<*mut T, *mut T> {
        let marked = mark(old);
        match self.ptr.compare_exchange(old as usize, marked as usize, Ordering::Acquire, Ordering::Relaxed) {
            Ok(ptr) => Ok(ptr as *mut T),
            Err(ptr) => Err(ptr as *mut T)
        }
    }

    fn compare_exchange(&self, current: *mut T, new: *mut T) -> Result<*mut T, *mut T> {
        match self.ptr.compare_exchange(current as usize, new as usize, Ordering::Acquire, Ordering::Relaxed) {
            Ok(ptr) => Ok(ptr as *mut T),
            Err(ptr) => Err(ptr as *mut T)
        }
    }
}

impl<T: Send> Default for AtomicMarkablePtr<T> {
    fn default() -> Self {
        AtomicMarkablePtr {
            ptr: AtomicUsize::new(0),
            _marker: PhantomData
        }
    }
}

pub fn is_marked<T>(ptr: *mut T) -> bool {
    let ptr_usize = ptr as usize;
    match ptr_usize & 0x1 {
        0 => false,
        _ => true,
    }
}

pub fn unmark<T>(ptr: *mut T) -> *mut T {
    let ptr_usize = ptr as usize;
    (ptr_usize & !(0x1)) as *mut T
}

pub fn mark<T>(ptr: *mut T) -> *mut T {
    let ptr_usize = ptr as usize;
    (ptr_usize | 0x1) as *mut T
}

mod tests {
    #![allow(unused_imports)]
    use super::SegQueueOld;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    #[test]
    #[ignore]
    fn test_enqueue() {
        let queue: SegQueueOld<u8> = SegQueueOld::new(4);

        let mut poss_set: HashSet<u8> = HashSet::new();

        queue.enqueue(3);
        poss_set.insert(3);
        queue.enqueue(4);
        poss_set.insert(4);
        queue.enqueue(5);
        poss_set.insert(5);
        queue.enqueue(6);
        poss_set.insert(6);

        queue.enqueue(7);

        println!("{:?}", queue);
        
        let res = queue.dequeue().unwrap();
        assert!(poss_set.contains(&res));
        poss_set.remove(&res);

        let res = queue.dequeue().unwrap();
        assert!(poss_set.contains(&res));
        poss_set.remove(&res);

        let res = queue.dequeue().unwrap();
        assert!(poss_set.contains(&res));
        poss_set.remove(&res);

        let res = queue.dequeue().unwrap();
        assert!(poss_set.contains(&res));
        poss_set.remove(&res);

        println!("{:?}", queue);

        assert_eq!(Some(7), queue.dequeue());
        assert_eq!(None, queue.dequeue());

        println!("{:?}", queue);
    }

    #[test]
    #[ignore]
    fn test_with_contention() {
        let mut queue: Arc<SegQueueOld<u16>> = Arc::new(SegQueueOld::new(20));
        
        let mut waitvec: Vec<thread::JoinHandle<()>> = Vec::new();

        for thread_no in 0..20 {
            let mut queue_copy = queue.clone();
            waitvec.push(thread::spawn(move || {
                for i in 0..10000 {
                    queue_copy.enqueue(i);
                }
                //println!("Push thread {} complete", i);
            }));
            queue_copy = queue.clone();
            waitvec.push(thread::spawn(move || {
                for i in 0..10000 {
                    let mut num = 0;
                    loop {
                        match queue_copy.dequeue() {
                            Some(_) => {num = 0; break},
                            None => {
                                num += 1;
                                if num > 1000 {
                                    //println!("{:?}", queue_copy);
                                    println!("{}", num);
                                    num = 0;
                                }
                            } 
                        }
                    }
                }
                println!("Pop thread {} complete", thread_no);
            }));
        }

        for handle in waitvec {
            match handle.join() {
                Ok(_) => {},
                Err(some) => println!("Couldn't join! {:?}", some) 
            }
        }
        println!("Joined all");
        assert_eq!(None, queue.dequeue());
    }
}
//! A collection of lock-free or wait-free data structures.
//!
//! All the data structures in the collection use the HPBRManager for memory
//! management as a proof-of-concept. They are all implemented from the papers cited
//! in their individual struct-level pages.

pub use self::stack::Stack;
pub use self::queue::Queue; 
pub use self::seg_queue::SegQueue;
pub use self::hash_map::HashMap;

mod stack;
mod queue;
mod seg_queue;
mod hash_map;
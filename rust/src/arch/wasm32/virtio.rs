//! virtio devices: the browser has none.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::virtio::{Kind, Transport};

pub fn find(_kind: Kind) -> Vec<Box<dyn Transport>> {
    Vec::new()
}

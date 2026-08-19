//! The Tonie Audio Format itself — header codec, Ogg framing, packet padding, and
//! reader/writer interfaces — with nothing about how audio gets into a TAF file.

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod digest;
pub mod header;
pub mod id;
pub mod ogg;
#[cfg(feature = "alloc")]
pub mod opus_packet;
pub mod reader;
#[cfg(feature = "alloc")]
pub mod writer;

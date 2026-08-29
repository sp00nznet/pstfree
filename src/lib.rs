//! Reading, exporting and repairing Outlook PST and OST files.
//!
//! Layered the way the format is: [`ndb`] is blocks and B-trees, [`ltp`] is heaps,
//! properties and tables on top of them, [`export`] turns what comes out into mail, and
//! [`crypt`] and [`cfbf`] are the two self-contained formats needed along the way.

/// What a long job calls as it goes: how much of it is done, out of how much.
///
/// One shape for every long job, because both front ends want the same thing out of all
/// of them — something on screen that changes, so that rebuilding a 40GB mailbox does not
/// look like a hang. Called once per unit; throttling is the caller's business, since a
/// terminal and a status bar want it at different rates.
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

pub mod cfbf;
pub mod convert;
pub mod crypt;
pub mod export;
pub mod ltp;
pub mod ndb;
pub mod repair;

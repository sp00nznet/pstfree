//! Reading, exporting and repairing Outlook PST and OST files.
//!
//! Layered the way the format is: [`ndb`] is blocks and B-trees, [`ltp`] is heaps,
//! properties and tables on top of them, [`export`] turns what comes out into mail, and
//! [`crypt`] and [`cfbf`] are the two self-contained formats needed along the way.

pub mod cfbf;
pub mod crypt;
pub mod export;
pub mod ltp;
pub mod ndb;
pub mod repair;

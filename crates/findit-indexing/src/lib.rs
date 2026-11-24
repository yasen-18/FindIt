//! Windows scanners for FindIt.
//!
//! * [`walk`]: standard mode, parallel `FindFirstFileExW` folder walk
//! * [`watch`] + [`apply`]: live updates with `ReadDirectoryChangesW`
//! * [`usn`] + [`fast`]: NTFS fast mode (MFT enumeration and USN journal)
//! * [`pipe`] + [`proto`]: the elevated helper process and its locked-down pipe
//! * [`drives`]: drive discovery

#![cfg(windows)]

pub mod apply;
pub mod drives;
pub mod fast;
pub mod pipe;
pub mod proto;
pub mod usn;
pub mod walk;
pub mod watch;
pub mod win;

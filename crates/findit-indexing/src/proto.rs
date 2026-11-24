//! Messages between the elevated helper and the app.
//!
//! Frames are a little-endian `u32` length followed by a JSON body. The receiver
//! rejects frames over [`MAX_FRAME`] and anything that does not parse, so a broken
//! or hostile peer cannot make the app allocate unbounded memory.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 64 << 20;

/// One record from the master file table enumeration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsnItem {
    #[serde(rename = "f")]
    pub frn: u64,
    #[serde(rename = "p")]
    pub parent: u64,
    #[serde(rename = "a")]
    pub attrs: u32,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Vec<u16>>,
}

/// An additional hard-link name of a file (NTFS lets one file live in several folders).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkName {
    #[serde(rename = "p")]
    pub parent: u64,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Vec<u16>>,
}

/// Size and date for one file, keyed by file reference number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaItem {
    #[serde(rename = "f")]
    pub frn: u64,
    #[serde(rename = "s")]
    pub size: u64,
    #[serde(rename = "m")]
    pub mtime: i64,
    #[serde(rename = "a")]
    pub attrs: u32,
    /// Set when this listing found the file under another name or folder.
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkName>,
}

/// The latest state of one file after a batch of journal records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeItem {
    #[serde(rename = "f")]
    pub frn: u64,
    #[serde(rename = "p")]
    pub parent: u64,
    #[serde(rename = "a")]
    pub attrs: u32,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "r", default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Vec<u16>>,
    #[serde(rename = "d")]
    pub deleted: bool,
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(rename = "m", default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum HelperMsg {
    Hello {
        version: u32,
    },
    /// Start of a volume. When `resumed` is false the app must drop what it has for
    /// this volume: a full enumeration follows.
    #[serde(rename_all = "camelCase")]
    Journal {
        vol: char,
        journal_id: u64,
        next_usn: i64,
        root_frn: u64,
        resumed: bool,
    },
    Records {
        vol: char,
        items: Vec<UsnItem>,
    },
    EnumDone {
        vol: char,
    },
    Meta {
        vol: char,
        items: Vec<MetaItem>,
    },
    MetaDone {
        vol: char,
    },
    #[serde(rename_all = "camelCase")]
    Changes {
        vol: char,
        items: Vec<ChangeItem>,
        next_usn: i64,
    },
    VolumeError {
        vol: char,
        message: String,
    },
    Fatal {
        message: String,
    },
}

pub fn write_msg<W: Write>(w: &mut W, msg: &HelperMsg) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_msg<R: Read>(r: &mut R) -> io::Result<HelperMsg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame length"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_limits() {
        let msgs = vec![
            HelperMsg::Hello { version: PROTOCOL_VERSION },
            HelperMsg::Journal { vol: 'C', journal_id: 9, next_usn: 100, root_frn: 0x5_0000_0000_0005, resumed: false },
            HelperMsg::Records {
                vol: 'C', items: vec![UsnItem { frn: 1, parent: 5, attrs: 0x10, name: "Документы".into(), raw: None }]
            },
            HelperMsg::Changes {
                vol: 'C',
                items: vec![ChangeItem {
                    frn: 2,
                    parent: 5,
                    attrs: 0,
                    name: "a".into(),
                    raw: Some(vec![0xD800]),
                    deleted: false,
                    size: Some(3),
                    mtime: None,
                }],
                next_usn: 200,
            },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_msg(&mut buf, m).unwrap();
        }
        let mut cur = io::Cursor::new(buf);
        for m in &msgs {
            assert_eq!(&read_msg(&mut cur).unwrap(), m);
        }
        let mut huge = io::Cursor::new(((MAX_FRAME + 1) as u32).to_le_bytes().to_vec());
        assert!(read_msg(&mut huge).is_err());
        let mut junk = Vec::new();
        junk.extend_from_slice(&5u32.to_le_bytes());
        junk.extend_from_slice(b"{bad}");
        assert!(read_msg(&mut io::Cursor::new(junk)).is_err());
    }
}

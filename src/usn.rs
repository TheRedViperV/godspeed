//! Live updates: reads the NTFS change journal, which records every create,
//! rename and delete on the volume, so the index can follow along.

use crate::index::filetime_to_unix;
use crate::ntfs::{le16, le32, le64, ATTR_HIDDEN, ATTR_SYSTEM};
use crate::win;
use std::fs::{File, OpenOptions};
use std::os::windows::io::AsRawHandle;

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
const REASON_FILE_DELETE: u32 = 0x0000_0200;
const ATTR_DIRECTORY: u32 = 0x10;
const RECORD_MASK: u64 = 0xFFFF_FFFF_FFFF;

/// What a file or folder looks like after a change (or that it's gone).
pub struct Change {
    /// MFT record number.
    pub index: u32,
    pub parent: u32,
    pub name: String,
    pub dir: bool,
    /// Hidden or system attribute.
    pub hidden: bool,
    pub deleted: bool,
    pub time: u32,
}

pub struct Cursor {
    file: File,
    journal: u64,
    next: i64,
}

#[repr(C)]
struct ReadData {
    start: i64,
    reason_mask: u32,
    only_on_close: u32,
    timeout: u64,
    wait_bytes: u64,
    journal: u64,
}

impl Cursor {
    /// Starts at the current end of the journal.
    pub fn open(letter: char) -> Result<Cursor, String> {
        let file = OpenOptions::new().read(true).open(format!(r"\\.\{letter}:")).map_err(|e| e.to_string())?;
        #[repr(C, align(8))]
        struct Buf([u8; 128]);
        let mut out = Buf([0; 128]);
        let mut ret = 0u32;
        let ok = unsafe {
            win::DeviceIoControl(
                file.as_raw_handle() as _,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                out.0.as_mut_ptr() as _,
                out.0.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || ret < 24 {
            return Err(format!("no change journal: {}", std::io::Error::last_os_error()));
        }
        Ok(Cursor { file, journal: le64(&out.0, 0), next: le64(&out.0, 16) as i64 })
    }

    /// Everything that changed since the last read. Doesn't wait for new changes.
    pub fn read(&mut self) -> Result<Vec<Change>, String> {
        #[repr(C, align(8))]
        struct Buf([u8; 1 << 16]);
        let mut out = Box::new(Buf([0; 1 << 16]));
        let mut changes = Vec::new();
        for _ in 0..512 {
            // Only "closed" records: one per file once whoever changed it lets go,
            // carrying the final name rather than every step along the way.
            let input = ReadData {
                start: self.next,
                reason_mask: u32::MAX,
                only_on_close: 1,
                timeout: 0,
                wait_bytes: 0,
                journal: self.journal,
            };
            let mut ret = 0u32;
            let ok = unsafe {
                win::DeviceIoControl(
                    self.file.as_raw_handle() as _,
                    FSCTL_READ_USN_JOURNAL,
                    &input as *const ReadData as _,
                    std::mem::size_of::<ReadData>() as u32,
                    out.0.as_mut_ptr() as _,
                    out.0.len() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let ret = (ret as usize).min(out.0.len());
            if ret < 8 {
                break;
            }
            let next = le64(&out.0, 0) as i64;
            let mut off = 8;
            while off + 60 <= ret {
                let b = &out.0[off..ret];
                let len = le32(b, 0) as usize;
                if len < 60 || len > b.len() {
                    break;
                }
                if le16(b, 4) == 2 {
                    let (nlen, noff) = (le16(b, 56) as usize, le16(b, 58) as usize);
                    if noff + nlen <= len {
                        let units = b[noff..noff + nlen].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
                        changes.push(Change {
                            index: (le64(b, 8) & RECORD_MASK) as u32,
                            parent: (le64(b, 16) & RECORD_MASK) as u32,
                            name: char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect(),
                            dir: le32(b, 52) & ATTR_DIRECTORY != 0,
                            hidden: le32(b, 52) & (ATTR_HIDDEN | ATTR_SYSTEM) != 0,
                            deleted: le32(b, 40) & REASON_FILE_DELETE != 0,
                            time: filetime_to_unix(le64(b, 32)),
                        });
                    }
                }
                off += len;
            }
            let advanced = next != self.next;
            self.next = next;
            if !advanced || ret <= 8 {
                break;
            }
        }
        Ok(changes)
    }
}

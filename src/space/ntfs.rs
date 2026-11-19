//! Fast scanner: reads the NTFS Master File Table straight off the volume.
//!
//! Every file and folder on an NTFS volume has a record in the MFT, which holds its
//! name, parent folder and size. Reading the table sequentially is far faster than
//! asking Windows to list every folder. Needs administrator rights.

use crate::space::tree::*;
use crate::space::win;
use rayon::prelude::*;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;
const ROOT_RECORD: u32 = 5;
const CHUNK: usize = 8 << 20;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;

struct Volume {
    bytes_per_sector: u64,
    bytes_per_cluster: u64,
    record_size: usize,
    mft_valid: u64,
    mft_lcn: u64,
}

#[inline]
fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline]
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub fn scan(letter: char, progress: &Progress, cancel: &AtomicBool) -> Result<Tree, String> {
    let device = format!(r"\\.\{letter}:");
    let file = OpenOptions::new().read(true).open(&device).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            "fast scan needs administrator rights".to_string()
        } else {
            format!("couldn't open the volume ({e})")
        }
    })?;
    let vol = volume_data(&file)?;
    let rs = vol.record_size;
    if rs < 512 || rs > 65536 {
        return Err("unexpected MFT record size".into());
    }

    let mut rec0 = vec![0u8; rs.max(vol.bytes_per_sector as usize)];
    read_exact_at(&file, &mut rec0, vol.mft_lcn * vol.bytes_per_cluster)?;
    let rec0 = &mut rec0[..rs];
    if &rec0[0..4] != b"FILE" || !apply_fixup(rec0) {
        return Err("unexpected MFT layout".into());
    }
    let runs = mft_runs(rec0, vol.bytes_per_cluster).ok_or("MFT is too fragmented to read directly")?;
    let run_bytes: u64 = runs.iter().map(|r| r.1).sum::<u64>() * vol.bytes_per_cluster;
    let sector = vol.bytes_per_sector.max(512);
    let total = vol.mft_valid.div_ceil(sector).saturating_mul(sector).min(run_bytes);
    let records = (vol.mft_valid.min(run_bytes) / rs as u64) as usize;

    progress.phase.store(PH_MFT, Relaxed);
    progress.total.store(total, Relaxed);

    let mut nodes: Vec<Node> = Vec::new();
    nodes.resize_with(records, Node::default);
    let mut base = vec![u32::MAX; records];

    let (tx, rx) = mpsc::sync_channel::<(usize, Vec<u8>)>(3);
    std::thread::scope(|s| -> Result<(), String> {
        let (file, runs) = (&file, &runs);
        let cluster = vol.bytes_per_cluster;
        let reader = s.spawn(move || read_mft(file, runs, cluster, total, rs, tx, cancel));
        for (first, mut buf) in rx {
            let end = (first + buf.len() / rs).min(records);
            if first >= end {
                continue;
            }
            let n = end - first;
            buf[..n * rs]
                .par_chunks_mut(rs)
                .zip(nodes[first..end].par_iter_mut())
                .zip(base[first..end].par_iter_mut())
                .for_each(|((rec, node), b)| parse_record(rec, node, b));
            progress.done.fetch_add((n * rs) as u64, Relaxed);
            progress.items.fetch_add(n as u64, Relaxed);
        }
        reader.join().map_err(|_| "MFT reader crashed".to_string())?
    })?;
    if cancel.load(Relaxed) {
        return Err("Cancelled".into());
    }

    progress.phase.store(PH_ANALYZE, Relaxed);
    merge_extensions(&mut nodes, &base);
    let root = ROOT_RECORD as usize;
    if root >= nodes.len() || nodes[root].kind != DIR {
        return Err("couldn't find the root folder in the MFT".into());
    }
    nodes[root].name = format!("{letter}:\\").into();
    nodes[root].parent = ROOT_RECORD;
    Ok(Tree::new(nodes, ROOT_RECORD, Method::Mft))
}

fn volume_data(file: &File) -> Result<Volume, String> {
    #[repr(C, align(8))]
    struct Buf([u8; 256]);
    let mut out = Buf([0; 256]);
    let mut returned = 0u32;
    let ok = unsafe {
        win::DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            out.0.as_mut_ptr() as _,
            out.0.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || returned < 96 {
        return Err("not an NTFS volume".into());
    }
    let b = &out.0;
    Ok(Volume {
        bytes_per_sector: le32(b, 40) as u64,
        bytes_per_cluster: le32(b, 44) as u64,
        record_size: le32(b, 48) as usize,
        mft_valid: le64(b, 56),
        mft_lcn: le64(b, 64),
    })
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> Result<(), String> {
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err("unexpected end of volume".into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) => return Err(format!("read failed ({e})")),
        }
    }
    Ok(())
}

/// Streams the MFT in large sequential reads, following its data runs, and hands
/// whole records to the parser.
fn read_mft(
    file: &File,
    runs: &[(i64, u64)],
    cluster: u64,
    total: u64,
    rs: usize,
    tx: mpsc::SyncSender<(usize, Vec<u8>)>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut staging: Vec<u8> = Vec::with_capacity(CHUNK * 2);
    let mut next_record = 0usize;
    let mut consumed = 0u64;
    for &(lcn, clusters) in runs {
        let mut remaining = (clusters * cluster).min(total - consumed);
        let mut offset = lcn.max(0) as u64 * cluster;
        while remaining > 0 {
            if cancel.load(Relaxed) {
                return Ok(());
            }
            let take = remaining.min(CHUNK as u64) as usize;
            let start = staging.len();
            staging.resize(start + take, 0);
            if lcn >= 0 {
                read_exact_at(file, &mut staging[start..], offset)?;
            }
            offset += take as u64;
            remaining -= take as u64;
            consumed += take as u64;
            if staging.len() >= CHUNK {
                let whole = staging.len() / rs * rs;
                let rest = staging[whole..].to_vec();
                staging.truncate(whole);
                let buf = std::mem::replace(&mut staging, Vec::with_capacity(CHUNK * 2));
                if tx.send((next_record, buf)).is_err() {
                    return Ok(());
                }
                next_record += whole / rs;
                staging.extend_from_slice(&rest);
            }
        }
        if consumed >= total {
            break;
        }
    }
    let whole = staging.len() / rs * rs;
    if whole > 0 {
        staging.truncate(whole);
        let _ = tx.send((next_record, staging));
    }
    Ok(())
}

/// Undoes NTFS "update sequence" protection: the last two bytes of every 512-byte
/// stride are stored in a fixup array and replaced with a check value on disk.
fn apply_fixup(rec: &mut [u8]) -> bool {
    let usa_off = le16(rec, 4) as usize;
    let usa_count = le16(rec, 6) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > rec.len() {
        return false;
    }
    let check = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let pos = i * 512 - 2;
        if pos + 2 > rec.len() {
            break;
        }
        if rec[pos..pos + 2] != check {
            return false;
        }
        rec[pos] = rec[usa_off + 2 * i];
        rec[pos + 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

struct Attrs<'a> {
    rec: &'a [u8],
    off: usize,
    end: usize,
}

impl<'a> Iterator for Attrs<'a> {
    type Item = (u32, &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.off + 16 > self.end {
            return None;
        }
        let kind = le32(self.rec, self.off);
        if kind == 0xFFFF_FFFF {
            return None;
        }
        let len = le32(self.rec, self.off + 4) as usize;
        if len < 16 || self.off + len > self.end {
            return None;
        }
        let a = &self.rec[self.off..self.off + len];
        self.off += len;
        Some((kind, a))
    }
}

fn attrs(rec: &[u8]) -> Attrs<'_> {
    let first = le16(rec, 0x14) as usize;
    let used = (le32(rec, 0x18) as usize).min(rec.len());
    Attrs { rec, off: first, end: used }
}

fn resident_value(a: &[u8]) -> Option<&[u8]> {
    if a.len() < 0x18 {
        return None;
    }
    let len = le32(a, 0x10) as usize;
    let off = le16(a, 0x14) as usize;
    a.get(off..off + len)
}

fn parse_record(rec: &mut [u8], node: &mut Node, base: &mut u32) {
    if rec.len() < 0x30 || &rec[0..4] != b"FILE" || !apply_fixup(rec) {
        return;
    }
    let rec = &*rec;
    let flags = le16(rec, 0x16);
    if flags & 1 == 0 {
        return; // record not in use
    }
    let base_ref = le64(rec, 0x20) & 0xFFFF_FFFF_FFFF;
    if base_ref != 0 {
        *base = base_ref as u32;
    }
    node.kind = if flags & 2 != 0 { DIR } else { FILE };

    for (kind, a) in attrs(rec) {
        let resident = a[8] == 0;
        let name_len = a[9];
        match kind {
            ATTR_STANDARD_INFORMATION if resident => {
                if let Some(v) = resident_value(a) {
                    if v.len() >= 0x10 {
                        node.mtime = le64(v, 0x08);
                    }
                }
            }
            ATTR_FILE_NAME if resident => {
                let Some(v) = resident_value(a) else { continue };
                if v.len() < 0x42 {
                    continue;
                }
                let n = v[0x40] as usize;
                // Namespaces: 0 POSIX, 1 Win32, 2 DOS 8.3, 3 Win32+DOS. Prefer the long name.
                let rank = match v[0x41] {
                    1 | 3 => 3,
                    0 => 2,
                    _ => 1,
                };
                if rank > node.rank && v.len() >= 0x42 + n * 2 {
                    let units = v[0x42..0x42 + n * 2]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]));
                    node.name = char::decode_utf16(units)
                        .map(|r| r.unwrap_or('\u{FFFD}'))
                        .collect::<String>()
                        .into_boxed_str();
                    node.parent = (le64(v, 0) & 0xFFFF_FFFF_FFFF) as u32;
                    node.rank = rank;
                }
            }
            ATTR_DATA if name_len == 0 => {
                if resident {
                    node.size += le32(a, 0x10) as u64;
                } else if a.len() >= 0x40 && le64(a, 0x10) == 0 {
                    // Sizes are only valid in the first extent (starting VCN 0).
                    let aflags = le16(a, 0x0C);
                    let compressed_or_sparse = aflags & 0x8001 != 0;
                    node.size += le64(a, 0x30);
                    node.alloc += if compressed_or_sparse && a.len() >= 0x48 {
                        le64(a, 0x40)
                    } else {
                        le64(a, 0x28)
                    };
                }
            }
            _ => {}
        }
    }
}

/// Large or heavily linked files spill attributes into extension records that point
/// back at a base record. Fold them into their base.
fn merge_extensions(nodes: &mut [Node], base: &[u32]) {
    for i in 0..nodes.len() {
        let b = base[i] as usize;
        if base[i] == u32::MAX || b == i || b >= nodes.len() {
            continue;
        }
        let ext = std::mem::take(&mut nodes[i]);
        let bn = &mut nodes[b];
        if bn.kind == NONE {
            continue;
        }
        bn.size += ext.size;
        bn.alloc += ext.alloc;
        if ext.rank > bn.rank {
            bn.name = ext.name;
            bn.parent = ext.parent;
            bn.rank = ext.rank;
        }
    }
    for nd in nodes.iter_mut() {
        if nd.kind != NONE && nd.rank == 0 {
            nd.kind = NONE;
        }
    }
}

/// Data runs of the MFT itself, from record 0. Returns None if the run list
/// continues in an extension record (an extremely fragmented MFT).
fn mft_runs(rec: &[u8], cluster: u64) -> Option<Vec<(i64, u64)>> {
    for (kind, a) in attrs(rec) {
        if kind != ATTR_DATA || a[9] != 0 || a[8] == 0 || a.len() < 0x40 {
            continue;
        }
        let start_vcn = le64(a, 0x10);
        let last_vcn = le64(a, 0x18);
        let allocated = le64(a, 0x28);
        if start_vcn != 0 || (last_vcn + 1) * cluster < allocated {
            return None;
        }
        return Some(parse_runs(a, le16(a, 0x20) as usize));
    }
    None
}

fn parse_runs(a: &[u8], mut p: usize) -> Vec<(i64, u64)> {
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    while p < a.len() {
        let header = a[p];
        if header == 0 {
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        p += 1;
        if len_size == 0 || len_size > 8 || off_size > 8 || p + len_size + off_size > a.len() {
            break;
        }
        let mut len = 0u64;
        for k in 0..len_size {
            len |= (a[p + k] as u64) << (8 * k);
        }
        p += len_size;
        if off_size == 0 {
            runs.push((-1, len)); // sparse
        } else {
            let mut delta = 0i64;
            for k in 0..off_size {
                delta |= (a[p + k] as i64) << (8 * k);
            }
            let shift = 64 - 8 * off_size as u32;
            delta = (delta << shift) >> shift; // sign-extend
            lcn += delta;
            runs.push((lcn, len));
        }
        p += off_size;
    }
    runs
}

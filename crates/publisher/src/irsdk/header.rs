// SPDX-License-Identifier: GPL-3.0-or-later

// These items are only called from the Windows platform block in mod.rs.
// On Linux (CI) they are compiled for the tests but not called from production
// paths, so suppress the dead_code lint on non-Windows targets.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// irsdk_header C struct layout (112 bytes at mmap offset 0).
///
/// All fields are little-endian i32. Layout from the iRacing SDK public header.
///
/// ```text
/// +0x00  ver               i32
/// +0x04  status            i32   bit 0 = connected
/// +0x08  tickRate          i32   always 60
/// +0x0C  sessionInfoUpdate i32
/// +0x10  sessionInfoLen    i32
/// +0x14  sessionInfoOffset i32
/// +0x18  numVars           i32
/// +0x1C  varHeaderOffset   i32
/// +0x20  numBuf            i32   always 4
/// +0x24  bufLen            i32   bytes per telemetry frame
/// +0x28  pad[2]            i32 × 2
/// +0x30  varBuf[4]         VarBuf × 4  (16 bytes each)
/// ```
pub struct IrsdkHeader {
    pub status: i32,
    pub session_info_update: i32,
    pub session_info_len: i32,
    pub session_info_offset: i32,
    pub num_vars: i32,
    pub var_header_offset: i32,
    pub num_buf: i32,
    pub buf_len: i32,
    pub var_bufs: [VarBuf; 4],
}

/// irsdk_varBuf (16 bytes).
///
/// ```text
/// +0x00  tickCount  i32  — pick the entry with the highest value (latest buffer)
/// +0x04  bufOffset  i32  — byte offset from start of mmap to this buffer
/// +0x08  pad[2]     i32 × 2
/// ```
#[derive(Clone, Copy, Default)]
pub struct VarBuf {
    pub tick_count: i32,
    pub buf_offset: i32,
}

/// irsdk_varHeader (144 bytes per variable).
///
/// ```text
/// +0x00  type    i32
/// +0x04  offset  i32   byte offset from start of the telemetry buffer row
/// +0x08  count   i32   1 for scalars, 64 for CarIdx arrays
/// +0x0C  countAsTime  i8
/// +0x0D  pad[3]  u8×3
/// +0x10  name    [u8;32]   null-terminated ASCII
/// +0x30  desc    [u8;64]   (not used by the engine)
/// +0x70  unit    [u8;32]   (not used by the engine)
/// ```
/// Total: 0x70 + 32 = 144 bytes.
#[derive(Debug, Clone)]
pub struct VarInfo {
    pub type_code: i32,
    pub offset: usize,
    pub count: usize,
}

/// Map from variable name → `VarInfo`.
pub type VarIndex = std::collections::HashMap<String, VarInfo>;

// ── Byte-offset constants ──────────────────────────────────────────────────

const HDR_STATUS: usize = 0x04;
const HDR_SESSION_INFO_UPDATE: usize = 0x0C;
const HDR_SESSION_INFO_LEN: usize = 0x10;
const HDR_SESSION_INFO_OFFSET: usize = 0x14;
const HDR_NUM_VARS: usize = 0x18;
const HDR_VAR_HEADER_OFFSET: usize = 0x1C;
const HDR_NUM_BUF: usize = 0x20;
const HDR_BUF_LEN: usize = 0x24;
const HDR_VAR_BUF_ARRAY: usize = 0x30;
const VAR_BUF_STRIDE: usize = 16;
const VAR_HDR_STRIDE: usize = 144;
const VAR_HDR_NAME_OFFSET: usize = 0x10;
const VAR_HDR_NAME_LEN: usize = 32;

// ── Read helpers (all infallible on a well-formed mmap) ───────────────────

fn read_i32_at(buf: &[u8], offset: usize) -> i32 {
    let bytes = buf[offset..offset + 4].try_into().expect("slice too short");
    i32::from_le_bytes(bytes)
}

// ── Public parsing functions ───────────────────────────────────────────────

/// Parse the irsdk_header from the raw mmap slice.
///
/// Returns `None` if the slice is shorter than the minimum header size.
pub fn parse_header(mmap: &[u8]) -> Option<IrsdkHeader> {
    if mmap.len() < 0x70 {
        return None;
    }

    let status = read_i32_at(mmap, HDR_STATUS);
    let session_info_update = read_i32_at(mmap, HDR_SESSION_INFO_UPDATE);
    let session_info_len = read_i32_at(mmap, HDR_SESSION_INFO_LEN);
    let session_info_offset = read_i32_at(mmap, HDR_SESSION_INFO_OFFSET);
    let num_vars = read_i32_at(mmap, HDR_NUM_VARS);
    let var_header_offset = read_i32_at(mmap, HDR_VAR_HEADER_OFFSET);
    let num_buf = read_i32_at(mmap, HDR_NUM_BUF);
    let buf_len = read_i32_at(mmap, HDR_BUF_LEN);

    let mut var_bufs = [VarBuf::default(); 4];
    for (i, var_buf) in var_bufs.iter_mut().enumerate() {
        let base = HDR_VAR_BUF_ARRAY + i * VAR_BUF_STRIDE;
        *var_buf = VarBuf {
            tick_count: read_i32_at(mmap, base),
            buf_offset: read_i32_at(mmap, base + 4),
        };
    }

    Some(IrsdkHeader {
        status,
        session_info_update,
        session_info_len,
        session_info_offset,
        num_vars,
        var_header_offset,
        num_buf,
        buf_len,
        var_bufs,
    })
}

/// Build a `VarIndex` from the variable-header array in the mmap.
///
/// Only variables whose names appear in `wanted` are added to the map.
/// Returns `None` if the mmap is too short to hold all variable headers.
pub fn build_var_index(mmap: &[u8], hdr: &IrsdkHeader, wanted: &[&str]) -> Option<VarIndex> {
    let base = hdr.var_header_offset as usize;
    let num_vars = hdr.num_vars as usize;
    let required = base + num_vars * VAR_HDR_STRIDE;

    if mmap.len() < required {
        return None;
    }

    let mut index = VarIndex::new();

    for i in 0..num_vars {
        let entry_base = base + i * VAR_HDR_STRIDE;

        // Read name (null-terminated ASCII)
        let name_bytes = &mmap
            [entry_base + VAR_HDR_NAME_OFFSET..entry_base + VAR_HDR_NAME_OFFSET + VAR_HDR_NAME_LEN];
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(VAR_HDR_NAME_LEN);
        let name = std::str::from_utf8(&name_bytes[..name_end]).ok()?;

        if !wanted.contains(&name) {
            continue;
        }

        let type_code = read_i32_at(mmap, entry_base);
        let offset = read_i32_at(mmap, entry_base + 4) as usize;
        let count = read_i32_at(mmap, entry_base + 8) as usize;

        index.insert(
            name.to_owned(),
            VarInfo {
                type_code,
                offset,
                count,
            },
        );
    }

    Some(index)
}

/// Returns `true` if the iRacing status field indicates a connected session.
pub fn is_connected(status: i32) -> bool {
    status & 0x01 != 0
}

/// Pick the varBuf with the highest `tick_count` (the most recently written buffer).
pub fn latest_buf(var_bufs: &[VarBuf; 4]) -> &VarBuf {
    var_bufs
        .iter()
        .max_by_key(|b| b.tick_count)
        .expect("varBuf array is always 4 elements")
}

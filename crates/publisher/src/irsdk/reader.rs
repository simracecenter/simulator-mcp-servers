// SPDX-License-Identifier: GPL-3.0-or-later

//! Typed read helpers and `build_frame()`.
//!
//! All reads take a `&[u8]` slice and a byte offset. The slice represents
//! one telemetry buffer frame (length = `IrsdkHeader::buf_len`).
//! No unsafe code here — the single unsafe block that creates the slice from
//! the raw mmap pointer lives in `mod.rs`.
//
// These items are only called from the Windows platform block in mod.rs.
// Suppress the dead_code lint on non-Windows targets.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use crate::telemetry_frame::TelemetryFrame as CoreFrame;

use super::header::VarIndex;

// ── irsdk type codes ──────────────────────────────────────────────────────

const IR_BOOL: i32 = 1;
const IR_INT: i32 = 2;
const IR_BITFIELD: i32 = 3;
const IR_FLOAT: i32 = 4;
const IR_DOUBLE: i32 = 5;

// ── Scalar read helpers ───────────────────────────────────────────────────

pub fn read_i32(buf: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(
        buf[offset..offset + 4]
            .try_into()
            .expect("buf too short for i32"),
    )
}

pub fn read_f32(buf: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(
        buf[offset..offset + 4]
            .try_into()
            .expect("buf too short for f32"),
    )
}

pub fn read_f64(buf: &[u8], offset: usize) -> f64 {
    f64::from_le_bytes(
        buf[offset..offset + 8]
            .try_into()
            .expect("buf too short for f64"),
    )
}

pub fn read_bool(buf: &[u8], offset: usize) -> bool {
    buf[offset] != 0
}

// ── Array read helpers ────────────────────────────────────────────────────

pub fn read_f32_array(buf: &[u8], offset: usize, count: usize) -> Vec<f32> {
    (0..count).map(|i| read_f32(buf, offset + i * 4)).collect()
}

pub fn read_i32_array(buf: &[u8], offset: usize, count: usize) -> Vec<i32> {
    (0..count).map(|i| read_i32(buf, offset + i * 4)).collect()
}

pub fn read_bool_array(buf: &[u8], offset: usize, count: usize) -> Vec<bool> {
    (0..count).map(|i| read_bool(buf, offset + i)).collect()
}

// ── Variable names we require ────────────────────────────────────────────

/// `SessionTimeRemain` value iRacing reports for a session with no time
/// limit (one week). Anything at or above it is treated as untimed.
pub const UNTIMED_SESSION_REMAIN_S: f64 = 604_800.0;
/// `SessionLapsRemainEx` sentinel for a session with no lap limit.
pub const UNLIMITED_SESSION_LAPS: i32 = 32_767;

/// All iRacing variable names the engine needs. Passed to `build_var_index`.
pub const REQUIRED_VARS: &[&str] = &[
    "SessionTime",
    "SessionFlags",
    "PlayerCarIdx",
    "Lap",
    "LapDistPct",
    "PlayerCarPosition",
    "OnPitRoad",
    "CarIdxLapDistPct",
    "CarIdxPosition",
    "CarIdxOnPitRoad",
    // Optional — absent in some session types; defaults to 0.0.
    "LapLastLapTime",
    // Optional — session metadata; default to 0 if absent.
    "SessionTick",
    "SessionState",
    "SessionNum",
    // Optional — session clock/lap budget; absent on some builds, `None` then.
    "SessionTimeRemain",
    "SessionLapsRemainEx",
    "PlayerCarMyIncidentCount",
    // Optional — absent in some sessions; defaults to empty vec.
    "CarIdxTrackSurface",
    // Optional — int array, same pattern as CarIdxPosition.
    "CarIdxLapCompleted",
];

// ── Frame builder ─────────────────────────────────────────────────────────

/// Build a `CoreFrame` from one telemetry buffer snapshot.
///
/// Returns `None` if any required variable is absent from `vars` or the
/// buffer is too short for a required offset.
pub fn build_frame(
    buf: &[u8],
    vars: &VarIndex,
    header_session_info_update: u32,
) -> Option<CoreFrame> {
    macro_rules! var {
        ($name:expr) => {
            vars.get($name)?
        };
    }

    let st_info = var!("SessionTime");
    let sf_info = var!("SessionFlags");
    let pci_info = var!("PlayerCarIdx");
    let lap_info = var!("Lap");
    let ldp_info = var!("LapDistPct");
    let pcp_info = var!("PlayerCarPosition");
    let opr_info = var!("OnPitRoad");
    let cidx_ldp = var!("CarIdxLapDistPct");
    let cidx_pos = var!("CarIdxPosition");
    let cidx_pit = var!("CarIdxOnPitRoad");

    // SessionTime is a double in the live API
    let session_time = if st_info.type_code == IR_DOUBLE {
        read_f64(buf, st_info.offset) as f32
    } else {
        read_f32(buf, st_info.offset)
    };

    let session_flags = if sf_info.type_code == IR_BITFIELD || sf_info.type_code == IR_INT {
        read_i32(buf, sf_info.offset) as u32
    } else {
        0
    };

    let player_car_idx = read_i32(buf, pci_info.offset) as u8;
    let lap = read_i32(buf, lap_info.offset) as u8;
    let lap_dist_pct = read_f32(buf, ldp_info.offset);
    let player_car_pos = read_i32(buf, pcp_info.offset) as u8;
    let on_pit_road = read_bool(buf, opr_info.offset);

    let count = cidx_ldp.count.min(64);

    let car_idx_lap_dist_pct = if cidx_ldp.type_code == IR_FLOAT {
        read_f32_array(buf, cidx_ldp.offset, count)
    } else {
        return None;
    };

    let car_idx_position = if cidx_pos.type_code == IR_INT {
        read_i32_array(buf, cidx_pos.offset, count)
            .into_iter()
            .map(|v| v as u8)
            .collect()
    } else {
        return None;
    };

    let car_idx_on_pit_road = if cidx_pit.type_code == IR_BOOL {
        read_bool_array(buf, cidx_pit.offset, count)
    } else {
        return None;
    };

    let lap_last_lap_time = vars
        .get("LapLastLapTime")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);

    // SessionInfoUpdate is sourced from the irsdk header counter.
    let session_info_update = header_session_info_update;

    let session_tick = vars
        .get("SessionTick")
        .map(|v| read_i32(buf, v.offset) as i64)
        .unwrap_or(0);

    let session_state = vars
        .get("SessionState")
        .map(|v| read_i32(buf, v.offset))
        .unwrap_or(0);

    let session_num = vars
        .get("SessionNum")
        .map(|v| read_i32(buf, v.offset))
        .unwrap_or(0);

    // iRacing reports one week (604800 s) when a session has no time limit
    // and a large sentinel (32767) when it has no lap limit.
    let session_time_remain = vars
        .get("SessionTimeRemain")
        .filter(|v| v.type_code == IR_DOUBLE)
        .map(|v| read_f64(buf, v.offset))
        .filter(|&s| (0.0..UNTIMED_SESSION_REMAIN_S).contains(&s));

    let session_laps_remain = vars
        .get("SessionLapsRemainEx")
        .filter(|v| v.type_code == IR_INT)
        .map(|v| read_i32(buf, v.offset))
        .filter(|&l| (0..UNLIMITED_SESSION_LAPS).contains(&l));

    let player_incident_count = vars
        .get("PlayerCarMyIncidentCount")
        .map(|v| read_i32(buf, v.offset))
        .unwrap_or(0);

    let car_idx_lap_completed = vars
        .get("CarIdxLapCompleted")
        .filter(|v| v.type_code == IR_INT)
        .map(|v| read_i32_array(buf, v.offset, v.count.min(64)))
        .unwrap_or_default();

    let car_idx_track_surface = vars
        .get("CarIdxTrackSurface")
        .filter(|v| v.type_code == IR_INT)
        .map(|v| read_i32_array(buf, v.offset, v.count.min(64)))
        .unwrap_or_default();

    let fuel_level = vars
        .get("FuelLevel")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);
    let throttle = vars
        .get("Throttle")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);
    let brake = vars
        .get("Brake")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);
    let speed = vars
        .get("Speed")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);
    let lf_temp_m = vars
        .get("LFtempM")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);
    let rf_temp_m = vars
        .get("RFtempM")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);
    let lr_temp_m = vars
        .get("LRtempM")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);
    let rr_temp_m = vars
        .get("RRtempM")
        .map(|v| read_f32(buf, v.offset))
        .unwrap_or(0.0);

    Some(CoreFrame {
        session_time,
        session_flags,
        player_car_idx,
        lap,
        lap_dist_pct,
        player_car_position: player_car_pos,
        on_pit_road,
        car_idx_lap_dist_pct,
        car_idx_position,
        car_idx_on_pit_road,
        car_idx_track_surface,
        lap_last_lap_time,
        session_info_update,
        session_tick,
        session_state,
        session_num,
        session_time_remain,
        session_laps_remain,
        player_incident_count,
        car_idx_lap_completed,
        lf_temp_m,
        rf_temp_m,
        lr_temp_m,
        rr_temp_m,
        fuel_level,
        throttle,
        brake,
        speed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irsdk::header::{VarIndex, VarInfo};

    /// Build a minimal telemetry buffer and matching VarIndex for testing.
    fn make_test_buf() -> (Vec<u8>, VarIndex) {
        // Lay out variables tightly:
        //   0x00  SessionTime  f64  8 bytes
        //   0x08  SessionFlags i32  4 bytes
        //   0x0C  PlayerCarIdx i32  4 bytes
        //   0x10  Lap          i32  4 bytes
        //   0x14  LapDistPct   f32  4 bytes
        //   0x18  PlayerCarPos i32  4 bytes
        //   0x1C  OnPitRoad    bool 1 byte
        //   0x1D  pad[3]
        //   0x20  CarIdxLapDistPct  f32×3   12 bytes
        //   0x2C  CarIdxPosition    i32×3   12 bytes
        //   0x38  CarIdxOnPitRoad   bool×3   3 bytes
        let mut buf = vec![0u8; 64];

        // SessionTime = 123.456 s
        buf[0x00..0x08].copy_from_slice(&123.456f64.to_le_bytes());
        // SessionFlags = 0x0100 (YELLOW_WAVE)
        buf[0x08..0x0C].copy_from_slice(&0x0100i32.to_le_bytes());
        // PlayerCarIdx = 5
        buf[0x0C..0x10].copy_from_slice(&5i32.to_le_bytes());
        // Lap = 3
        buf[0x10..0x14].copy_from_slice(&3i32.to_le_bytes());
        // LapDistPct = 0.75
        buf[0x14..0x18].copy_from_slice(&0.75f32.to_le_bytes());
        // PlayerCarPosition = 7
        buf[0x18..0x1C].copy_from_slice(&7i32.to_le_bytes());
        // OnPitRoad = false
        buf[0x1C] = 0;
        // CarIdxLapDistPct = [0.1, 0.5, 0.9]
        buf[0x20..0x24].copy_from_slice(&0.1f32.to_le_bytes());
        buf[0x24..0x28].copy_from_slice(&0.5f32.to_le_bytes());
        buf[0x28..0x2C].copy_from_slice(&0.9f32.to_le_bytes());
        // CarIdxPosition = [1, 2, 3]
        buf[0x2C..0x30].copy_from_slice(&1i32.to_le_bytes());
        buf[0x30..0x34].copy_from_slice(&2i32.to_le_bytes());
        buf[0x34..0x38].copy_from_slice(&3i32.to_le_bytes());
        // CarIdxOnPitRoad = [false, true, false]
        buf[0x38] = 0;
        buf[0x39] = 1;
        buf[0x3A] = 0;

        let mut index = VarIndex::new();
        index.insert(
            "SessionTime".into(),
            VarInfo {
                type_code: IR_DOUBLE,
                offset: 0x00,
                count: 1,
            },
        );
        index.insert(
            "SessionFlags".into(),
            VarInfo {
                type_code: IR_BITFIELD,
                offset: 0x08,
                count: 1,
            },
        );
        index.insert(
            "PlayerCarIdx".into(),
            VarInfo {
                type_code: IR_INT,
                offset: 0x0C,
                count: 1,
            },
        );
        index.insert(
            "Lap".into(),
            VarInfo {
                type_code: IR_INT,
                offset: 0x10,
                count: 1,
            },
        );
        index.insert(
            "LapDistPct".into(),
            VarInfo {
                type_code: IR_FLOAT,
                offset: 0x14,
                count: 1,
            },
        );
        index.insert(
            "PlayerCarPosition".into(),
            VarInfo {
                type_code: IR_INT,
                offset: 0x18,
                count: 1,
            },
        );
        index.insert(
            "OnPitRoad".into(),
            VarInfo {
                type_code: IR_BOOL,
                offset: 0x1C,
                count: 1,
            },
        );
        index.insert(
            "CarIdxLapDistPct".into(),
            VarInfo {
                type_code: IR_FLOAT,
                offset: 0x20,
                count: 3,
            },
        );
        index.insert(
            "CarIdxPosition".into(),
            VarInfo {
                type_code: IR_INT,
                offset: 0x2C,
                count: 3,
            },
        );
        index.insert(
            "CarIdxOnPitRoad".into(),
            VarInfo {
                type_code: IR_BOOL,
                offset: 0x38,
                count: 3,
            },
        );

        (buf, index)
    }

    #[test]
    fn build_frame_round_trips_all_fields() {
        let (buf, index) = make_test_buf();
        let frame = build_frame(&buf, &index, 42).expect("build_frame should succeed");

        assert!((frame.session_time - 123.456).abs() < 0.001, "session_time");
        assert_eq!(frame.session_flags, 0x0100, "session_flags");
        assert_eq!(frame.player_car_idx, 5, "player_car_idx");
        assert_eq!(frame.lap, 3, "lap");
        assert!((frame.lap_dist_pct - 0.75).abs() < 1e-5, "lap_dist_pct");
        assert_eq!(frame.player_car_position, 7, "player_car_position");
        assert!(!frame.on_pit_road, "on_pit_road");
        assert_eq!(frame.session_info_update, 42, "session_info_update");
        assert_eq!(frame.car_idx_lap_dist_pct.len(), 3);
        assert_eq!(frame.car_idx_position, vec![1, 2, 3]);
        assert_eq!(frame.car_idx_on_pit_road, vec![false, true, false]);
    }
}

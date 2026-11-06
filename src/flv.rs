//! Bounded FLV framing and a reconnect timeline, implemented from the file format.
//!
//! The relay forwards complete tags only. A truncated upstream tag is discarded
//! with that upstream connection rather than spliced into the next response.

use std::io::{self, ErrorKind, Read};
use std::time::{Duration, Instant};

pub const MAX_TAG_PAYLOAD: usize = 8 * 1024 * 1024;
pub const MAX_HEADER_BYTES: usize = 4096;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

/// Read the FLV header, optional header extension, and PreviousTagSize0.
pub fn read_header<R: Read>(input: &mut R) -> io::Result<Vec<u8>> {
    let mut basic = [0_u8; 9];
    input.read_exact(&mut basic)?;
    if &basic[..3] != b"FLV" || basic[3] != 1 || basic[4] & !5 != 0 {
        return Err(invalid("invalid FLV header"));
    }
    let offset = u32::from_be_bytes(basic[5..9].try_into().unwrap()) as usize;
    if !(9..=MAX_HEADER_BYTES).contains(&offset) {
        return Err(invalid("invalid FLV data offset"));
    }
    let mut header = vec![0; offset + 4];
    header[..9].copy_from_slice(&basic);
    input.read_exact(&mut header[9..])?;
    if header[offset..] != [0, 0, 0, 0] {
        return Err(invalid("invalid FLV initial previous-tag size"));
    }
    Ok(header)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlvTag {
    bytes: Vec<u8>,
}

impl FlvTag {
    /// Construct a standard tag, useful for adapters and synthetic fixtures.
    pub fn new(kind: u8, timestamp: u32, payload: &[u8]) -> io::Result<Self> {
        if !matches!(kind, 8 | 9 | 18) || payload.len() > MAX_TAG_PAYLOAD {
            return Err(invalid("invalid FLV tag kind or size"));
        }
        let length = payload.len();
        let mut bytes = vec![0; 11 + length + 4];
        bytes[0] = kind;
        bytes[1] = (length >> 16) as u8;
        bytes[2] = (length >> 8) as u8;
        bytes[3] = length as u8;
        bytes[11..11 + length].copy_from_slice(payload);
        bytes[11 + length..].copy_from_slice(&((11 + length) as u32).to_be_bytes());
        let mut tag = Self { bytes };
        tag.set_timestamp(timestamp);
        Ok(tag)
    }

    pub fn kind(&self) -> u8 {
        self.bytes[0]
    }
    pub fn timestamp(&self) -> u32 {
        (u32::from(self.bytes[7]) << 24)
            | (u32::from(self.bytes[4]) << 16)
            | (u32::from(self.bytes[5]) << 8)
            | u32::from(self.bytes[6])
    }
    pub fn set_timestamp(&mut self, timestamp: u32) {
        self.bytes[4] = (timestamp >> 16) as u8;
        self.bytes[5] = (timestamp >> 8) as u8;
        self.bytes[6] = timestamp as u8;
        self.bytes[7] = (timestamp >> 24) as u8;
    }
    pub fn payload(&self) -> &[u8] {
        &self.bytes[11..self.bytes.len() - 4]
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Hand the framed bytes back to the caller so a relay loop can move the
    /// allocation into its scratch buffer instead of freeing it per tag.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// AAC, AVC/HEVC, or enhanced-video sequence initialization packets.
    /// These frequently carry timestamp zero and must survive reconnects.
    pub fn is_codec_header(&self) -> bool {
        let p = self.payload();
        if p.len() < 2 {
            return false;
        }
        match self.kind() {
            8 => p[0] >> 4 == 10 && p[1] == 0,
            9 => {
                ((matches!(p[0] & 15, 7 | 12)) && p[1] == 0)
                    || (p[0] & 0x80 != 0 && p[0] & 15 == 0 && p.len() >= 5)
            }
            _ => false,
        }
    }
}

/// EOF is normal only at a tag boundary; partial headers/bodies are errors.
pub fn read_tag<R: Read>(input: &mut R) -> io::Result<Option<FlvTag>> {
    let mut header = [0_u8; 11];
    loop {
        match input.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    input.read_exact(&mut header[1..])?;
    if !matches!(header[0], 8 | 9 | 18) || header[8..11] != [0, 0, 0] {
        return Err(invalid("invalid FLV tag header"));
    }
    let length =
        (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
    if length > MAX_TAG_PAYLOAD {
        return Err(invalid("FLV tag exceeds size limit"));
    }
    let mut bytes = vec![0; 11 + length + 4];
    bytes[..11].copy_from_slice(&header);
    input.read_exact(&mut bytes[11..])?;
    let previous_size = u32::from_be_bytes(bytes[11 + length..].try_into().unwrap());
    if previous_size != (11 + length) as u32 {
        return Err(invalid("invalid FLV previous-tag size"));
    }
    Ok(Some(FlvTag { bytes }))
}

/// Same framing rules as read_tag, but reuses the caller's buffer so a relay
/// forwarding tens of thousands of tags per second does not allocate and free
/// one Vec per tag. The buffer is consumed by the returned tag; call
/// FlvTag::into_bytes after forwarding to hand the allocation back.
///
/// The buffer keeps whatever capacity the largest recycled tag needed. A relay
/// should drop it after an unusually large tag instead of pinning that memory.
pub fn read_tag_reusing<R: Read>(
    input: &mut R,
    scratch: &mut Vec<u8>,
) -> io::Result<Option<FlvTag>> {
    let mut header = [0_u8; 11];
    loop {
        match input.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    input.read_exact(&mut header[1..])?;
    if !matches!(header[0], 8 | 9 | 18) || header[8..11] != [0, 0, 0] {
        return Err(invalid("invalid FLV tag header"));
    }
    let length =
        (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
    if length > MAX_TAG_PAYLOAD {
        return Err(invalid("FLV tag exceeds size limit"));
    }
    let mut bytes = std::mem::take(scratch);
    bytes.resize(11 + length + 4, 0);
    bytes[..11].copy_from_slice(&header);
    input.read_exact(&mut bytes[11..])?;
    let previous_size = u32::from_be_bytes(bytes[11 + length..].try_into().unwrap());
    if previous_size != (11 + length) as u32 {
        return Err(invalid("invalid FLV previous-tag size"));
    }
    Ok(Some(FlvTag { bytes }))
}

/// Bounded onMetaData summary used by the diagnostic streaminfo command.
/// A stream without a leading script tag simply reports None.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_kbps: u32,
    pub codec: Option<CodecKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecKind {
    H264,
    Hevc,
    H263,
    Vp6,
}

impl CodecKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::H263 => "h263",
            Self::Vp6 => "vp6",
        }
    }

    fn from_id(id: u32) -> Option<Self> {
        match id {
            2 => Some(Self::H263),
            4 => Some(Self::Vp6),
            7 => Some(Self::H264),
            12 => Some(Self::Hevc),
            _ => None,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.contains("avc") || lower.contains("h264") {
            return Some(Self::H264);
        }
        if lower.contains("hev") || lower.contains("h265") {
            return Some(Self::Hevc);
        }
        None
    }
}

/// Upper bound on the onMetaData payload parsed by read_stream_info.
pub const MAX_STREAM_INFO_BYTES: usize = 64 * 1024;
const WINDOW_RECYCLE_LIMIT: usize = 1024 * 1024;

/// Read the FLV header and first tag; when that tag is an onMetaData script tag,
/// return its bounded summary. Consumes exactly the header plus that one tag, so
/// a following live measurement can continue at the next tag boundary.
pub fn read_stream_info<R: Read>(input: &mut R) -> io::Result<Option<StreamInfo>> {
    read_header(input)?;
    let mut scratch = Vec::new();
    let Some(tag) = read_tag_reusing(input, &mut scratch)? else {
        return Ok(None);
    };
    if tag.kind() != 18 || tag.payload().len() > MAX_STREAM_INFO_BYTES {
        return Ok(None);
    }
    Ok(parse_stream_info(tag.payload()))
}

/// Parse an onMetaData AMF0 script payload without allocating per property.
/// Unsupported AMF0 value kinds end the scan instead of guessing; numeric
/// fields already parsed stay usable, matching the reference diagnostics.
fn parse_stream_info(payload: &[u8]) -> Option<StreamInfo> {
    if payload.first() != Some(&0x02) {
        return None;
    }
    let mut p = 1usize;
    let name_len = usize::from(u16::from_be_bytes([*payload.get(p)?, *payload.get(p + 1)?]));
    p += 2 + name_len;
    let table = *payload.get(p)?;
    p += match table {
        0x08 => 5,
        0x03 => 1,
        _ => return None,
    };
    let mut info = StreamInfo::default();
    let mut found = false;
    while p + 3 <= payload.len() {
        if payload[p..p + 3] == [0, 0, 9] {
            break;
        }
        let key_len = usize::from(u16::from_be_bytes([payload[p], payload[p + 1]]));
        p += 2;
        if key_len == 0 || p + key_len >= payload.len() {
            break;
        }
        let key = String::from_utf8_lossy(&payload[p..p + key_len]).into_owned();
        p += key_len;
        let marker = payload[p];
        p += 1;
        match marker {
            0x00 => {
                if p + 8 > payload.len() {
                    break;
                }
                let value = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
                p += 8;
                match key.as_str() {
                    "width" => {
                        info.width = value.max(0.0).round() as u32;
                        found = true;
                    }
                    "height" => {
                        info.height = value.max(0.0).round() as u32;
                        found = true;
                    }
                    "framerate" => {
                        info.fps = value.max(0.0);
                        found = true;
                    }
                    "videodatarate" => {
                        info.video_kbps = value.max(0.0).round() as u32;
                        found = true;
                    }
                    "videocodecid" => {
                        info.codec = CodecKind::from_id(value.max(0.0).round() as u32);
                        found |= info.codec.is_some();
                    }
                    _ => {}
                }
            }
            0x01 => p += 1,
            0x02 => {
                if p + 2 > payload.len() {
                    break;
                }
                let len = usize::from(u16::from_be_bytes([payload[p], payload[p + 1]]));
                if p + 2 + len > payload.len() {
                    break;
                }
                if key.as_str() == "codecname" && info.codec.is_none() {
                    info.codec = CodecKind::from_name(&String::from_utf8_lossy(
                        &payload[p + 2..p + 2 + len],
                    ));
                }
                p += 2 + len;
            }
            0x0c => {
                if p + 4 > payload.len() {
                    break;
                }
                let len = u32::from_be_bytes(payload[p..p + 4].try_into().unwrap()) as usize;
                if p + 4 + len > payload.len() {
                    break;
                }
                p += 4 + len;
            }
            0x05 | 0x06 => {}
            0x0b => p += 10,
            _ => break,
        }
    }
    (found || info.codec.is_some()).then_some(info)
}

/// Largest per-tag timestamp step accepted as one continuous media interval.
/// Larger steps (ad splice, absolute-time base change, stream discontinuity)
/// are counted as anomalies instead of stretching the measured window.
pub const MAX_TIMELINE_JUMP_MS: i64 = 2000;

/// Video-track measurement over a bounded live window. The media span is the
/// sum of accepted consecutive timestamp steps, not the raw last-first span:
/// some CDNs splice absolute-time or disconnected segments into one response,
/// and the raw span then reports thousands of seconds for a 3 second window.
/// Raw span, wall clock and anomaly count are kept as diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VideoWindow {
    pub frames: u64,
    pub intervals: u64,
    pub video_bytes: u64,
    pub span_ms: u64,
    pub raw_span_ms: u64,
    pub wall_ms: u64,
    pub fps: f64,
    pub kbps: u32,
    pub median_delta_ms: f64,
    pub anomalies: u64,
    /// True when timestamps cannot be trusted as continuous milliseconds
    /// (media span far beyond the wall clock, an implausible interval rate, or
    /// a majority of discontinuities). Consumers must treat fps/kbps as
    /// unavailable in that case.
    pub timeline_suspect: bool,
}

/// Read complete tags until the byte budget or deadline, then derive the video
/// frame rate and bitrate. Returns None when the window had no usable span.
/// The caller must position the reader at a tag boundary: streaminfo calls this
/// after read_stream_info.
pub fn measure_video_window<R: Read>(
    input: &mut R,
    max_bytes: usize,
    deadline: std::time::Instant,
) -> io::Result<Option<VideoWindow>> {
    let started = std::time::Instant::now();
    let mut scratch = Vec::new();
    let mut frames = 0u64;
    let mut intervals = 0u64;
    let mut video_bytes = 0u64;
    let mut raw_first: Option<u32> = None;
    let mut raw_last = 0u32;
    let mut previous: Option<u32> = None;
    let mut span_ms = 0u64;
    let mut anomalies = 0u64;
    let mut deltas: Vec<u32> = Vec::new();
    let mut budget = max_bytes;
    while budget > 0 && std::time::Instant::now() < deadline {
        let Some(tag) = read_tag_reusing(input, &mut scratch)? else {
            break;
        };
        let framed = tag.as_bytes().len();
        budget = budget.saturating_sub(framed);
        if tag.kind() == 9 {
            let timestamp = tag.timestamp();
            raw_first.get_or_insert(timestamp);
            raw_last = raw_last.max(timestamp);
            if let Some(previous) = previous {
                let delta = i64::from(timestamp) - i64::from(previous);
                if delta > 0 && delta <= MAX_TIMELINE_JUMP_MS {
                    span_ms = span_ms.saturating_add(delta as u64);
                    intervals += 1;
                    if deltas.len() < 8192 {
                        deltas.push(delta as u32);
                    }
                } else {
                    // A duplicate, backward reset or multi-second jump starts a
                    // new continuous interval instead of stretching the window.
                    anomalies += 1;
                }
            }
            frames += 1;
            video_bytes += framed as u64;
            previous = Some(timestamp);
        }
        if framed <= WINDOW_RECYCLE_LIMIT {
            scratch = tag.into_bytes();
        }
    }
    let Some(first_timestamp) = raw_first else {
        return Ok(None);
    };
    let wall_ms = started.elapsed().as_millis() as u64;
    if frames < 2 || intervals == 0 || span_ms <= 200 {
        return Ok(None);
    }
    let raw_span_ms = u64::from(raw_last.saturating_sub(first_timestamp));
    deltas.sort_unstable();
    let median_delta_ms = f64::from(deltas[deltas.len() / 2]);
    let seconds = span_ms as f64 / 1000.0;
    let fps = intervals as f64 / seconds;
    let kbps = (video_bytes as f64 * 8.0 / 1000.0 / seconds).round() as u32;
    // A real bounded window cannot contain many times more media time than the
    // wall clock it was captured in (an initial GOP burst stays well under 4x),
    // and fewer than 1 or more than 240 intervals per second means the tags do
    // not carry plain millisecond timestamps.
    let total_steps = intervals + anomalies;
    let timeline_suspect = span_ms > wall_ms.saturating_mul(4).saturating_add(2000)
        || !(1.0..=240.0).contains(&fps)
        || (total_steps > 0 && anomalies.saturating_mul(4) > total_steps.saturating_mul(3));
    Ok(Some(VideoWindow {
        frames,
        intervals,
        video_bytes,
        span_ms,
        raw_span_ms,
        wall_ms,
        fps,
        kbps,
        median_delta_ms,
        anomalies,
        timeline_suspect,
    }))
}

/// Gap inserted after a mid-stream discontinuity so the forwarded timeline keeps
/// advancing even when the upstream clock jumps over dropped content.
const MEDIA_JUMP_GAP_MS: u64 = 20;

/// How long an epoch transition may stay unconfirmed before the guard is dropped.
///
/// A normal transition is confirmed by the peer track within a few interleaved
/// tags (tens of milliseconds). If the HTTP response ends first, or one track
/// stays absent, the next response may carry a completely different clock; a
/// permanent "unknown generation" guard would then discard every tag forever
/// while the relay kept reading (the player would starve without any error).
/// After this bound the pending guard is abandoned and the ordinary reconnect /
/// forward rebase logic takes over; a genuine old-track tail is still dropped by
/// the per-track backward watermark check.
const EPOCH_GUARD_TIMEOUT: Duration = Duration::from_secs(5);

/// Media tracks have separate watermarks: equal audio/video timestamps are valid.
/// Duplicate filtering is enabled only while catching up after a reconnect.
///
/// Besides reconnect overlap, the timeline also absorbs discontinuities that a
/// live CDN can inject inside one HTTP response: when the client falls behind,
/// huya drops its own queued data and resumes at the live edge, which shows up
/// as a multi-second (measured: +114 s) timestamp jump. Forwarding that jump
/// makes the player treat the stream as restarted and rebuild its demuxer
/// cache, which is a visible re-buffer. The timeline rebases such a jump onto
/// the already forwarded clock (the skipped content stays lost, the clock does
/// not break). The correction is shared by both tracks so A/V stay aligned.
#[derive(Debug, Clone)]
struct ClockEpoch {
    origin: u64,
    latest: u64,
    confirmed: [bool; 2],
}

#[derive(Debug)]
pub struct ReconnectTimeline {
    last: [Option<u64>; 2],
    last_raw: [Option<u64>; 2],
    catching_up: [bool; 2],
    shift: i64,
    epoch: Option<ClockEpoch>,
    epoch_started: Option<Instant>,
    jumps: u64,
    epochs: u64,
    last_jump_ms: i64,
    exhausted: bool,
    discarded_epoch_tail: u64,
    /// Test-injectable guard bound; production always uses EPOCH_GUARD_TIMEOUT.
    epoch_guard_timeout: Duration,
}

impl Default for ReconnectTimeline {
    fn default() -> Self {
        Self {
            last: [None; 2],
            last_raw: [None; 2],
            catching_up: [false; 2],
            shift: 0,
            epoch: None,
            epoch_started: None,
            jumps: 0,
            epochs: 0,
            last_jump_ms: 0,
            exhausted: false,
            discarded_epoch_tail: 0,
            epoch_guard_timeout: EPOCH_GUARD_TIMEOUT,
        }
    }
}

impl ReconnectTimeline {
    pub fn begin_reconnect(&mut self) {
        self.catching_up = [self.last[0].is_some(), self.last[1].is_some()];
        // Keep an unfinished epoch guard across HTTP boundaries: an old tail
        // must not acquire the new offset merely because transport reconnected.
    }
    pub fn jumps(&self) -> u64 { self.jumps }
    /// Clock generations (epoch transitions). Reconnect resets are generations
    /// too, so this can exceed the in-response jump counter.
    pub fn epochs(&self) -> u64 { self.epochs }
    pub fn last_jump_ms(&self) -> i64 { self.last_jump_ms }
    /// Current common clock correction: output timestamp minus input timestamp.
    pub fn shift_ms(&self) -> i64 { self.shift }
    pub fn exhausted(&self) -> bool { self.exhausted }
    pub fn discarded_epoch_tail(&self) -> u64 { self.discarded_epoch_tail }

    fn adjusted(&self, raw: u64) -> i128 {
        i128::from(raw) + i128::from(self.shift)
    }

    fn begin_epoch(&mut self, slot: usize, raw: u64, target: u64, jump: i128, transport_reset: bool) {
        // Compute from the original clock, never from a clamped intermediate.
        self.shift = (i128::from(target) - i128::from(raw)) as i64;
        let mut confirmed = [self.last[0].is_none(), self.last[1].is_none()];
        confirmed[slot] = true;
        self.epoch = Some(ClockEpoch {
            origin: raw, latest: raw, confirmed,
        });
        self.epoch_started = Some(Instant::now());
        self.catching_up[slot] = false;
        self.epochs = self.epochs.saturating_add(1);
        if !transport_reset { self.jumps = self.jumps.saturating_add(1); }
        self.last_jump_ms = jump.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    }

    /// Drop an epoch guard that stayed unconfirmed for too long. Kept separate
    /// from the accept path so the condition and its comment stay auditable.
    fn expire_stale_epoch(&mut self) {
        if self
            .epoch_started
            .is_some_and(|started| started.elapsed() >= self.epoch_guard_timeout)
        {
            self.epoch = None;
            self.epoch_started = None;
        }
    }

    /// Forward complete tags while preserving one common affine clock mapping.
    /// A discontinuity is an epoch transition, not permission to rebase on each
    /// interleaved tag. Until both tracks confirm the new epoch, old-track tails
    /// are discarded. Payload/CTS is never rewritten.
    pub fn accept(&mut self, tag: &mut FlvTag) -> bool {
        let boundary = self.last.iter().flatten().copied().max().unwrap_or(0);
        if tag.is_codec_header() {
            tag.set_timestamp(boundary.min(u64::from(u32::MAX)) as u32);
            return true;
        }
        let slot = match tag.kind() { 8 => 0, 9 => 1, _ => return true };
        let raw = u64::from(tag.timestamp());
        let limit = MAX_TIMELINE_JUMP_MS as u64;

        // A pending epoch is expected to resolve within a few interleaved tags.
        // Never let an unresolved guard survive indefinitely: after a reconnect
        // the next response can use a different clock, and a permanent unknown-
        // generation guard would discard every tag forever (silent starvation).
        self.expire_stale_epoch();
        if let Some(epoch) = self.epoch.as_mut() {
            // A track's first packet may be slightly earlier than its peer's
            // first DTS. Permit that normal interleave, but not the previous
            // generation many seconds away. A moving upper edge also permits
            // a temporarily absent track to join the established new clock.
            let lower = epoch.origin.saturating_sub(limit);
            let upper = epoch.latest.saturating_add(limit);
            if raw < lower || raw > upper {
                self.discarded_epoch_tail = self.discarded_epoch_tail.saturating_add(1);
                return false;
            }
            epoch.confirmed[slot] = true;
            epoch.latest = epoch.latest.max(raw);
        }

        let mut adjusted = self.adjusted(raw);
        let track_last = self.last[slot];
        let catching = self.catching_up[slot];
        if self.epoch.is_none() {
            if let Some(last) = track_last {
                let forward = adjusted - i128::from(boundary);
                let backward = i128::from(last) - adjusted;
                let clock_reset = catching && raw < 1000
                    && self.last_raw[slot].unwrap_or(0).saturating_sub(raw) > limit;
                if clock_reset || forward > i128::from(limit) || (!catching && backward > i128::from(limit)) {
                    let gap = if clock_reset { 1 } else { MEDIA_JUMP_GAP_MS };
                    let target = boundary.saturating_add(gap);
                    self.begin_epoch(slot, raw, target, if forward > i128::from(limit) { forward } else { -backward }, clock_reset);
                    adjusted = self.adjusted(raw);
                }
            }
        }
        if catching && self.epoch.is_none() {
            if track_last.is_some_and(|last| adjusted <= i128::from(last)) { return false; }
            self.catching_up[slot] = false;
        } else if self.epoch.is_some() {
            // Tail/duplicate data must not move an already forwarded track
            // backwards even if it lies within the epoch's interleave window.
            if track_last.is_some_and(|last| adjusted < i128::from(last)
                || (catching && adjusted == i128::from(last))) { return false; }
            self.catching_up[slot] = false;
        }
        if adjusted < 0 || adjusted > i128::from(u32::MAX) {
            self.exhausted = true;
            return false;
        }
        let adjusted = adjusted as u64;
        self.last[slot] = Some(self.last[slot].unwrap_or(0).max(adjusted));
        self.last_raw[slot] = Some(raw);
        tag.set_timestamp(adjusted as u32);
        let epoch_complete = self.epoch.as_ref().is_some_and(|epoch| {
            epoch.confirmed.iter().all(|ready| *ready)
        });
        if epoch_complete {
            self.epoch = None;
            self.epoch_started = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn media(kind: u8, timestamp: u32) -> FlvTag {
        FlvTag::new(
            kind,
            timestamp,
            if kind == 8 {
                &[0xaf, 1, 9]
            } else {
                &[0x27, 1, 9]
            },
        )
        .unwrap()
    }

    #[test]
    fn rebases_a_mid_stream_forward_discontinuity_without_a_reconnect() {
        let mut timeline = ReconnectTimeline::default();
        let mut video = media(9, 1000);
        assert!(timeline.accept(&mut video));
        assert_eq!(video.timestamp(), 1000);
        let mut audio = media(8, 1000);
        assert!(timeline.accept(&mut audio));
        assert_eq!(audio.timestamp(), 1000);
        let mut video = media(9, 1016);
        assert!(timeline.accept(&mut video));
        assert_eq!(video.timestamp(), 1016);
        assert_eq!(timeline.jumps(), 0);

        // A live CDN skipped 113 s of queued media inside the same response.
        let mut jumped = media(9, 116_016);
        assert!(timeline.accept(&mut jumped));
        assert_eq!(timeline.jumps(), 1);
        assert_eq!(timeline.last_jump_ms(), 116_016 - 1016);
        assert_eq!(jumped.timestamp(), 1016 + 20);
        assert_eq!(timeline.jumps(), 1);

        // Following tags keep the corrected clock; no further rebase is needed.
        let mut following_video = media(9, 116_032);
        assert!(timeline.accept(&mut following_video));
        assert_eq!(following_video.timestamp(), 1052);
        let mut following_audio = media(8, 116_040);
        assert!(timeline.accept(&mut following_audio));
        assert_eq!(following_audio.timestamp(), 1060);
        assert_eq!(timeline.jumps(), 1);
    }

    #[test]
    fn rebases_a_mid_stream_backward_discontinuity() {
        let mut timeline = ReconnectTimeline::default();
        let mut first = media(9, 5000);
        assert!(timeline.accept(&mut first));
        let mut next = media(9, 5016);
        assert!(timeline.accept(&mut next));

        // The upstream clock restarted far behind the forwarded timeline.
        let mut restarted = media(9, 0);
        assert!(timeline.accept(&mut restarted));
        assert_eq!(timeline.jumps(), 1);
        assert_eq!(timeline.last_jump_ms(), -5016);
        assert_eq!(restarted.timestamp(), 5016 + 20);

        let mut following_audio = media(8, 24);
        assert!(timeline.accept(&mut following_audio));
        assert_eq!(following_audio.timestamp(), 5060);
        assert_eq!(timeline.jumps(), 1);
    }

    #[test]
    fn validates_header_and_preserves_extensions() {
        let header = b"FLV\x01\x05\x00\x00\x00\x0bXY\0\0\0\0";
        assert_eq!(read_header(&mut Cursor::new(header)).unwrap(), header);
        assert!(read_header(&mut Cursor::new(b"FLV\x01\x05\xff\xff\xff\xff")).is_err());
        assert!(read_header(&mut Cursor::new(b"FLV\x01\x05\0\0\0\x09\0\0\0\x01")).is_err());
    }

    #[test]
    fn tag_roundtrip_and_truncation() {
        let tag = media(9, 0xfedcba98);
        let bytes = tag.as_bytes();
        assert_eq!(
            read_tag(&mut Cursor::new(bytes)).unwrap(),
            Some(tag.clone())
        );
        for len in 1..bytes.len() {
            assert!(read_tag(&mut Cursor::new(&bytes[..len])).is_err());
        }
        assert!(read_tag(&mut Cursor::new([])).unwrap().is_none());
    }

    #[test]
    fn reusing_reader_matches_read_tag_and_recycles_the_allocation() {
        let mut framed = Vec::new();
        for (kind, timestamp) in [(9_u8, 1000_u32), (8, 1000), (9, 1010), (18, 0)] {
            framed.extend_from_slice(&media(kind, timestamp).as_bytes());
        }
        let mut expected = Cursor::new(framed.clone());
        let mut actual = Cursor::new(framed.clone());
        let mut scratch = Vec::new();
        loop {
            let expected_tag = read_tag(&mut expected).unwrap();
            let Some(tag) = read_tag_reusing(&mut actual, &mut scratch).unwrap() else {
                assert!(expected_tag.is_none());
                break;
            };
            assert_eq!(Some(tag.clone()), expected_tag);
            let framed_len = tag.as_bytes().len();
            scratch = tag.into_bytes();
            assert_eq!(scratch.len(), framed_len);
        }
        // Truncation and bad framing behave exactly like read_tag.
        let mut tag = media(8, 10).as_bytes().to_vec();
        *tag.last_mut().unwrap() ^= 1;
        assert!(read_tag_reusing(&mut Cursor::new(tag), &mut scratch).is_err());
    }

    #[test]
    fn reusing_reader_keeps_capacity_for_later_smaller_tags() {
        let large = FlvTag::new(9, 10, &vec![0x27_u8; 64 * 1024]).unwrap();
        let small = media(8, 20);
        let mut scratch = Vec::new();
        let mut cursor = Cursor::new(large.as_bytes());
        let tag = read_tag_reusing(&mut cursor, &mut scratch)
            .unwrap()
            .unwrap();
        scratch = tag.into_bytes();
        let capacity = scratch.capacity();
        assert!(capacity >= 64 * 1024);
        let mut cursor = Cursor::new(small.as_bytes());
        let tag = read_tag_reusing(&mut cursor, &mut scratch)
            .unwrap()
            .unwrap();
        assert_eq!(tag, small);
        scratch = tag.into_bytes();
        assert_eq!(scratch.capacity(), capacity, "capacity must be reused");
    }

    fn amf0_key(key: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.extend_from_slice(key.as_bytes());
        out
    }

    fn amf0_number(key: &str, value: f64) -> Vec<u8> {
        let mut out = amf0_key(key);
        out.push(0x00);
        out.extend_from_slice(&value.to_be_bytes());
        out
    }

    fn metadata_payload() -> Vec<u8> {
        let mut out = vec![0x02];
        out.extend_from_slice(&(b"onMetaData".len() as u16).to_be_bytes());
        out.extend_from_slice(b"onMetaData");
        out.push(0x08);
        out.extend_from_slice(&5_u32.to_be_bytes());
        out.extend_from_slice(&amf0_number("width", 1280.0));
        out.extend_from_slice(&amf0_number("height", 720.0));
        out.extend_from_slice(&amf0_number("framerate", 30.0));
        out.extend_from_slice(&amf0_number("videodatarate", 2500.0));
        out.extend_from_slice(&amf0_number("videocodecid", 7.0));
        out.extend_from_slice(&[0x00, 0x00, 0x09]);
        out
    }

    #[test]
    fn stream_info_parses_on_metadata_and_rejects_missing_script_tag() {
        let mut stream = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        stream.extend_from_slice(FlvTag::new(18, 0, &metadata_payload()).unwrap().as_bytes());
        let mut cursor = Cursor::new(&stream);
        let info = read_stream_info(&mut cursor).unwrap().unwrap();
        assert_eq!((info.width, info.height), (1280, 720));
        assert!((info.fps - 30.0).abs() < f64::EPSILON);
        assert_eq!(info.video_kbps, 2500);
        assert_eq!(info.codec, Some(CodecKind::H264));
        // Consumes the header plus exactly one script tag.
        assert_eq!(cursor.position() as usize, stream.len());

        let mut media_only = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        media_only.extend_from_slice(media(9, 0).as_bytes());
        assert!(
            read_stream_info(&mut Cursor::new(media_only))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn video_window_measures_fps_and_bitrate_from_video_tags_only() {
        let mut stream = Vec::new();
        for index in 0..11_u32 {
            stream.extend_from_slice(media(8, index * 100).as_bytes());
            let video = FlvTag::new(9, index * 100, &[0x27, 1, 7, 2, 3]).unwrap();
            stream.extend_from_slice(video.as_bytes());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let window = measure_video_window(&mut Cursor::new(&stream), 1 << 20, deadline)
            .unwrap()
            .unwrap();
        assert_eq!(window.frames, 11);
        assert_eq!(window.span_ms, 1000);
        assert!((window.fps - 10.0).abs() < 1e-9);
        let expected_kbps = (window.video_bytes as f64 * 8.0 / 1000.0).round() as u32;
        assert_eq!(window.kbps, expected_kbps);

        let mut audio_only = Vec::new();
        for index in 0..11_u32 {
            audio_only.extend_from_slice(media(8, index * 100).as_bytes());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        assert!(
            measure_video_window(&mut Cursor::new(audio_only), 1 << 20, deadline)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn video_window_ignores_timestamp_splices_and_absolute_time_bases() {
        // Three 30 fps frames, then a CDN absolute-time base change, then the
        // same cadence continues. The splice must not stretch the window.
        let mut stream = Vec::new();
        for timestamp in [0_u32, 100, 200] {
            stream.extend_from_slice(FlvTag::new(9, timestamp, &[0x27, 1, 7]).unwrap().as_bytes());
        }
        for timestamp in [1_700_000_000_u32, 1_700_000_100, 1_700_000_200] {
            stream.extend_from_slice(FlvTag::new(9, timestamp, &[0x27, 1, 7]).unwrap().as_bytes());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let window = measure_video_window(&mut Cursor::new(&stream), 1 << 20, deadline)
            .unwrap()
            .unwrap();
        assert_eq!(window.frames, 6);
        assert_eq!(window.intervals, 4);
        assert_eq!(window.anomalies, 1);
        assert_eq!(window.span_ms, 400, "only the accepted 100 ms steps count");
        assert_eq!(window.raw_span_ms, 1_700_000_200);
        assert!((window.fps - 10.0).abs() < 0.01);
        assert!(!window.timeline_suspect);
    }

    #[test]
    fn video_window_counts_duplicate_and_backward_timestamps_as_anomalies() {
        let mut stream = Vec::new();
        for timestamp in [0_u32, 100, 200, 200, 300, 250] {
            stream.extend_from_slice(FlvTag::new(9, timestamp, &[0x27, 1, 7]).unwrap().as_bytes());
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let window = measure_video_window(&mut Cursor::new(&stream), 1 << 20, deadline)
            .unwrap()
            .unwrap();
        assert_eq!(window.frames, 6);
        assert_eq!(window.intervals, 3);
        assert_eq!(window.anomalies, 2);
        assert_eq!(window.span_ms, 300);
        assert!((window.fps - 10.0).abs() < 0.01);
    }

    #[test]
    fn video_window_marks_implausible_timeline_unreliable() {
        // 5 accepted intervals of 1.9 s can never fit a real bounded window:
        // this stands in for a non-millisecond timestamp unit and must be
        // reported as unreliable rather than as a tiny fps.
        let mut stream = Vec::new();
        let mut timestamp = 0_u32;
        for _ in 0..6 {
            stream.extend_from_slice(FlvTag::new(9, timestamp, &[0x27, 1, 7]).unwrap().as_bytes());
            timestamp = timestamp.wrapping_add(1900);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let window = measure_video_window(&mut Cursor::new(&stream), 1 << 20, deadline)
            .unwrap()
            .unwrap();
        assert_eq!(window.intervals, 5);
        assert_eq!(window.span_ms, 9500);
        assert!(window.timeline_suspect, "span far outside the wall window");
        assert!(window.fps < 1.0);
    }

    #[test]
    fn rejects_bad_previous_size_and_oversize_before_allocating() {
        let mut tag = media(8, 10).as_bytes().to_vec();
        *tag.last_mut().unwrap() ^= 1;
        assert!(read_tag(&mut Cursor::new(tag)).is_err());
        let mut oversized = [0_u8; 11];
        oversized[0] = 9;
        oversized[1] = 0x80;
        oversized[3] = 1;
        assert_eq!(
            read_tag(&mut Cursor::new(oversized)).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn only_reconnect_overlap_is_dropped_and_tracks_are_independent() {
        let mut timeline = ReconnectTimeline::default();
        for kind in [8, 9] {
            assert!(timeline.accept(&mut media(kind, 2000)));
            assert!(timeline.accept(&mut media(kind, 2000)));
        }
        timeline.begin_reconnect();
        for kind in [8, 9] {
            assert!(!timeline.accept(&mut media(kind, 1999)));
            assert!(!timeline.accept(&mut media(kind, 2000)));
            assert!(timeline.accept(&mut media(kind, 2001)));
            assert!(timeline.accept(&mut media(kind, 2001)));
        }
    }

    #[test]
    fn repeated_clock_resets_rebase_both_tracks_and_preserve_codec_headers() {
        let mut timeline = ReconnectTimeline::default();
        assert!(timeline.accept(&mut media(8, 9000)));
        assert!(timeline.accept(&mut media(9, 9000)));
        timeline.begin_reconnect();
        let mut codec = FlvTag::new(9, 0, &[0x17, 0, 1]).unwrap();
        assert!(timeline.accept(&mut codec));
        assert_eq!(codec.timestamp(), 9000);
        let mut audio = media(8, 0);
        assert!(timeline.accept(&mut audio));
        assert_eq!(audio.timestamp(), 9001);
        let mut video = media(9, 0);
        assert!(timeline.accept(&mut video));
        assert_eq!(video.timestamp(), 9001);
        // A +7 s jump inside one response is compressed onto the forwarded clock
        // instead of being forwarded as a discontinuity (the player stays synced).
        let mut forward = media(8, 7000);
        assert!(timeline.accept(&mut forward));
        assert_eq!(forward.timestamp(), 9021);
        let mut forward_video = media(9, 7000);
        assert!(timeline.accept(&mut forward_video));
        assert_eq!(forward_video.timestamp(), 9021);
        assert_eq!(timeline.jumps(), 1);
        // A later upstream clock restart still rebases after the forwarded clock.
        timeline.begin_reconnect();
        let mut reset = media(9, 0);
        assert!(timeline.accept(&mut reset));
        assert_eq!(reset.timestamp(), 9022);
    }

    #[test]
    fn epoch_transition_discards_old_audio_tail_without_changing_pair_delay() {
        let mut timeline = ReconnectTimeline::default();
        for kind in [8, 9] { assert!(timeline.accept(&mut media(kind, 100_000))); }
        let mut video = media(9, 110_000);
        assert!(timeline.accept(&mut video));
        for raw in (100_020..=101_500).step_by(20) {
            assert!(!timeline.accept(&mut media(8, raw)));
        }
        let mut audio = media(8, 110_000);
        assert!(timeline.accept(&mut audio));
        assert_eq!(audio.timestamp(), video.timestamp());
        assert_eq!(timeline.jumps(), 1);
        assert_eq!(timeline.discarded_epoch_tail(), 75);
        for raw in [110_040, 110_080] {
            let mut a = media(8, raw); let mut v = media(9, raw);
            assert!(timeline.accept(&mut a) && timeline.accept(&mut v));
            assert_eq!(a.timestamp(), v.timestamp());
        }
    }

    #[test]
    fn reconnect_reset_never_applies_new_shift_to_old_other_track_tail() {
        let mut timeline = ReconnectTimeline::default();
        for kind in [8, 9] { assert!(timeline.accept(&mut media(kind, 9000))); }
        timeline.begin_reconnect();
        let mut v = media(9, 0); assert!(timeline.accept(&mut v));
        assert!(!timeline.accept(&mut media(8, 8900)));
        let mut a = media(8, 0); assert!(timeline.accept(&mut a));
        assert_eq!(a.timestamp(), v.timestamp());
        assert_eq!(a.timestamp(), 9001);
        timeline.begin_reconnect();
        assert!(!timeline.accept(&mut media(8, 0)), "same-epoch duplicate on a new connection is dropped");
    }

    #[test]
    fn normal_two_hour_span_and_signed_cts_do_not_accumulate_drift() {
        let mut timeline = ReconnectTimeline::default();
        for raw in (100_000..=7_300_000).step_by(40) {
            let mut audio = media(8, raw); let mut video = media(9, raw);
            assert!(timeline.accept(&mut audio) && timeline.accept(&mut video));
            assert_eq!(audio.timestamp(), raw); assert_eq!(video.timestamp(), raw);
        }
        assert_eq!(timeline.jumps(), 0);
        for cts in [-80_i32, 0, 120] {
            let bytes = cts.to_be_bytes();
            let mut tag = FlvTag::new(9, 7_300_040, &[0x27, 1, bytes[1], bytes[2], bytes[3], 9]).unwrap();
            let payload = tag.payload().to_vec(); assert!(timeline.accept(&mut tag));
            assert_eq!(tag.payload(), payload); assert_eq!(tag.timestamp(), 7_300_040);
        }
    }

    #[test]
    fn stale_unconfirmed_epoch_is_abandoned_instead_of_starving_the_stream() {
        let mut timeline = ReconnectTimeline {
            epoch_guard_timeout: Duration::from_millis(1),
            ..ReconnectTimeline::default()
        };
        assert!(timeline.accept(&mut media(8, 100_000)));
        assert!(timeline.accept(&mut media(9, 100_000)));
        // A mid-response jump starts an epoch; the audio track cannot confirm it
        // before the HTTP response ends.
        let mut jumped = media(9, 200_000);
        assert!(timeline.accept(&mut jumped));
        assert_eq!(timeline.jumps(), 1);
        assert_eq!(timeline.epochs(), 1);
        std::thread::sleep(Duration::from_millis(3));
        timeline.begin_reconnect();
        // The next CDN response uses a completely different clock. A permanent
        // unknown-generation guard would discard every tag forever (the relay
        // keeps reading, the player starves, no reconnect is triggered).
        let mut audio = media(8, 500);
        assert!(
            timeline.accept(&mut audio),
            "a stale epoch guard must not starve the forwarded stream"
        );
        let mut video = media(9, 500);
        assert!(timeline.accept(&mut video));
        // The abandoned guard must not rewind the forwarded clock.
        assert!(audio.timestamp() >= 100_020, "audio out={}", audio.timestamp());
        assert!(video.timestamp() >= 100_020, "video out={}", video.timestamp());
        assert!(timeline.epochs() >= 2, "generation count must expose the rebase");
        // The common affine correction stays positive and keeps both tracks on
        // the already forwarded clock (an exact value depends on which tag
        // triggers the rebase, so only its sign/size bound is asserted here).
        assert!(timeline.shift_ms() > 0);
        assert!((audio.timestamp() as i64 - video.timestamp() as i64).abs() <= 20);
    }

    #[test]
    fn output_clock_overflow_is_rejected_instead_of_saturated() {
        let mut timeline = ReconnectTimeline::default();
        for kind in [8, 9] { assert!(timeline.accept(&mut media(kind, u32::MAX - 10))); }
        timeline.begin_reconnect();
        for kind in [8, 9] { assert!(timeline.accept(&mut media(kind, 0))); }
        assert!(!timeline.accept(&mut media(8, 100)));
        assert!(timeline.exhausted());
    }
}

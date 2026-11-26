//! Frame rate and process statistics in the top left corner of the picture.
//!
//! The window's caption has the frame rate, but a capture of the window's
//! picture (a stream) does not carry the caption. Here the number is part
//! of the picture: a small box of text copied onto the swap chain's back
//! buffer right after the title's frame, before it is presented. The
//! title's own frame is not touched, so saved frames and everything read
//! back from it are as before.
//!
//! The text is drawn on the CPU with a font of its own (5 by 7 dots a
//! glyph) into an immutable upload buffer. Each command list keeps the
//! buffer alive until the list is reset.

use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};

use crate::host::{Host, HostList};

/// Resource values at display precision (tenths of a percent or GB).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stats {
    cpu: Option<u32>,
    gpu: Option<u32>,
    ram: Option<[u64; 2]>,
    vram: Option<[u64; 2]>,
}

fn tenths(value: f64) -> u32 {
    (value * 10.0).round() as u32
}

fn cpu_percent(cpu_us: u64, wall: Duration, logical_cpus: u32) -> Option<f64> {
    let capacity = wall.as_secs_f64() * 1e6 * f64::from(logical_cpus);
    (capacity > 0.0).then(|| cpu_us as f64 / capacity * 100.0)
}

fn gb_tenths(bytes: u64) -> u64 {
    const TENTH_GB: u64 = 100_000_000;
    bytes / TENTH_GB + u64::from(bytes % TENTH_GB >= TENTH_GB / 2)
}

/// Our queue's busy share of the interval: `idle_ms` is how long the card
/// had nothing of ours queued (the host's card clock), `wall_ms` the
/// interval. Work of other processes on the card is not counted.
fn busy_percent(idle_ms: f64, wall_ms: f64) -> Option<f64> {
    (wall_ms > 0.0).then(|| (100.0 * (1.0 - idle_ms / wall_ms)).clamp(0.0, 100.0))
}

/// A glyph's seven rows, five dots each (bit 4 is the left one).
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'a' => [0, 0, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111],
        'm' => [0, 0, 0b11010, 0b10101, 0b10101, 0b10101, 0b10101],
        'n' => [0, 0, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001],
        's' => [0, 0, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
        '/' => [0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000],
        '%' => [0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011],
        _ => [0; 7],
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Display {
    fps: u32,
    ms: u32,
    stats: Option<Stats>,
}

impl Display {
    fn text(self) -> String {
        use std::fmt::Write;

        fn decimal(tenths: u64) -> String {
            format!("{}.{}", tenths / 10, tenths % 10)
        }

        let Some(stats) = self.stats else {
            return format!("{} FPS", decimal(self.fps.into()));
        };
        let mut text = format!("FPS {} / {} ms", decimal(self.fps.into()), decimal(self.ms.into()));
        for (label, value) in [("CPU %", stats.cpu), ("GPU %", stats.gpu)] {
            let value = value.map_or_else(|| "n/a".into(), |v| decimal(v.into()));
            let _ = write!(text, "\n{label} {value}");
        }
        for (label, value) in [("RAM", stats.ram), ("VRAM", stats.vram)] {
            let value = value.map_or_else(
                || "n/a".into(),
                |[used, limit]| format!("{} / {} GB", decimal(used), decimal(limit)),
            );
            let _ = write!(text, "\n{label} {value}");
        }
        text
    }
}

/// Bytes a pixel of a swap chain format this layer presents in, or none.
fn pixel_bytes(format: DXGI_FORMAT) -> Option<usize> {
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_R10G10B10A2_UNORM => Some(4),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(8),
        _ => None,
    }
}

/// One pixel of `format`: of the text (`lit`) or of the box behind it.
fn pixel(format: DXGI_FORMAT, lit: bool, out: &mut [u8]) {
    // The text: a bright yellow. The box: nearly black.
    let (r, g, b): (f32, f32, f32) = if lit { (1.0, 0.92, 0.16) } else { (0.03, 0.03, 0.03) };
    let byte = |v: f32| (v * 255.0).round() as u8;
    match format {
        DXGI_FORMAT_R8G8B8A8_UNORM => out.copy_from_slice(&[byte(r), byte(g), byte(b), 255]),
        DXGI_FORMAT_B8G8R8A8_UNORM => out.copy_from_slice(&[byte(b), byte(g), byte(r), 255]),
        DXGI_FORMAT_R10G10B10A2_UNORM => {
            let ten = |v: f32| (v * 1023.0).round() as u32;
            out.copy_from_slice(&(ten(r) | ten(g) << 10 | ten(b) << 20 | 3 << 30).to_le_bytes());
        }
        _ => {
            for (i, v) in [r, g, b, 1.0].into_iter().enumerate() {
                out[i * 2..i * 2 + 2].copy_from_slice(&half(v).to_le_bytes());
            }
        }
    }
}

/// A value from 0 to 1 as a 16-bit float.
fn half(v: f32) -> u16 {
    let bits = v.clamp(0.0, 1.0).to_bits();
    if v <= 0.0 {
        return 0;
    }
    // Sign 0; the exponent moves from a bias of 127 to one of 15; the top
    // ten bits of the fraction stay. Values under 2^-14 (none of ours) are 0.
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    if exponent <= 0 {
        return 0;
    }
    ((exponent as u32) << 10 | (bits >> 13) & 0x3FF) as u16
}

/// The box for `text` at `scale` dots a pixel: its width and height in
/// pixels and, row by row from the top, whether each pixel is text.
fn bitmap(text: &str, scale: u32) -> (u32, u32, Vec<bool>) {
    let pad = 2 * scale;
    let columns = text.lines().map(|line| line.chars().count()).max().unwrap_or(0).max(1) as u32;
    let lines = text.lines().count().max(1) as u32;
    let width = 2 * pad + (columns * 6 - 1) * scale;
    let height = 2 * pad + (lines * 9 - 2) * scale;
    let mut lit = vec![false; (width * height) as usize];
    for (line, text) in text.lines().enumerate() {
        for (i, c) in text.chars().enumerate() {
            for (row, bits) in glyph(c).into_iter().enumerate() {
                for column in 0..5u32 {
                    if bits >> (4 - column) & 1 == 0 {
                        continue;
                    }
                    let x0 = pad + (i as u32 * 6 + column) * scale;
                    let y0 = pad + (line as u32 * 9 + row as u32) * scale;
                    for y in y0..y0 + scale {
                        for x in x0..x0 + scale {
                            lit[(y * width + x) as usize] = true;
                        }
                    }
                }
            }
        }
    }
    (width, height, lit)
}

/// A buffer the card copies a text's box from, with its footprint.
struct Made {
    buffer: ID3D12Resource,
    width: u32,
    height: u32,
    pitch: u32,
}

fn retry_upload<T>(retry_at: &mut Option<Instant>, now: impl Fn() -> Instant, create: impl FnOnce() -> Option<T>) -> Option<T> {
    if retry_at.is_some_and(|at| now() < at) {
        return None;
    }
    let made = create();
    *retry_at = made.is_none().then(|| now() + Duration::from_secs(1));
    made
}

pub(crate) struct Overlay {
    on: bool,
    stats: bool,
    rate: Option<(u32, u32)>,
    /// Resources as of the last rate, sampled with it; none without stats.
    sampled: Option<Stats>,
    /// The process's CPU time at the last sample, and when.
    last_cpu: Option<(Instant, u64)>,
    /// Only the current text, format and scale are cached.
    made: Option<((Display, DXGI_FORMAT, u32), Made)>,
    retry_at: Option<Instant>,
}

impl Overlay {
    pub(crate) fn new() -> Self {
        // VICTORY_FPS_COUNTER=0 hides the overlay. VICTORY_STATS=0 shows
        // the FPS line alone and skips resource sampling.
        let on = std::env::var_os("VICTORY_FPS_COUNTER").is_none_or(|v| v != "0");
        let stats = on && std::env::var_os("VICTORY_STATS").is_none_or(|v| v != "0");
        Self {
            on,
            stats,
            rate: None,
            sampled: None,
            last_cpu: None,
            made: None,
            retry_at: None,
        }
    }

    /// Updates the rate from the presenter's one-second meter and samples
    /// the resources with it, so the box changes at most once a second.
    /// `card_idle_ms` is the card clock's idle time per frame, if it runs.
    pub(crate) fn set_rate(&mut self, fps: f64, ms: f64, card_idle_ms: Option<f64>) {
        if !self.on {
            return;
        }
        self.rate = Some((tenths(fps).min(99_999), if self.stats { tenths(ms) } else { 0 }));
        if self.stats {
            self.sampled = Some(self.sample(card_idle_ms.and_then(|idle| busy_percent(idle, ms))));
        }
    }

    fn sample(&mut self, gpu: Option<f64>) -> Stats {
        // SAFETY: this query takes a processor group number or all groups.
        let cores = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
        let now = Instant::now();
        let current = crate::framelog::process_times().map(|(user, kernel)| (now, user + kernel));
        let cpu = current
            .zip(self.last_cpu)
            .and_then(|((now, total), (then, old))| cpu_percent(total.checked_sub(old)?, now - then, cores).map(tenths));
        self.last_cpu = current;
        Stats {
            cpu,
            gpu: gpu.map(tenths),
            ram: crate::framelog::process_memory().map(|(working, commit)| [working, commit].map(gb_tenths)),
            vram: crate::host::host()
                .and_then(Host::video_memory_groups)
                .map(|[local, _]| [local[1], local[0]].map(gb_tenths)),
        }
    }

    /// Records the copy and keeps the upload alive through list completion.
    pub(crate) fn draw(
        &mut self,
        host: &Host,
        copy: &HostList,
        back: &ID3D12Resource,
        (width, height, format): (u32, u32, DXGI_FORMAT),
    ) {
        let (Some((fps, ms)), Some(bytes)) = (self.rate, pixel_bytes(format)) else {
            return;
        };
        let display = Display { fps, ms, stats: self.sampled };
        let scale = (height / 360).clamp(2, 12);
        let key = (display, format, scale);
        if self.made.as_ref().is_none_or(|(old, _)| *old != key) {
            self.made = retry_upload(&mut self.retry_at, Instant::now, || {
                Made::new(host, &display.text(), format, scale, bytes).map(|made| (key, made))
            });
        }
        let Some((_, made)) = self.made.as_ref() else {
            return;
        };
        let margin = 4 * scale;
        if made.width + 2 * margin > width || made.height + 2 * margin > height {
            return;
        }
        copy.keep(&made.buffer);
        made.copy_to(&copy.list, back, format, margin);
    }
}

impl Made {
    fn new(host: &Host, text: &str, format: DXGI_FORMAT, scale: u32, bytes: usize) -> Option<Self> {
        let (w, h, lit) = bitmap(text, scale);
        let pitch = (w as usize * bytes).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize);
        let (buffer, data) = host.shared_buffer((pitch * h as usize) as u64)?;
        // SAFETY: the buffer's own mapped bytes, `pitch * h` of them.
        let out = unsafe { std::slice::from_raw_parts_mut(data, pitch * h as usize) };
        for y in 0..h as usize {
            for x in 0..w as usize {
                let at = y * pitch + x * bytes;
                pixel(format, lit[y * w as usize + x], &mut out[at..at + bytes]);
            }
        }
        Some(Self {
            buffer,
            width: w,
            height: h,
            pitch: pitch as u32,
        })
    }

    fn copy_to(&self, list: &ID3D12GraphicsCommandList, back: &ID3D12Resource, format: DXGI_FORMAT, margin: u32) {
        // SAFETY: an open list with the back buffer in copy destination
        // state. The box fits, and the presenter keeps the source alive.
        unsafe {
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: format,
                            Width: self.width,
                            Height: self.height,
                            Depth: 1,
                            RowPitch: self.pitch,
                        },
                    },
                },
            };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(back),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.CopyTextureRegion(&to, margin, margin, 0, &from, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_upload_skips_rebuilds_for_one_second() {
        let start = Instant::now();
        let failed = start + Duration::from_millis(40);
        let now = std::cell::Cell::new(start);
        let mut retry_at = Some(start);
        assert_eq!(retry_upload::<()>(&mut retry_at, || now.get(), || {
            now.set(failed);
            None
        }), None);
        now.set(failed + Duration::from_millis(999));
        assert_eq!(retry_upload::<()>(&mut retry_at, || now.get(), || panic!("rebuilt during the cooldown")), None);
        now.set(failed + Duration::from_secs(1));
        assert_eq!(retry_upload(&mut retry_at, || now.get(), || Some(())), Some(()));
        assert_eq!(retry_at, None);
    }

    #[test]
    fn fps_only_uses_the_original_text_and_box_size() {
        let display = Display {
            fps: 600,
            ms: 167,
            stats: None,
        };
        let text = display.text();
        assert_eq!(text, "60.0 FPS");
        let (width, height, _) = bitmap(&text, 2);
        assert_eq!((width, height), (102, 22));
    }

    #[test]
    fn formats_the_frame_time_and_resource_lines() {
        let display = Display {
            fps: 600,
            ms: 167,
            stats: Some(Stats {
                cpu: Some(125),
                gpu: None,
                ram: Some([123, 157]),
                vram: Some([68, 110]),
            }),
        };
        let text = display.text();
        assert_eq!(
            text,
            "FPS 60.0 / 16.7 ms\nCPU % 12.5\nGPU % n/a\nRAM 12.3 / 15.7 GB\nVRAM 6.8 / 11.0 GB"
        );
        assert!(text.chars().filter(|c| !c.is_whitespace()).all(|c| glyph(c) != [0; 7]));
    }

    #[test]
    fn normalizes_cpu_time_by_wall_time_and_all_logical_processors() {
        let wall = Duration::from_millis(500);
        assert_eq!(cpu_percent(500_000, wall, 8), Some(12.5));
        assert_eq!(cpu_percent(4_000_000, wall, 8), Some(100.0));
        assert_eq!(cpu_percent(0, wall, 8), Some(0.0));
        assert_eq!(cpu_percent(1, wall, 0), None);
        assert_eq!(cpu_percent(1, Duration::ZERO, 8), None);
    }

    #[test]
    fn busy_percent_is_the_non_idle_share_of_the_interval() {
        assert_eq!(busy_percent(4.0, 16.0), Some(75.0));
        assert_eq!(busy_percent(0.0, 16.0), Some(100.0));
        assert_eq!(busy_percent(16.0, 16.0), Some(0.0));
        // An idle stretch counted late can exceed the interval.
        assert_eq!(busy_percent(20.0, 16.0), Some(0.0));
        assert_eq!(busy_percent(-1.0, 16.0), Some(100.0));
        assert_eq!(busy_percent(1.0, 0.0), None);
    }

    #[test]
    fn rounds_bytes_to_tenths_of_decimal_gigabytes() {
        assert_eq!(gb_tenths(1_000_000_000), 10);
        assert_eq!(gb_tenths(1_049_999_999), 10);
        assert_eq!(gb_tenths(1_050_000_000), 11);
        assert_eq!(gb_tenths(u64::MAX), 184_467_440_737);
    }

    #[test]
    fn the_box_uses_the_widest_line_and_leaves_a_gap() {
        let (w, h, lit) = bitmap("FPS\nRAM 0", 1);
        assert_eq!((w, h), (33, 20));
        let row = |y: usize| &lit[y * w as usize..(y + 1) * w as usize];
        assert!(row(8).iter().any(|&dot| dot));
        assert!(row(9).iter().chain(row(10)).all(|&dot| !dot));
        assert!(row(11).iter().any(|&dot| dot));
    }

    #[test]
    fn the_box_holds_every_glyph_with_a_margin() {
        let (w, h, lit) = bitmap("10.3 FPS", 3);
        // Eight glyphs of five dots and seven gaps of one, a pad of two dots all round.
        assert_eq!((w, h), ((8 * 5 + 7 + 4) * 3, (7 + 4) * 3));
        assert_eq!(lit.len(), (w * h) as usize);
        // The pad is box, not text; the "1" has its stem lit.
        assert!(lit[..(w * 6) as usize].iter().all(|&l| !l), "the top pad");
        assert!(
            (0..h).all(|y| !lit[(y * w) as usize] && !lit[(y * w + w - 1) as usize]),
            "the side pads"
        );
        let at = |x: u32, y: u32| lit[(y * w + x) as usize];
        // Glyph 0 ("1"): dot column 2 of row 0 is lit, column 0 is not.
        assert!(at(6 + 2 * 3, 6) && !at(6, 6));
        // A space lights nothing: glyph 4.
        assert!((0..7 * 3).all(|y| (0..5 * 3).all(|x| !at(6 + 4 * 18 + x, 6 + y))));
        // Every glyph used has at least one dot, but the space.
        for c in "0123456789.FPS".chars() {
            assert!(glyph(c).iter().any(|&r| r != 0), "{c}");
        }
    }

    #[test]
    fn pixels_are_in_the_formats_the_layer_presents_in() {
        let mut four = [0u8; 4];
        pixel(DXGI_FORMAT_R8G8B8A8_UNORM, true, &mut four);
        assert_eq!(four, [255, 235, 41, 255]);
        pixel(DXGI_FORMAT_B8G8R8A8_UNORM, true, &mut four);
        assert_eq!(four, [41, 235, 255, 255]);
        pixel(DXGI_FORMAT_R10G10B10A2_UNORM, true, &mut four);
        let v = u32::from_le_bytes(four);
        assert_eq!((v & 1023, v >> 10 & 1023, v >> 20 & 1023, v >> 30), (1023, 941, 164, 3));
        let mut eight = [0u8; 8];
        pixel(DXGI_FORMAT_R16G16B16A16_FLOAT, true, &mut eight);
        assert_eq!(u16::from_le_bytes([eight[0], eight[1]]), 0x3C00, "1.0 as a half");
        assert_eq!(u16::from_le_bytes([eight[6], eight[7]]), 0x3C00);
        assert_eq!(half(0.5), 0x3800);
        assert_eq!(half(0.0), 0);
        assert_eq!(pixel_bytes(DXGI_FORMAT_R16G16B16A16_FLOAT), Some(8));
        assert_eq!(pixel_bytes(DXGI_FORMAT_D32_FLOAT), None);
    }
}

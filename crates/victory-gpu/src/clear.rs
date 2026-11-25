//! Clears of unordered access views. List slots 0x1a8 and 0x1b0 are taken
//! for the console's `ClearUnorderedAccessViewUint` and `...Float`: the two
//! methods after the render target clear on PC. That is inferred, not
//! seen: the newer lists have two more methods somewhere between the
//! clears and `ExecuteIndirect` (`crate::list::newer_slot`), and one run
//! (s20) called 0x1a8 once. The first calls are logged with their
//! arguments read as these methods' (does the second name one of our view
//! records, and does that view name the third), which settles it.
//!
//! The host's own clear needs the view in the list's bound shader-visible
//! heap and again in a CPU-only one; a texture is cleared here instead by
//! copies from an upload buffer that holds the clear value as texels: of
//! the view's mip, its array slices (or depth slices) and the rectangles,
//! nothing else. What the title asks and what is done is counted
//! ([`UAV_CLEARS`]).
//!
//! shortcut: buffers, depth textures shaders also write (their colour
//! twin), multisampled views and formats without a packing here are left
//! out (counted), as all clears were before.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

/// Since the last frame-rate line: unordered access view clears asked,
/// done, left out because [`wanted`] is off, left out as buffers, and left
/// out otherwise (no view record, an unknown format or view).
pub(crate) static UAV_CLEARS: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Whether unordered access view clears are done (`VICTORY_UAV_CLEAR=1`;
/// off by default). Off, they are counted and the first ones logged with
/// their arguments decoded, so that a run can show the slots are what this
/// module takes them for, and dropped, as before.
pub(crate) fn wanted() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UAV_CLEAR").is_some_and(|v| v == "1"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Float,
    Unorm,
    Snorm,
    Uint,
    Sint,
    /// Typeless: the bits as given.
    Raw,
}

/// The channels of a view format in memory order, lowest bits first: their
/// width, kind and which of the four clear values (R, G, B, A) each takes.
fn channels(format: u32) -> Option<Vec<(u32, Kind, usize)>> {
    use Kind::*;
    let same =
        |n: usize, bits: u32, kind: Kind| (0..n).map(|i| (bits, kind, i)).collect::<Vec<_>>();
    Some(match format {
        1 => same(4, 32, Raw),
        2 => same(4, 32, Float),
        3 => same(4, 32, Uint),
        4 => same(4, 32, Sint),
        9 => same(4, 16, Raw),
        10 => same(4, 16, Float),
        11 => same(4, 16, Unorm),
        12 => same(4, 16, Uint),
        13 => same(4, 16, Snorm),
        14 => same(4, 16, Sint),
        15 => same(2, 32, Raw),
        16 => same(2, 32, Float),
        17 => same(2, 32, Uint),
        18 => same(2, 32, Sint),
        23 => vec![(10, Raw, 0), (10, Raw, 1), (10, Raw, 2), (2, Raw, 3)],
        24 => vec![
            (10, Unorm, 0),
            (10, Unorm, 1),
            (10, Unorm, 2),
            (2, Unorm, 3),
        ],
        25 => vec![(10, Uint, 0), (10, Uint, 1), (10, Uint, 2), (2, Uint, 3)],
        26 => vec![(11, Float, 0), (11, Float, 1), (10, Float, 2)],
        27 => same(4, 8, Raw),
        // sRGB views are plain UNORM on the host (it has no sRGB unordered
        // access).
        28 | 29 => same(4, 8, Unorm),
        30 => same(4, 8, Uint),
        31 => same(4, 8, Snorm),
        32 => same(4, 8, Sint),
        33 => same(2, 16, Raw),
        34 => same(2, 16, Float),
        35 => same(2, 16, Unorm),
        36 => same(2, 16, Uint),
        37 => same(2, 16, Snorm),
        38 => same(2, 16, Sint),
        39 => same(1, 32, Raw),
        41 => same(1, 32, Float),
        42 => same(1, 32, Uint),
        43 => same(1, 32, Sint),
        48 => same(2, 8, Raw),
        49 => same(2, 8, Unorm),
        50 => same(2, 8, Uint),
        51 => same(2, 8, Snorm),
        52 => same(2, 8, Sint),
        53 => same(1, 16, Raw),
        54 => same(1, 16, Float),
        56 => same(1, 16, Unorm),
        57 => same(1, 16, Uint),
        58 => same(1, 16, Snorm),
        59 => same(1, 16, Sint),
        60 => same(1, 8, Raw),
        61 => same(1, 8, Unorm),
        62 => same(1, 8, Uint),
        63 => same(1, 8, Snorm),
        64 => same(1, 8, Sint),
        65 => vec![(8, Unorm, 3)],
        // B, G, R, A in memory.
        87 | 91 => vec![(8, Unorm, 2), (8, Unorm, 1), (8, Unorm, 0), (8, Unorm, 3)],
        88 | 93 => vec![(8, Unorm, 2), (8, Unorm, 1), (8, Unorm, 0), (8, Raw, 4)],
        90 => vec![(8, Raw, 2), (8, Raw, 1), (8, Raw, 0), (8, Raw, 3)],
        _ => return None,
    })
}

/// A float as a smaller float of `exponent` and `mantissa` bits (signed:
/// with a sign bit above them), rounded to nearest even; NaN stays NaN,
/// what is too large becomes infinity, an unsigned one's negatives 0.
fn small_float(v: f32, exponent: u32, mantissa: u32, signed: bool) -> u64 {
    let bias = (1i64 << (exponent - 1)) - 1;
    let top = (1u64 << exponent) - 1;
    if v.is_nan() {
        return top << mantissa | 1 << (mantissa - 1);
    }
    let sign = if v.is_sign_negative() && v != 0.0 {
        if !signed {
            return 0;
        }
        1u64 << (exponent + mantissa)
    } else {
        0
    };
    let a = f64::from(v.abs());
    if a.is_infinite() {
        return sign | top << mantissa;
    }
    if a == 0.0 {
        return sign;
    }
    let e = ((a.to_bits() >> 52) & 0x7ff) as i64 - 1023;
    if e + bias <= 0 {
        // Below the normal range: steps of 2^(1 - bias - mantissa).
        let steps =
            (a * 2f64.powi((bias - 1 + i64::from(mantissa)) as i32)).round_ties_even() as u64;
        return sign | steps;
    }
    let mut exp = (e + bias) as u64;
    let mut man =
        ((a / 2f64.powi(e as i32) - 1.0) * 2f64.powi(mantissa as i32)).round_ties_even() as u64;
    if man == 1 << mantissa {
        man = 0;
        exp += 1;
    }
    if exp >= top {
        return sign | top << mantissa;
    }
    sign | exp << mantissa | man
}

/// One channel of `bits` bits of `kind` for clear value `value` (a float's
/// bits for a float clear).
fn channel(bits: u32, kind: Kind, value: u32, float: bool) -> u64 {
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    if !float {
        // A uint clear copies each value's lower bits, whatever the format.
        return u64::from(value) & mask;
    }
    let f = f32::from_bits(value);
    let max = mask as f64;
    let v = match kind {
        Kind::Float if bits == 32 => u64::from(value),
        Kind::Float if bits == 16 => small_float(f, 5, 10, true),
        Kind::Float => small_float(f, 5, bits - 5, false),
        Kind::Unorm => {
            let f = if f.is_nan() {
                0.0
            } else {
                f64::from(f).clamp(0.0, 1.0)
            };
            (f * max).round_ties_even() as u64
        }
        Kind::Snorm => {
            let f = if f.is_nan() {
                0.0
            } else {
                f64::from(f).clamp(-1.0, 1.0)
            };
            let half = ((1u64 << (bits - 1)) - 1) as f64;
            (f * half).round_ties_even() as i64 as u64
        }
        Kind::Uint => f64::from(f).clamp(0.0, max) as u64,
        Kind::Sint => {
            let half = (1i64 << (bits - 1)) as f64;
            f64::from(f).clamp(-half, half - 1.0) as i64 as u64
        }
        Kind::Raw => u64::from(value),
    };
    v & mask
}

/// The texel a clear writes to a view of format `format` (the DXGI number,
/// as the host has it): values R, G, B, A (a float clear's as float bits),
/// packed in the format's order. `None` for a format without a packing
/// here.
pub(crate) fn texel(format: u32, values: [u32; 4], float: bool) -> Option<Vec<u8>> {
    let channels = channels(format)?;
    let mut bits = 0u128;
    let mut at = 0;
    for (width, kind, from) in channels {
        let value = values.get(from).copied().unwrap_or(u32::MAX);
        bits |= u128::from(channel(width, kind, value, float)) << at;
        at += width;
    }
    Some(bits.to_le_bytes()[..(at / 8) as usize].to_vec())
}

/// The copies a clear of a view (`view`: its words, `None`: the resource's
/// own view) of a texture made as `desc` on the host is made of: per
/// subresource, the box of it to write. `rects` (left, top, right, bottom)
/// are cut to the mip; none: the whole mip. `None` for a view this does not
/// clear (buffers, multisampled textures, a mip past the chain).
pub(crate) fn plan(
    view: Option<&[u32; 10]>,
    desc: &D3D12_RESOURCE_DESC,
    rects: &[[i32; 4]],
) -> Option<Vec<(u32, D3D12_BOX)>> {
    const TEXTURE_1D: u32 = 2;
    const TEXTURE_1D_ARRAY: u32 = 3;
    const TEXTURE_2D: u32 = 4;
    const TEXTURE_2D_ARRAY: u32 = 5;
    const TEXTURE_3D: u32 = 8;
    let three_d = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
    let layers = if three_d {
        1
    } else {
        u32::from(desc.DepthOrArraySize).max(1)
    };
    let mips = u32::from(desc.MipLevels).max(1);
    if desc.SampleDesc.Count > 1 || desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
        return None;
    }
    // (mip, first layer or depth slice, how many; u32::MAX: the rest).
    let (mip, first, count) = match view.filter(|w| w[1] != 0) {
        None => (0, 0, u32::MAX),
        Some(w) => match w[1] {
            TEXTURE_1D | TEXTURE_2D => (w[2], 0, 1),
            TEXTURE_1D_ARRAY | TEXTURE_2D_ARRAY | TEXTURE_3D => (w[2], w[3], w[4]),
            _ => return None,
        },
    };
    if mip >= mips {
        return None;
    }
    let width = (desc.Width >> mip).max(1) as u32;
    let height = (desc.Height >> mip).max(1);
    let depth = if three_d {
        (u32::from(desc.DepthOrArraySize) >> mip).max(1)
    } else {
        layers
    };
    let end = first.saturating_add(count).min(depth);
    if first >= end {
        return Some(Vec::new());
    }
    let whole = [[0, 0, width as i32, height as i32]];
    let rects = if rects.is_empty() { &whole[..] } else { rects };
    let mut copies = Vec::new();
    for r in rects {
        let (left, top) = (r[0].max(0) as u32, r[1].max(0) as u32);
        let (right, bottom) = (
            r[2].clamp(0, width as i32) as u32,
            r[3].clamp(0, height as i32) as u32,
        );
        if left >= right || top >= bottom {
            continue;
        }
        let area = |front, back| D3D12_BOX {
            left,
            top,
            front,
            right,
            bottom,
            back,
        };
        if three_d {
            copies.push((mip, area(first, end)));
        } else {
            copies.extend((first..end).map(|layer| (mip + layer * mips, area(0, 1))));
        }
    }
    Some(copies)
}

/// Records `copies` (from [`plan`]) of `texel` into `texture` (in the copy
/// destination state, of host format `format`) on `list`, through upload
/// memory that `upload` gives (a buffer and its mapping of the bytes
/// asked; it must live until the list has run). False if no upload memory
/// was had.
///
/// # Safety
/// `list` is open; `texture` is live, of format `format` with elements of
/// `texel`'s size, and every box lies inside its subresource.
pub(crate) unsafe fn record(
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    format: DXGI_FORMAT,
    texel: &[u8],
    copies: &[(u32, D3D12_BOX)],
    upload: impl FnOnce(u64) -> Option<(ID3D12Resource, *mut u8)>,
) -> bool {
    if copies.is_empty() || texel.is_empty() {
        return true;
    }
    let size = |b: &D3D12_BOX| (b.right - b.left, b.bottom - b.top, b.back - b.front);
    let (w, h, d) = copies.iter().fold((1, 1, 1), |(w, h, d), (_, b)| {
        let (bw, bh, bd) = size(b);
        (w.max(bw), h.max(bh), d.max(bd))
    });
    // Every element size divides 256, so the rows are whole texels and the
    // buffer is one run of them; each copy takes the corner it needs.
    let pitch = (w as usize * texel.len()).next_multiple_of(256);
    let bytes = pitch * h as usize * d as usize;
    let Some((buffer, data)) = upload(bytes as u64) else {
        return false;
    };
    // SAFETY: the mapping holds `bytes` bytes.
    let mapped = unsafe { std::slice::from_raw_parts_mut(data, bytes) };
    for (i, byte) in mapped.iter_mut().enumerate() {
        *byte = texel[i % texel.len()];
    }
    for (subresource, b) in copies {
        let (bw, bh, bd) = size(b);
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: *subresource,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above; the caller keeps the buffer.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: bw,
                        Height: bh,
                        Depth: bd,
                        RowPitch: pitch as u32,
                    },
                },
            },
        };
        // SAFETY: the caller's open list; both locations name live
        // resources and the box lies inside the subresource.
        unsafe { list.CopyTextureRegion(&to, b.left, b.top, b.front, &from, None) };
    }
    true
}

/// On the software adapter (WARP): textures of several mips and slices
/// filled with a known pattern, one subresource or rectangle cleared or
/// uploaded through our path, everything read back, the cleared part and
/// what lies around it checked.
#[cfg(test)]
mod warp {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    struct Warp {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        done: u64,
        event: HANDLE,
        /// Upload buffers the open list copies from.
        held: Vec<ID3D12Resource>,
    }

    impl Warp {
        fn new() -> Option<Warp> {
            // SAFETY: plain DXGI and D3D12 object creation.
            unsafe {
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
                let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    return None;
                }
                let mut device: Option<ID3D12Device> = None;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device).ok()?;
                let device = device?;
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .ok()?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                Some(Warp {
                    device,
                    queue,
                    allocator,
                    list,
                    fence,
                    done: 0,
                    event,
                    held: Vec::new(),
                })
            }
        }

        fn run(&mut self) {
            self.done += 1;
            // SAFETY: our own list, queue and fence.
            unsafe {
                self.list.Close().unwrap();
                self.queue
                    .ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
                self.queue.Signal(&self.fence, self.done).unwrap();
                self.fence
                    .SetEventOnCompletion(self.done, self.event)
                    .unwrap();
                WaitForSingleObject(self.event, INFINITE);
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
            self.held.clear();
        }

        fn buffer(&self, kind: D3D12_HEAP_TYPE, size: u64) -> (ID3D12Resource, *mut u8) {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size.max(256),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let state = if kind == D3D12_HEAP_TYPE_UPLOAD {
                D3D12_RESOURCE_STATE_GENERIC_READ
            } else {
                D3D12_RESOURCE_STATE_COPY_DEST
            };
            let mut buffer: Option<ID3D12Resource> = None;
            let mut data = std::ptr::null_mut();
            // SAFETY: plain creation and a whole mapping.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: kind,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        state,
                        None,
                        &mut buffer,
                    )
                    .unwrap();
                let buffer = buffer.unwrap();
                buffer.Map(0, None, Some(&mut data)).unwrap();
                (buffer, data.cast())
            }
        }

        fn texture(&self, desc: &D3D12_RESOURCE_DESC) -> ID3D12Resource {
            let mut texture: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            unsafe {
                self.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut texture,
                    )
                    .unwrap();
            }
            texture.unwrap()
        }

        /// Footprints and row counts of every subresource of `desc`, and
        /// the bytes they take.
        fn layout(
            &self,
            desc: &D3D12_RESOURCE_DESC,
            first: u32,
            count: u32,
        ) -> (Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>, Vec<u32>, u64) {
            let mut footprints =
                vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
            let mut rows = vec![0u32; count as usize];
            let mut total = 0;
            // SAFETY: outputs for `count` subresources.
            unsafe {
                self.device.GetCopyableFootprints(
                    desc,
                    first,
                    count,
                    0,
                    Some(footprints.as_mut_ptr()),
                    Some(rows.as_mut_ptr()),
                    None,
                    Some(&mut total),
                )
            };
            (footprints, rows, total)
        }

        /// Copies `bytes`, laid out as `layout(desc, first, n)`, into
        /// subresources `first..first + n` of `texture`.
        fn upload(&mut self, texture: &ID3D12Resource, first: u32, bytes: &[u8]) {
            // SAFETY: a live texture.
            let desc = unsafe { texture.GetDesc() };
            let count = bytes_count(&desc) - first;
            let (footprints, _, total) = self.layout(&desc, first, count);
            let (buffer, data) = self.buffer(D3D12_HEAP_TYPE_UPLOAD, total);
            // SAFETY: the mapping holds `total` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len().min(total as usize))
            };
            for (i, f) in footprints.iter().enumerate() {
                // SAFETY: an open list and live resources.
                unsafe {
                    self.list.CopyTextureRegion(
                        &at(texture, first + i as u32),
                        0,
                        0,
                        0,
                        &placed(&buffer, *f),
                        None,
                    )
                };
            }
            self.held.push(buffer);
        }

        /// Every subresource of `texture` (in the copy source state after),
        /// read back: one vector of tightly packed rows each.
        fn read(
            &mut self,
            texture: &ID3D12Resource,
            before: D3D12_RESOURCE_STATES,
        ) -> Vec<Vec<u8>> {
            // SAFETY: a live texture.
            let desc = unsafe { texture.GetDesc() };
            let count = bytes_count(&desc);
            let (footprints, rows, total) = self.layout(&desc, 0, count);
            let (buffer, data) = self.buffer(D3D12_HEAP_TYPE_READBACK, total);
            // SAFETY: an open list and live resources.
            unsafe {
                if before != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    self.list.ResourceBarrier(&[crate::host::transition(
                        texture,
                        before,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                    )]);
                }
                for (i, f) in footprints.iter().enumerate() {
                    self.list.CopyTextureRegion(
                        &placed(&buffer, *f),
                        0,
                        0,
                        0,
                        &at(texture, i as u32),
                        None,
                    );
                }
            }
            self.run();
            let element = crate::resource::element(desc.Format.0 as u32).unwrap().0 as usize;
            footprints
                .iter()
                .zip(&rows)
                .map(|(f, &n)| {
                    let row = f
                        .Footprint
                        .Width
                        .div_ceil(if element >= 8 && is_bc(desc.Format) {
                            4
                        } else {
                            1
                        }) as usize
                        * element;
                    (0..f.Footprint.Depth as usize * n as usize)
                        .flat_map(|r| {
                            let at = f.Offset as usize + r * f.Footprint.RowPitch as usize;
                            // SAFETY: inside the mapped read-back buffer.
                            unsafe { std::slice::from_raw_parts(data.add(at), row) }.to_vec()
                        })
                        .collect()
                })
                .collect()
        }
    }

    fn is_bc(format: DXGI_FORMAT) -> bool {
        matches!(format.0, 70..=84 | 94..=99)
    }

    fn bytes_count(desc: &D3D12_RESOURCE_DESC) -> u32 {
        let layers = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            1
        } else {
            u32::from(desc.DepthOrArraySize)
        };
        u32::from(desc.MipLevels) * layers
    }

    fn at(texture: &ID3D12Resource, subresource: u32) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: subresource,
            },
        }
    }

    fn placed(
        buffer: &ID3D12Resource,
        footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    ) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        }
    }

    fn texture_desc(
        format: i32,
        width: u64,
        height: u32,
        layers: u16,
        mips: u16,
        flags: D3D12_RESOURCE_FLAGS,
    ) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width,
            Height: height,
            DepthOrArraySize: layers,
            MipLevels: mips,
            Format: DXGI_FORMAT(format),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: flags,
            ..Default::default()
        }
    }

    /// Fills every subresource with a pattern no clear writes: byte `b` of
    /// subresource `s` is `(s * 37 + b * 7) | 1`... as laid out for a copy.
    fn fill(w: &mut Warp, texture: &ID3D12Resource) {
        // SAFETY: a live texture.
        let desc = unsafe { texture.GetDesc() };
        let (footprints, rows, total) = w.layout(&desc, 0, bytes_count(&desc));
        let mut bytes = vec![0u8; total as usize];
        for (s, (f, &n)) in footprints.iter().zip(&rows).enumerate() {
            for r in 0..f.Footprint.Depth as usize * n as usize {
                let start = f.Offset as usize + r * f.Footprint.RowPitch as usize;
                // The last row of all has no padding after it.
                let end = (start + f.Footprint.RowPitch as usize).min(bytes.len());
                for (b, byte) in bytes[start..end]
                    .iter_mut()
                    .enumerate()
                {
                    *byte = (s * 37 + r * 11 + b * 7) as u8 | 1;
                }
            }
        }
        w.upload(texture, 0, &bytes);
    }

    /// Checks that, of every subresource read back (`after`) against the
    /// same before the clear (`before`), the texels inside `cleared`
    /// (subresource, box) are `texel` and every other one is as it was.
    fn check(
        desc: &D3D12_RESOURCE_DESC,
        before: &[Vec<u8>],
        after: &[Vec<u8>],
        texel: &[u8],
        cleared: &[(u32, D3D12_BOX)],
    ) {
        let n = texel.len();
        let mips = u32::from(desc.MipLevels);
        for (s, (old, new)) in before.iter().zip(after).enumerate() {
            let mip = s as u32 % mips;
            let width = (desc.Width >> mip).max(1) as usize;
            let height = (desc.Height >> mip).max(1) as usize;
            for y in 0..height {
                for x in 0..width {
                    let inside = cleared.iter().any(|(sub, b)| {
                        *sub == s as u32
                            && (b.left as usize..b.right as usize).contains(&x)
                            && (b.top as usize..b.bottom as usize).contains(&y)
                    });
                    let i = (y * width + x) * n;
                    let want = if inside { texel } else { &old[i..i + n] };
                    assert_eq!(
                        &new[i..i + n],
                        want,
                        "subresource {s} ({x}, {y}) {}",
                        if inside { "cleared" } else { "kept" }
                    );
                }
            }
        }
    }

    fn rtv_desc(words: [u32; 6]) -> D3D12_RENDER_TARGET_VIEW_DESC {
        // SAFETY: the host's 24-byte layout, as `RtvRecord::host_desc` reads it.
        unsafe {
            (&raw const words)
                .cast::<D3D12_RENDER_TARGET_VIEW_DESC>()
                .read_unaligned()
        }
    }

    fn dsv_desc(words: [u32; 6]) -> D3D12_DEPTH_STENCIL_VIEW_DESC {
        // SAFETY: as above.
        unsafe {
            (&raw const words)
                .cast::<D3D12_DEPTH_STENCIL_VIEW_DESC>()
                .read_unaligned()
        }
    }

    fn heap(
        w: &Warp,
        kind: D3D12_DESCRIPTOR_HEAP_TYPE,
    ) -> (ID3D12DescriptorHeap, D3D12_CPU_DESCRIPTOR_HANDLE) {
        // SAFETY: plain creation.
        unsafe {
            let heap: ID3D12DescriptorHeap = w
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .unwrap();
            let at = heap.GetCPUDescriptorHandleForHeapStart();
            (heap, at)
        }
    }

    /// A render target clear through a described view (the title's words:
    /// a 2D array view of mip 1, slice 2) with a rectangle reaches that
    /// rectangle of that subresource and nothing else.
    #[test]
    fn warp_a_target_clear_reaches_its_mip_slice_and_rect_only() {
        let Some(mut w) = Warp::new() else { return };
        let desc = texture_desc(28, 32, 16, 4, 3, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET);
        let texture = w.texture(&desc);
        fill(&mut w, &texture);
        let before = w.read(&texture, D3D12_RESOURCE_STATE_COPY_DEST);
        let (_heap, rtv) = heap(&w, D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let rect = windows::Win32::Foundation::RECT {
            left: 3,
            top: 2,
            right: 11,
            bottom: 7,
        };
        // SAFETY: an open list, a live texture, our own heap.
        unsafe {
            w.device
                .CreateRenderTargetView(&texture, Some(&rtv_desc([28, 5, 1, 2, 1, 0])), rtv);
            w.list.ResourceBarrier(&[crate::host::transition(
                &texture,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )]);
            w.list
                .ClearRenderTargetView(rtv, &[1.0, 0.0, 1.0, 0.0], Some(&[rect]));
        }
        let after = w.read(&texture, D3D12_RESOURCE_STATE_RENDER_TARGET);
        let cleared = [(
            1 + 2 * 3,
            D3D12_BOX {
                left: 3,
                top: 2,
                right: 11,
                bottom: 7,
                front: 0,
                back: 1,
            },
        )];
        check(&desc, &before, &after, &[255, 0, 255, 0], &cleared);
    }

    /// A depth clear through a described view (mip 1 of slice 3) and through
    /// one the title made without a description, of a typeless host copy
    /// (`crate::descriptor::null_view_words`): spelled as by default, slice 0
    /// alone; with `VICTORY_NULL_VIEW_WHOLE=1`, mip 0 of every slice, as on the
    /// console, and no other mip.
    #[test]
    fn warp_depth_clears_reach_their_slices_and_mips_only() {
        let Some(mut w) = Warp::new() else { return };
        // R32_TYPELESS: the host copy of a depth texture shaders read.
        let desc = texture_desc(39, 16, 16, 6, 2, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL);
        let texture = w.texture(&desc);
        fill(&mut w, &texture);
        let before = w.read(&texture, D3D12_RESOURCE_STATE_COPY_DEST);
        let (_heap, dsv) = heap(&w, D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        let one = |w: &mut Warp,
                   words: [u32; 6],
                   state: D3D12_RESOURCE_STATES,
                   rects: &[windows::Win32::Foundation::RECT]| {
            // SAFETY: as above.
            unsafe {
                w.device
                    .CreateDepthStencilView(&texture, Some(&dsv_desc(words)), dsv);
                w.list.ResourceBarrier(&[crate::host::transition(
                    &texture,
                    state,
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                )]);
                w.list
                    .ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.5, 0, Some(rects));
            }
            w.read(&texture, D3D12_RESOURCE_STATE_DEPTH_WRITE)
        };
        let half = 0.5f32.to_le_bytes();
        // D32_FLOAT, a 2D array view: mip 1, slice 3, one slice; a rect.
        let rect = windows::Win32::Foundation::RECT {
            left: 1,
            top: 2,
            right: 5,
            bottom: 8,
        };
        let after = one(
            &mut w,
            [40, 4, 0, 1, 3, 1],
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            &[rect],
        );
        let cleared = [(
            1 + 3 * 2,
            D3D12_BOX {
                left: 1,
                top: 2,
                right: 5,
                bottom: 8,
                front: 0,
                back: 1,
            },
        )];
        check(&desc, &before, &after, &half, &cleared);
        // Without a description, as the host is given it by default: a 2D
        // view, which reaches slice 0 alone.
        let after = {
            let before = after;
            let after = one(&mut w, [40, 3, 0, 0, 0, 0], D3D12_RESOURCE_STATE_COPY_SOURCE, &[]);
            let slice_0 = [(0, D3D12_BOX { left: 0, top: 0, right: 16, bottom: 16, front: 0, back: 1 })];
            check(&desc, &before, &after, &half, &slice_0);
            // Slice 0 back as it was, for the next clear to show itself.
            // SAFETY: an open list, a live texture.
            unsafe {
                w.list.ResourceBarrier(&[crate::host::transition(
                    &texture,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
            }
            fill(&mut w, &texture);
            w.read(&texture, D3D12_RESOURCE_STATE_COPY_DEST)
        };
        // Without a description, with VICTORY_NULL_VIEW_WHOLE=1: the view a
        // null description stands for.
        let words = crate::descriptor::null_view_words(true, 40, 3, 6, 1).expect("a depth view");
        assert_eq!(
            words,
            [40, 4, 0, 0, 0, 6],
            "a 2D array view of mip 0, all six slices"
        );
        let again = one(&mut w, words, D3D12_RESOURCE_STATE_COPY_SOURCE, &[]);
        let whole: Vec<(u32, D3D12_BOX)> = (0..6)
            .map(|s| {
                (
                    s * 2,
                    D3D12_BOX {
                        left: 0,
                        top: 0,
                        right: 16,
                        bottom: 16,
                        front: 0,
                        back: 1,
                    },
                )
            })
            .collect();
        check(&desc, &after, &again, &half, &whole);
    }

    /// An unordered access clear by copies ([`plan`], [`record`]): mip 1 of
    /// slice 1 of an array, within a rectangle, uint and float. (A volume's
    /// depth slices are planned in `a_clear_covers_its_views_subresources_and_rects`
    /// and not run here.)
    #[test]
    fn warp_unordered_clears_by_copies_reach_their_view_only() {
        let Some(mut w) = Warp::new() else { return };
        for (format, view_format, values, float) in [
            (42, 42, [0x1234_5678, 0, 0, 0], false),
            (
                10,
                10,
                [1.0f32.to_bits(), (-2.0f32).to_bits(), 0, 0.5f32.to_bits()],
                true,
            ),
        ] {
            let desc = texture_desc(
                format,
                24,
                20,
                3,
                3,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            );
            let texture = w.texture(&desc);
            fill(&mut w, &texture);
            let before = w.read(&texture, D3D12_RESOURCE_STATE_COPY_DEST);
            let view = [view_format, 5, 1, 1, 1, 0, 0, 0, 0, 0];
            let copies = plan(Some(&view), &desc, &[[2, 1, 9, 30]]).unwrap();
            let texel = texel(view_format, values, float).unwrap();
            // SAFETY: an open list, a live texture.
            unsafe {
                w.list.ResourceBarrier(&[crate::host::transition(
                    &texture,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
                let device = w.device.clone();
                let mut held = None;
                let made = record(&w.list, &texture, desc.Format, &texel, &copies, |bytes| {
                    let (buffer, data) = Warp::buffer_on(&device, bytes);
                    held = Some(buffer.clone());
                    Some((buffer, data))
                });
                assert!(made);
                w.held.extend(held);
            }
            let after = w.read(&texture, D3D12_RESOURCE_STATE_COPY_DEST);
            // Mip 1 is 12x10: the rectangle is cut to (2, 1)-(9, 10).
            let cleared = [(
                1 + 3,
                D3D12_BOX {
                    left: 2,
                    top: 1,
                    right: 9,
                    bottom: 10,
                    front: 0,
                    back: 1,
                },
            )];
            assert_eq!(copies, cleared);
            check(&desc, &before, &after, &texel, &cleared);
        }
    }

    impl Warp {
        fn buffer_on(device: &ID3D12Device, size: u64) -> (ID3D12Resource, *mut u8) {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: size.max(256),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut buffer: Option<ID3D12Resource> = None;
            let mut data = std::ptr::null_mut();
            // SAFETY: plain creation and a whole mapping.
            unsafe {
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_UPLOAD,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_GENERIC_READ,
                        None,
                        &mut buffer,
                    )
                    .unwrap();
                let buffer = buffer.unwrap();
                buffer.Map(0, None, Some(&mut data)).unwrap();
                (buffer, data.cast())
            }
        }
    }

    /// The reference swizzle the detiling is checked against: the byte
    /// offset of element (`x`, `y`) of mip `mip` of array slice `slice`,
    /// worked out address bit by address bit. The 256-byte patterns are
    /// AMD addrlib's `GFX10_SW_256_S_RBPLUS` and `GFX10_SW_256_D_RBPLUS`
    /// (Mesa `src/amd/addrlib`, as `victory_formats::texture` transcribes
    /// them; written out again here from those tables, not shared), the
    /// bits above them alternate y and x from y (addrlib's thin block
    /// equations), blocks go row by row. Where each mip and the mip tail
    /// lie is taken from the layout (`Mip::offset`, `Mip::tail`; checked
    /// by hand in `victory_formats`' own tests): this checks the element
    /// addressing and our staging path, not the placement rules.
    fn reference_offset(
        l: &victory_formats::texture::Layout,
        mip: u32,
        x: u32,
        y: u32,
        slice: u32,
    ) -> usize {
        const S: [&str; 5] = [
            "x0x1x2x3y0y1y2y3",
            "--x0x1x2y0y1y2x3",
            "----x0x1y0y1y2x2",
            "------x0y0y1x1x2",
            "--------y0y1x0x1",
        ];
        const D: [&str; 5] = [
            "x0x1x2y1y0y2x3y3",
            "--x0x1x2y0y1y2x3",
            "----x0x1y0y1x2y2",
            "------x0y0x1x2y1",
            "--------x0y0x1y1",
        ];
        let m = &l.mips[mip as usize];
        let n = l.desc.element_bytes;
        let base = u64::from(slice) * l.slice_size + m.offset;
        if l.desc.swizzle.0 == 0 {
            return (base + (u64::from(y) * u64::from(m.pitch) + u64::from(x)) * u64::from(n))
                as usize;
        }
        let (x, y) = (x + m.tail.map_or(0, |t| t.x), y + m.tail.map_or(0, |t| t.y));
        let display = matches!(l.desc.swizzle.0, 2 | 6 | 10);
        let micro = (if display { D } else { S })[n.trailing_zeros() as usize].as_bytes();
        let log2 = l.alignment.trailing_zeros();
        let [bw, bh, _] = l.block;
        let (ix, iy) = (x % bw, y % bh);
        let (mut next_x, mut next_y) = (0, 0);
        let mut inside = 0u64;
        for bit in 0..log2 {
            let take = |axis: u8, index: u32| {
                u64::from(((if axis == b'x' { ix } else { iy }) >> index) & 1) << bit
            };
            if bit < 8 {
                let (axis, index) = (micro[2 * bit as usize], micro[2 * bit as usize + 1]);
                if axis == b'-' {
                    continue;
                }
                let index = u32::from(index - b'0');
                inside |= take(axis, index);
                if axis == b'x' {
                    next_x = next_x.max(index + 1);
                } else {
                    next_y = next_y.max(index + 1);
                }
            } else if (bit - 8) % 2 == 0 {
                inside |= take(b'y', next_y);
                next_y += 1;
            } else {
                inside |= take(b'x', next_x);
                next_x += 1;
            }
        }
        let block = u64::from(y / bh) * u64::from(m.pitch / bw) + u64::from(x / bw);
        (base + (block << log2) + inside) as usize
    }

    /// The staging path's detiling (`crate::texture::detile_parts`, with the
    /// footprints and parts `crate::texture::staging` uses) of synthetic
    /// tiled memory, uploaded to a host texture and read back, against the
    /// reference: a BC1 texture of 9 mips viewed from mip 1, given only the
    /// memory mips 1 and up take (mip 0 keeps what it held; mips 2-8 are
    /// the tail), a 4-byte one of 7 mips viewed from mip 3, and an
    /// array of 4-byte texels whose view from mip 1 stages all of it.
    #[test]
    fn warp_detiling_writes_each_resident_mip_as_the_reference_says() {
        use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
        let Some(mut w) = Warp::new() else { return };
        for (format, width, height, layers, mips, swizzle, view_first, staged_first) in [
            (
                71u32,
                256u32,
                256u32,
                1u32,
                9u32,
                Swizzle::SW_4KB_S,
                1u32,
                1u32,
            ),
            (28, 40, 24, 2, 4, Swizzle::SW_64KB_D, 1, 0),
            (28, 100, 60, 1, 7, Swizzle::SW_64KB_D, 3, 3),
        ] {
            let layout = Texture::new(&TextureDesc {
                dimension: Dimension::Tex2d,
                width,
                height,
                depth_or_array: layers,
                mips,
                format,
                samples: 1,
                swizzle,
            })
            .unwrap()
            .planes[0]
                .layout
                .clone();
            let (first, size) = crate::texture::staged_part(&layout, mips, view_first);
            assert_eq!(first, staged_first, "{format} {swizzle:?}");
            // Only the memory of the staged mips: a read past it panics.
            let tiled: Vec<u8> = (0..size)
                .map(|i| (i.wrapping_mul(0x9e37_79b1) >> 13) as u8)
                .collect();
            let desc = texture_desc(
                format as i32,
                u64::from(width),
                height,
                layers as u16,
                mips as u16,
                D3D12_RESOURCE_FLAG_NONE,
            );
            let texture = w.texture(&desc);
            fill(&mut w, &texture);
            let before = w.read(&texture, D3D12_RESOURCE_STATE_COPY_DEST);
            let count = mips * layers - first;
            let (footprints, rows, total) = w.layout(&desc, first, count);
            let mut staging = vec![0xeeu8; total as usize];
            crate::texture::detile_parts(
                &layout,
                &tiled,
                &footprints,
                &rows,
                &mut staging,
                |i| crate::texture::part_of(mips, first, i),
                u32::MAX,
            )
            .expect("detiled");
            // SAFETY: a live texture.
            unsafe {
                w.list.ResourceBarrier(&[crate::host::transition(
                    &texture,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
            }
            w.upload(&texture, first, &staging);
            let after = w.read(&texture, D3D12_RESOURCE_STATE_COPY_DEST);
            let n = layout.desc.element_bytes as usize;
            for (s, (old, new)) in before.iter().zip(&after).enumerate() {
                let (mip, slice) = (s as u32 % mips, s as u32 / mips);
                if mip < first {
                    assert_eq!(
                        old, new,
                        "{format}: mip {mip} is not staged and keeps its texels"
                    );
                    continue;
                }
                let m = &layout.mips[mip as usize];
                for y in 0..m.height {
                    for x in 0..m.width {
                        let i = (y * m.width + x) as usize * n;
                        let at = reference_offset(&layout, mip, x, y, slice);
                        assert_eq!(
                            &new[i..i + n],
                            &tiled[at..at + n],
                            "{format} {swizzle:?} mip {mip} slice {slice} ({x}, {y})"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texels_pack_in_the_formats_order() {
        // A uint clear takes each value's low bits.
        assert_eq!(
            texel(30, [1, 2, 3, 0x1ff], false),
            Some(vec![1, 2, 3, 0xff])
        );
        assert_eq!(
            texel(87, [1, 2, 3, 4], false),
            Some(vec![3, 2, 1, 4]),
            "BGRA"
        );
        assert_eq!(
            texel(42, [0xdead_beef, 0, 0, 0], false),
            Some(0xdead_beefu32.to_le_bytes().to_vec())
        );
        assert_eq!(
            texel(25, [0x3ff, 0, 0x155, 3], false),
            Some((0x3ffu32 | 0x155 << 20 | 3 << 30).to_le_bytes().to_vec())
        );
        let f = |v: [f32; 4]| v.map(f32::to_bits);
        assert_eq!(
            texel(28, f([1.0, 0.0, 1.0, 0.0]), true),
            Some(vec![255, 0, 255, 0])
        );
        assert_eq!(
            texel(31, f([-1.0, 1.0, 0.0, 2.0]), true),
            Some(vec![0x81, 0x7f, 0, 0x7f])
        );
        assert_eq!(
            texel(41, f([0.5, 9.0, 9.0, 9.0]), true),
            Some(0.5f32.to_bits().to_le_bytes().to_vec())
        );
        // Half floats: 1.0 is 0x3c00, -2.0 0xc000, 65520 rounds to infinity.
        assert_eq!(
            texel(54, f([1.0, 0.0, 0.0, 0.0]), true),
            Some(vec![0x00, 0x3c])
        );
        assert_eq!(
            texel(54, f([-2.0, 0.0, 0.0, 0.0]), true),
            Some(vec![0x00, 0xc0])
        );
        assert_eq!(
            texel(54, f([65520.0, 0.0, 0.0, 0.0]), true),
            Some(vec![0x00, 0x7c])
        );
        assert_eq!(
            texel(54, f([5.960_464_5e-8, 0.0, 0.0, 0.0]), true),
            Some(vec![0x01, 0x00]),
            "the smallest half"
        );
        // R11G11B10: 1.0 is 0x3c0 in the 11-bit fields, 0x1e0 in the 10-bit one.
        assert_eq!(
            texel(26, f([1.0, 1.0, 1.0, 0.0]), true),
            Some(
                (0x3c0u32 | 0x3c0 << 11 | 0x1e0 << 22)
                    .to_le_bytes()
                    .to_vec()
            )
        );
        assert_eq!(
            texel(70, [0; 4], false),
            None,
            "block compression has no packing"
        );
    }

    fn desc(
        dimension: D3D12_RESOURCE_DIMENSION,
        width: u64,
        height: u32,
        layers: u16,
        mips: u16,
    ) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: dimension,
            Width: width,
            Height: height,
            DepthOrArraySize: layers,
            MipLevels: mips,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_clear_covers_its_views_subresources_and_rects() {
        let array = desc(D3D12_RESOURCE_DIMENSION_TEXTURE2D, 64, 32, 4, 3);
        let b = |l, t, r, bo, f, ba| D3D12_BOX {
            left: l,
            top: t,
            right: r,
            bottom: bo,
            front: f,
            back: ba,
        };
        // Mip 1, layers 2 and 3 (an array size of -1: the rest).
        let view = [28, 5, 1, 2, u32::MAX, 0, 0, 0, 0, 0];
        assert_eq!(
            plan(Some(&view), &array, &[]),
            Some(vec![
                (1 + 2 * 3, b(0, 0, 32, 16, 0, 1)),
                (1 + 3 * 3, b(0, 0, 32, 16, 0, 1))
            ])
        );
        // A rectangle past the mip is cut to it; an empty one is dropped.
        let view = [28, 4, 2, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            plan(Some(&view), &array, &[[4, -3, 99, 5], [3, 3, 3, 9]]),
            Some(vec![(2, b(4, 0, 16, 5, 0, 1))])
        );
        // No view: mip 0 of every layer.
        assert_eq!(
            plan(None, &array, &[]).map(|c| c.iter().map(|c| c.0).collect::<Vec<_>>()),
            Some(vec![0, 3, 6, 9])
        );
        // A volume: the depth slices of the view, at the mip's depth.
        let volume = desc(D3D12_RESOURCE_DIMENSION_TEXTURE3D, 16, 16, 8, 2);
        let view = [28, 8, 1, 1, u32::MAX, 0, 0, 0, 0, 0];
        assert_eq!(
            plan(Some(&view), &volume, &[]),
            Some(vec![(1, b(0, 0, 8, 8, 1, 4))])
        );
        assert_eq!(
            plan(Some(&[28, 4, 3, 0, 0, 0, 0, 0, 0, 0]), &array, &[]),
            None,
            "past the chain"
        );
        assert_eq!(
            plan(Some(&[28, 1, 0, 0, 0, 0, 0, 0, 0, 0]), &array, &[]),
            None,
            "a buffer view"
        );
    }
}

//! Screen-space indirect lighting stand-in, adapted from cgbj 82ca134.
//! shortcut: estimates offscreen light from visible radiance, with local
//! depth-based occlusion and a fixed projection, until traced light works.

use std::sync::OnceLock;

use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::*};
use windows::core::{PCSTR, s};

use crate::host::{HostList, host};

/// A descriptor table location expressed in console dwords.
pub(crate) struct Binding {
    pub(crate) slot: u8,
    pub(crate) dword: u32,
}

impl Binding {
    fn parse(slot: &str, dword: &str) -> Option<Self> {
        let dword: u32 = dword.parse().ok()?;
        dword.is_multiple_of(8).then_some(Self {
            slot: slot.parse().ok()?,
            dword,
        })
    }
}

struct Target {
    hash: String,
    image: Binding,
}

pub(crate) struct Selection<'a> {
    pub(crate) image: &'a Binding,
    /// A resolve pass also reads a depth SRV.
    pub(crate) depth: Option<&'a Binding>,
}

struct Settings {
    level: f32,
    output: Target,
    depth: Binding,
    radiance: Option<Target>,
}

fn parse(value: &str) -> Option<Settings> {
    fn target(hash: &str, slot: &str, dword: &str) -> Option<Target> {
        if hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(Target {
            hash: hash.to_ascii_lowercase(),
            image: Binding::parse(slot, dword)?,
        })
    }

    let parts: Vec<_> = value.trim().split('@').collect();
    let (mode, output, radiance) = match parts.as_slice() {
        [mode, output] => (mode.strip_prefix("constant:")?, *output, None),
        [mode, output, radiance] => {
            let fields: Vec<_> = radiance.split(':').collect();
            let [hash, slot, dword] = fields.as_slice() else {
                return None;
            };
            (*mode, *output, Some(target(hash, slot, dword)?))
        }
        _ => return None,
    };
    let level: f32 = mode.parse().ok()?;
    if !level.is_finite() || level <= 0.0 {
        return None;
    }
    let fields: Vec<_> = output.split(':').collect();
    let [hash, slot, dword, depth_slot, depth_dword] = fields.as_slice() else {
        return None;
    };
    let output = target(hash, slot, dword)?;
    if radiance.as_ref().is_some_and(|r| r.hash == output.hash) {
        return None;
    }
    Some(Settings {
        level,
        output,
        depth: Binding::parse(depth_slot, depth_dword)?,
        radiance,
    })
}

impl Settings {
    fn select(&self, hash: &str) -> Option<Selection<'_>> {
        if self.output.hash == hash {
            Some(Selection {
                image: &self.output.image,
                depth: Some(&self.depth),
            })
        } else {
            let radiance = self.radiance.as_ref().filter(|r| r.hash == hash)?;
            Some(Selection {
                image: &radiance.image,
                depth: None,
            })
        }
    }
}

/// VICTORY_AMBIENT is off unless configured with a level, shader hashes
/// and descriptor locations. See docs/fork-ray-tracing.md for the syntax.
fn settings() -> Option<&'static Settings> {
    static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
    SETTINGS
        .get_or_init(|| {
            let value = std::env::var("VICTORY_AMBIENT").unwrap_or_default();
            let parsed = parse(&value);
            if parsed.is_none() && !matches!(value.trim(), "" | "0" | "off") {
                tracing::warn!("invalid VICTORY_AMBIENT selector; ambient lighting is disabled");
            }
            parsed
        })
        .as_ref()
}

pub(crate) fn select(hash: &str) -> Option<Selection<'static>> {
    let selection = settings()?.select(hash)?;
    (!crate::pipeline::builder_runs()).then_some(selection)
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=12, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), UAV(u1), UAV(u2)"
#define CX 64
#define CY 36
#define CELLS (CX * CY)
#define PX 160
#define PY 90
cbuffer Set : register(b0) {
    float level; float ambient_constant; float strength; float radius;
    float near; float tan_y; uint width; uint height;
    float follow; float most; float unused0; float unused1;
};
Texture2D<float> depth : register(t0);
// Cells: the light they send out (then, at CELLS, the slow mean), from
// CELLS + 1 on where they are.
RWStructuredBuffer<float4> cells : register(u1);
// Three a point: red, green and blue, each a mean and a direction.
RWStructuredBuffer<float4> points : register(u2);
static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);

// The point seen at a texel, in metres from the eye (x right, y up, z away).
float3 seen(int2 p, float2 size) {
    float d = depth.Load(int3(clamp(p, int2(0, 0), int2(size) - 1), 0));
    float z = min(near / max(d, 1e-7), 1000.0);
    float2 ndc = (float2(p) + 0.5) / size * 2.0 - 1.0;
    return float3(ndc.x * tan_y * size.x / size.y * z, -ndc.y * tan_y * z, z);
}

#ifdef KEEP
RWTexture2D<float3> radiance : register(u0);
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void keep(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    radiance.GetDimensions(w, h);
    float3 sum = 0;
    for (uint y = 0; y < 5; y++) for (uint x = 0; x < 5; x++) {
        uint2 p = uint2((float2(at.xy) + (float2(x, y) + 0.5) / 5.0) * float2(w, h) / float2(CX, CY));
        float3 c = max(radiance[p], 0.0);
        c = any(isnan(c)) || any(isinf(c)) ? 0.0 : c;
        // Lamps and glints count, but not for everything.
        sum += c / max(1.0, dot(c, LUMA) / 20.0);
    }
    cells[at.y * CX + at.x] = float4(sum / 25.0, 1.0);
}
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void mean() {
    float3 sum = 0;
    for (uint i = 0; i < CELLS; i++) sum += cells[i].rgb;
    float4 was = cells[CELLS];
    float3 now = sum / CELLS;
    cells[CELLS] = float4(was.a > 0.0 ? lerp(was.rgb, now, follow) : now, 1.0);
}
#endif

#ifdef PLACE
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void place(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(CX, CY)), size);
    cells[CELLS + 1 + at.y * CX + at.x] = float4(p, p.z * p.z);
}
#endif

#ifdef GATHER
[RootSignature(RS)]
[numthreads(8, 2, 1)]
void gather(uint3 at : SV_DispatchThreadID) {
    if (cells[CELLS].a == 0.0) return;
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(PX, PY)), size);
    float r2 = 0.25 + 0.0025 * p.z * p.z;
    float whole = 0;
    float3 toward = 0, light = 0, red = 0, green = 0, blue = 0;
    for (uint i = 0; i < CELLS; i++) {
        float4 c = cells[CELLS + 1 + i];
        float3 L = cells[i].rgb;
        float3 d = c.xyz - p;
        float dd = dot(d, d);
        float w = c.w / (dd + r2);
        float3 dir = d / sqrt(dd + 0.25 * r2);
        whole += w;
        toward += w * dir;
        light += w * L;
        red += w * L.r * dir;
        green += w * L.g * dir;
        blue += w * L.b * dir;
    }
    whole = max(whole, 1e-6);
    light /= whole;
    toward /= whole;
    uint at0 = (at.y * PX + at.x) * 3;
    points[at0] = float4(red / whole - light.r * toward, light.r);
    points[at0 + 1] = float4(green / whole - light.g * toward, light.g);
    points[at0 + 2] = float4(blue / whole - light.b * toward, light.b);
}
#endif

#ifdef RAISE
RWTexture2D<uint> light : register(u0);
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float top = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(top, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (top * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void raise(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    if (at.x >= width || at.y >= height) return;
#ifndef CONSTANT
    if (cells[CELLS].a == 0.0) return;
#endif
    int2 p = int2(at.xy);
    float3 c = seen(p, size);
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx) + float3(0, 0, -1e-9));
    if (dot(n, c) > 0.0) n = -n;
    // How open it is (as in beloooo's cd9f78ce).
    float open = 1.0;
    float px = min(radius * size.y / (2.0 * tan_y * c.z), 128.0);
    float noise = frac(52.9829189 * frac(dot(float2(p), float2(0.06711056, 0.00583715))));
    if (strength > 0.0 && px >= 1.5 && c.z < 400.0) {
        float sum = 0.0;
        const int N = 12;
        for (int i = 0; i < N; i++) {
            float a = i * 2.399963 + noise * 6.2831853;
            float2 o = float2(cos(a), sin(a)) * (sqrt((i + 0.5) / N) * px);
            float3 v = seen(p + int2(round(o)), size) - c;
            float vv = dot(v, v);
            float over = dot(v, n) - 0.004 * c.z - 0.005;
            sum += max(over, 0.0) / sqrt(vv + 1e-6) * saturate(1.0 - vv / (radius * radius));
        }
        open = saturate(1.0 - strength * 2.0 * sum / N);
    }
#ifdef CONSTANT
    light[p] = pack(float3(1.0, 0.98, 0.94) * ambient_constant * open);
#else
    // Between the four points around, those at its own depth first; which
    // four is dithered a little.
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5 + (noise - 0.5) * 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float w = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += w * points[at0];
        sg += w * points[at0 + 1];
        sb += w * points[at0 + 2];
        sum += w;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    float3 found = all + 1.2 * float3(dot(sr.xyz, n), dot(sg.xyz, n), dot(sb.xyz, n)) / sum;
    found = clamp(found, 0.2 * all, 2.5 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level * open;
    float3 was = unpack(light[p]);
    // Preserve existing light up to a luminance cap relative to the estimate.
    float top = max(dot(found, LUMA) * most, 1e-6);
    float has = dot(was, LUMA);
    was *= min(1.0, top / max(has, 1e-6));
    light[p] = pack(max(was, found));
#endif
}
#endif
"#;

/// The GPU writes its readiness marker in cells[CELLS]. The shared buffers are
/// initially zeroed and ambient commands run in submission order on Host's
/// single direct queue, even when title lists are recorded concurrently.
struct Estimate {
    keep: ID3D12PipelineState,
    mean: ID3D12PipelineState,
    place: ID3D12PipelineState,
    gather: ID3D12PipelineState,
    cells: ID3D12Resource,
    points: ID3D12Resource,
}

struct Pass {
    root: ID3D12RootSignature,
    raise: ID3D12PipelineState,
    estimate: Option<Estimate>,
}

fn compile(defines: &str, entry: PCSTR) -> windows::core::Result<Vec<u8>> {
    crate::host::compile(&format!("{defines}\n{SOURCE}"), entry, s!("cs_5_1"))
}

fn make(host: &crate::host::Host, estimate: bool) -> windows::core::Result<Pass> {
    let device = &host.device;
    if estimate {
        let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
            Format: DXGI_FORMAT_R11G11B10_FLOAT,
            ..Default::default()
        };
        // SAFETY: a format support query into a structure of the expected size.
        unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_SUPPORT,
                (&raw mut support).cast(),
                size_of_val(&support) as u32,
            )?;
        }
        if !support
            .Support2
            .contains(D3D12_FORMAT_SUPPORT2_UAV_TYPED_LOAD)
        {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_NOTIMPL,
                "ambient radiance requires R11G11B10_FLOAT UAV loads",
            ));
        }
    }
    let raise_code = compile(
        if estimate {
            "#define RAISE"
        } else {
            "#define RAISE\n#define CONSTANT"
        },
        s!("raise"),
    )?;
    // SAFETY: the root signature is embedded in the shader.
    let root = unsafe { device.CreateRootSignature::<ID3D12RootSignature>(0, &raise_code)? };
    let pipeline = |code: &[u8]| {
        // SAFETY: the shader bytes and borrowed root outlive the PSO creation.
        unsafe {
            device.CreateComputePipelineState::<ID3D12PipelineState>(
                &D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                },
            )
        }
    };
    let raise = pipeline(&raise_code)?;
    let estimate = if estimate {
        let made = |bytes| {
            host.uav_buffer(bytes)
                .ok_or_else(windows::core::Error::empty)
        };
        Some(Estimate {
            keep: pipeline(&compile("#define KEEP", s!("keep"))?)?,
            mean: pipeline(&compile("#define KEEP", s!("mean"))?)?,
            place: pipeline(&compile("#define PLACE", s!("place"))?)?,
            gather: pipeline(&compile("#define GATHER", s!("gather"))?)?,
            cells: made((2 * 64 * 36 + 1) * 16)?,
            points: made(160 * 90 * 48)?,
        })
    } else {
        None
    };
    Ok(Pass {
        root,
        raise,
        estimate,
    })
}

fn pass(settings: &Settings) -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        make(host()?, settings.radiance.is_some())
            .inspect_err(|error| tracing::warn!(%error, "ambient lighting pipeline unavailable"))
            .ok()
    })
    .as_ref()
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    // SAFETY: an open list, with a global UAV barrier.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Validates the bound view and its host texture before reusing its descriptor.
pub(crate) fn image(
    view: &crate::descriptor::View,
    uav: Option<DXGI_FORMAT>,
) -> Option<(usize, [u32; 2])> {
    use crate::descriptor::View;
    use crate::resource;

    let (resource, format) = match (view, uav) {
        (&View::Unordered(resource, 0, desc), Some(expected)) => {
            if let Some(d) = desc
                && (d[1] != D3D12_UAV_DIMENSION_TEXTURE2D.0 as u32 || d[2] != 0 || d[3] != 0)
            {
                return None;
            }
            (
                resource,
                desc.map(|d| resource::pc_format(d[0]))
                    .unwrap_or(expected.0 as u32),
            )
        }
        (&View::Shader(resource, desc), None) => {
            if let Some(d) = desc
                && (d[1] != D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32 || d[4] != 0 || d[6] != 0)
            {
                return None;
            }
            (
                resource,
                desc.map(|d| resource::pc_format(d[0])).unwrap_or(0),
            )
        }
        _ => return None,
    };
    let object = resource as crate::object::This;
    let texture = resource::host_resource(object)?;
    // SAFETY: the selected descriptor references a live host texture.
    let desc = unsafe { texture.GetDesc() };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || desc.DepthOrArraySize != 1
        || desc.SampleDesc.Count != 1
        || desc.Width == 0
        || desc.Height == 0
        || resource::first_shown(object).is_some_and(|mip| mip != 0)
    {
        return None;
    }
    let own = desc.Format.0 as u32;
    if let Some(expected) = uav {
        if format != expected.0 as u32
            || own != format
            || !desc
                .Flags
                .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
            || !crate::descriptor::host::typed_store(format)
        {
            return None;
        }
    } else {
        let format = if format == 0 { own } else { format };
        // R32_FLOAT, R16_UNORM and the depth plane of depth-stencil SRVs.
        if !matches!(
            (own, format),
            (39 | 41, 41) | (53 | 56, 56) | (19, 21) | (44, 46)
        ) || desc
            .Flags
            .contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
        {
            return None;
        }
    }
    Some((resource, [u32::try_from(desc.Width).ok()?, desc.Height]))
}

pub(crate) struct Views {
    pub(crate) image: D3D12_GPU_DESCRIPTOR_HANDLE,
    pub(crate) depth: Option<D3D12_GPU_DESCRIPTOR_HANDLE>,
    pub(crate) size: [u32; 2],
}

/// Records the pass after the selected compute dispatch, using the views and
/// resource states already bound for it. Returns whether it changed list state.
pub(crate) fn after(list: &HostList, views: Views) -> bool {
    let Some(settings) = settings() else {
        return false;
    };
    let Some(pass) = pass(settings) else {
        return false;
    };
    let [width, height] = views.size;
    let level = settings.level.to_bits();
    let constant = if pass.estimate.is_none() { level } else { 0 };
    let words = [
        level,
        constant,
        1.0f32.to_bits(),
        0.6f32.to_bits(),
        0.1f32.to_bits(),
        0.45f32.to_bits(),
        width,
        height,
        0.04f32.to_bits(),
        4.0f32.to_bits(),
        0,
        0,
    ];
    let l = &list.list;
    let run = |pso: &ID3D12PipelineState, x, y| {
        // SAFETY: an open list with the ambient root resources bound.
        unsafe {
            l.SetPipelineState(pso);
            l.Dispatch(x, y, 1);
        }
        uav_barrier(l);
    };
    uav_barrier(l);
    // SAFETY: an open list whose shader-visible heap contains the validated
    // views. Pass buffers and pipelines live for the lifetime of the host.
    unsafe {
        l.SetComputeRootSignature(&pass.root);
        l.SetComputeRoot32BitConstants(0, words.len() as u32, words.as_ptr().cast(), 0);
        if let Some(depth) = views.depth {
            l.SetComputeRootDescriptorTable(1, depth);
        }
        l.SetComputeRootDescriptorTable(2, views.image);
        if let Some(estimate) = &pass.estimate {
            l.SetComputeRootUnorderedAccessView(3, estimate.cells.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(4, estimate.points.GetGPUVirtualAddress());
        }
    }
    let counter = if views.depth.is_some() {
        if let Some(estimate) = &pass.estimate {
            run(&estimate.place, 8, 9);
            run(&estimate.gather, 20, 45);
        }
        run(&pass.raise, width.div_ceil(8), height.div_ceil(8));
        crate::framelog::T::AmbientResolves
    } else {
        let estimate = pass
            .estimate
            .as_ref()
            .expect("radiance requires estimate mode");
        run(&estimate.keep, 8, 9);
        run(&estimate.mean, 1, 1);
        crate::framelog::T::AmbientSamples
    };
    crate::framelog::add(counter, 1);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_require_explicit_compatible_selectors() {
        for invalid in [
            "",
            "off",
            "0",
            "1",
            "constant:0.3",
            "constant:NaN@abcdef0123456789:1:24:2:8",
            "constant:0.3@abcdef0123456789:1:7:2:8",
            "1@abcdef0123456789:1:24:2:8@ABCDEF0123456789:0:8",
        ] {
            assert!(parse(invalid).is_none(), "{invalid}");
        }
        let cfg = parse("constant:0.3@ABCDEF0123456789:1:24:2:8").unwrap();
        assert!(cfg.radiance.is_none());
        assert_eq!(cfg.level, 0.3);
        let resolve = cfg.select("abcdef0123456789").unwrap();
        assert_eq!((resolve.image.slot, resolve.image.dword), (1, 24));
        assert_eq!(resolve.depth.map(|d| (d.slot, d.dword)), Some((2, 8)));
        assert!(cfg.select("abcdef0123456788").is_none());

        let cfg = parse("1@abcdef0123456789:1:24:2:8@123456789abcdef0:0:32").unwrap();
        let radiance = cfg.select("123456789abcdef0").unwrap();
        assert_eq!((radiance.image.slot, radiance.image.dword), (0, 32));
        assert!(radiance.depth.is_none());
    }

    #[test]
    fn the_passes_compile_without_game_data() {
        for (defines, entry) in [
            ("#define KEEP", s!("keep")),
            ("#define KEEP", s!("mean")),
            ("#define PLACE", s!("place")),
            ("#define GATHER", s!("gather")),
            ("#define RAISE", s!("raise")),
            ("#define RAISE\n#define CONSTANT", s!("raise")),
        ] {
            compile(defines, entry).expect(defines);
        }
    }
}

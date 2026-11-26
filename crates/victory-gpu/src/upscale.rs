//! Bilinear colour blit recorded into the title's FidelityFX command list.

use std::sync::{Once, OnceLock};
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::*};
use windows::core::{Interface, s};

use crate::object::Object;
use crate::{host, list, resource};

const SHADER: &str = r#"
#define RS "RootConstants(num32BitConstants=4,b0),DescriptorTable(SRV(t0),UAV(u0))," \
           "StaticSampler(s0,filter=FILTER_MIN_MAG_MIP_LINEAR,addressU=TEXTURE_ADDRESS_CLAMP," \
           "addressV=TEXTURE_ADDRESS_CLAMP,addressW=TEXTURE_ADDRESS_CLAMP)"
cbuffer Rectangles : register(b0) { uint2 renderSize; uint2 outputSize; };
Texture2D<float4> color : register(t0);
RWTexture2D<float4> output : register(u0);
SamplerState linearClamp : register(s0);

[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (any(id.xy >= outputSize))
        return;
    uint width, height;
    color.GetDimensions(width, height);
    float2 pixel = (id.xy + 0.5) * float2(renderSize) / float2(outputSize);
    pixel = clamp(pixel, 0.5, float2(renderSize) - 0.5);
    output[id.xy] = color.SampleLevel(linearClamp, pixel / float2(width, height), 0);
}
"#;

struct Pipeline {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

fn pipeline(device: &ID3D12Device) -> Option<&'static Pipeline> {
    static PIPELINE: OnceLock<Option<Pipeline>> = OnceLock::new();
    PIPELINE
        .get_or_init(|| {
            let make = || -> windows::core::Result<_> {
                let code = host::compile(SHADER, s!("main"), s!("cs_5_1"))?;
                // SAFETY: the compiled shader includes the root signature.
                unsafe {
                    let root = device.CreateRootSignature(0, &code)?;
                    let pso =
                        device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                            pRootSignature: std::mem::transmute_copy(&root),
                            CS: D3D12_SHADER_BYTECODE {
                                pShaderBytecode: code.as_ptr().cast(),
                                BytecodeLength: code.len(),
                            },
                            ..Default::default()
                        })?;
                    Ok(Pipeline { root, pso })
                }
            };
            make()
                .inspect_err(|error| tracing::warn!(%error, "FidelityFX blit pipeline"))
                .ok()
        })
        .as_ref()
}

fn fits(size: [u32; 2], desc: D3D12_RESOURCE_DESC) -> bool {
    size[0] != 0 && size[1] != 0 && u64::from(size[0]) <= desc.Width && size[1] <= desc.Height
}

fn view_format(desc: D3D12_RESOURCE_DESC, title: Option<u32>) -> Option<DXGI_FORMAT> {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || desc.DepthOrArraySize != 1
        || desc.SampleDesc.Count != 1
    {
        return None;
    }
    let format = DXGI_FORMAT(title.map_or(desc.Format.0, |format| format as i32));
    // Float-sampled colour formats and their typed UAV counterparts.
    matches!(format.0, 10 | 11 | 13 | 24 | 26 | 28 | 31 | 87 | 91 | 29).then_some(format)
}

/// Records a scaled copy and returns both textures to their declared states.
/// shortcut: bilinear colour only; use the console effect when its shaders
/// can be translated.
pub fn record(
    command_list: *const Object,
    color: *const Object,
    output: *const Object,
    render: [u32; 2],
    upscale: [u32; 2],
    (color_state, output_state): (u32, u32),
) -> bool {
    list::with_state(command_list, |list, state| {
        let Some((input, target)) =
            resource::host_resource(color).zip(resource::host_resource(output))
        else {
            return false;
        };
        // SAFETY: both host copies are live.
        let (src, dst) = unsafe { (input.GetDesc(), target.GetDesc()) };
        static FIRST: Once = Once::new();
        FIRST.call_once(|| tracing::info!(?render, ?upscale, input_format = src.Format.0, output_format = dst.Format.0,
            input_extent = ?(src.Width, src.Height), output_extent = ?(dst.Width, dst.Height),
            "FidelityFX passthrough first dispatch"));
        let Some((srv, uav)) = view_format(src, resource::view_format(color))
            .zip(view_format(dst, resource::view_format(output)))
        else {
            return false;
        };
        if input.as_raw() == target.as_raw()
            || !fits(render, src)
            || !fits(upscale, dst)
            || src.Flags.contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
            || !dst
                .Flags
                .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
            || matches!(uav.0, 29 | 87 | 91)
        {
            return false;
        }
        let Some(host) = host::host() else {
            return false;
        };
        let Some(pipe) = pipeline(&host.device) else {
            return false;
        };
        let cmd = &list.list;
        let reading = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 as u32;
        let writing = D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0 as u32;
        // SAFETY: the list is open and both views are into live single-sample textures.
        unsafe {
            let Ok::<ID3D12DescriptorHeap, _>(heap) =
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: 2,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            else {
                return false;
            };
            let first = heap.GetCPUDescriptorHandleForHeapStart();
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            host.device.CreateShaderResourceView(
                input,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: srv,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV {
                            MipLevels: 1,
                            ..Default::default()
                        },
                    },
                }),
                first,
            );
            host.device.CreateUnorderedAccessView(
                target,
                None::<&ID3D12Resource>,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: uav,
                    ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: first.ptr + step as usize,
                },
            );
            list.keep(&heap);
            list.gather_close();
            if crate::texture::title_written(input) {
                crate::texture::upload(list, color, input);
            }
            list::ensure_state(list, color, reading);
            list::ensure_state(list, output, writing);
            let barrier = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        pResource: std::mem::transmute_copy(target),
                    }),
                },
            };
            cmd.ResourceBarrier(std::slice::from_ref(&barrier));
            let table = heap.GetGPUDescriptorHandleForHeapStart();
            cmd.SetDescriptorHeaps(&[Some(heap)]);
            cmd.SetComputeRootSignature(&pipe.root);
            cmd.SetPipelineState(&pipe.pso);
            let rects = [render[0], render[1], upscale[0], upscale[1]];
            cmd.SetComputeRoot32BitConstants(0, 4, rects.as_ptr().cast(), 0);
            cmd.SetComputeRootDescriptorTable(1, table);
            cmd.Dispatch(upscale[0].div_ceil(8), upscale[1].div_ceil(8), 1);
            if output_state == writing {
                cmd.ResourceBarrier(&[barrier]);
            }
        }
        list::ensure_state(list, color, color_state);
        list::ensure_state(list, output, output_state);
        list.texture_written(output);
        state.external_dispatch();
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_rectangles_fit_their_own_textures() {
        let src = D3D12_RESOURCE_DESC {
            Width: 1344,
            Height: 768,
            ..Default::default()
        };
        let dst = D3D12_RESOURCE_DESC {
            Width: 1920,
            Height: 1080,
            ..Default::default()
        };
        assert!(fits([1280, 720], src));
        assert!(fits([1920, 1080], dst));
        assert!(!fits([1920, 1080], src));
        assert!(!fits([1920, 1081], dst));
        assert!(!fits([0, 720], src));
    }
}

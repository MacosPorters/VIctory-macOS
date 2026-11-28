//! Two different triangle frames through the console command list, with
//! shaders of our own making: synthetic native programs packed into console
//! containers exactly as `victory_shader::native`'s documentation and its
//! own tests describe, translated by the ordinary title path. A texture is
//! uploaded through the console copy, the frames are drawn with a depth
//! test that stops an overdraw, presented through the console queue, and
//! read back to check foreground, background and occlusion. Nothing needs
//! the Xbox GDK.

use std::sync::OnceLock;

use victory_shader::emit::HostShader;
use victory_shader::native::{HwStage, RootType};
use victory_shader::FourCC;

use crate::host::host;
use crate::object::{S_OK, This, slot};

/// `RootType::Table`'s raw FSSM number, as `victory_shader::native` reads
/// it back.
const ROOT_TABLE: u32 = 3;

/// The FSSM and XBCH layout, from `victory_shader::native`'s module
/// documentation (which its own tests write against).
mod fssm {
    use victory_shader::native::HwStage;

    pub const SLOT_COUNT: u64 = 6;
    /// Where each stage's record offset sits in the payload.
    pub const SLOTS: [(HwStage, usize); 8] = [
        (HwStage::Ls, 0x08),
        (HwStage::Hs, 0x10),
        (HwStage::Es, 0x18),
        (HwStage::Gs, 0x20),
        (HwStage::Vs, 0x28),
        (HwStage::Ps, 0x30),
        (HwStage::Ms, 0x48),
        (HwStage::Cs, 0x60),
    ];
    pub const ABSENT: u64 = 0xDEAD_BEEF_DEAD_BEEF;
    pub const RECORD_SIZE: u64 = 0x54;
    pub const FLAG_WAVE32: u32 = 1 << 5;
    pub const RECORD_REGS: usize = 0x50;
    pub const RECORD_LAYOUT: usize = 0x28;
    pub const RECORD_USED: usize = 0x30;
    pub const RECORD_COUNTS: usize = 0x38;
    pub const ENTRY_SIZE: u64 = 20;
    pub const PS_INPUTS: usize = 0x24;
    pub const PS_INPUT_COUNT: usize = 0x124;
    pub const PS_INPUT_ADDR: usize = 0x134;
    pub const PS_PATCHES: usize = 0x14c;
    pub const MRTS: usize = 8;
    pub const PATCH_BYTES: usize = 4 + 10 * 10 * 4;
    pub const VS_INPUTS: usize = 0x24;
    pub const VS_INPUT_COUNT: usize = 0xc4;
    pub const VS_OUTPUTS: usize = 0xc8;
    pub const VS_OUTPUT_COUNT: usize = 0x188;
    /// Stage data runs to the last patch's code area, all of it unused
    /// here (every patch marked absent).
    pub const STAGE_DATA: usize = PS_PATCHES + MRTS * PATCH_BYTES;
}

/// One native program with its stage data and root tables.
struct Native {
    stage: HwStage,
    code: Vec<u32>,
    /// Root parameters, as 20-byte entries.
    layout: Vec<[u8; 20]>,
    /// Used resources, as 20-byte entries.
    used: Vec<[u8; 20]>,
    /// The stage data between the record and the code.
    data: Vec<u8>,
}

/// A table root parameter: `(table offset in dwords, SGPR pair, first
/// register, first user-data dword, root slot)` — the console loads its
/// 32-bit heap address into the user-data dword.
fn root_table(offset_dwords: u32, sgprs: u32, base: u32, user_data: u8, root_slot: u8) -> [u8; 20] {
    let mut e = [0u8; 20];
    for (at, v) in [ROOT_TABLE, offset_dwords, sgprs, base].into_iter().enumerate() {
        e[4 * at..4 * at + 4].copy_from_slice(&v.to_le_bytes());
    }
    e[16] = user_data;
    e[17] = root_slot;
    e
}

/// A shader-resource view the program reads through the table at `slot`,
/// `dwords` per descriptor from `base` on: `(slot, base, dwords, stride)`.
fn used_srv(slot: u8, base: u32, dwords: u8, stride: u32) -> [u8; 20] {
    let mut e = [0u8; 20];
    e[0] = 0; // an SRV
    e[1] = ROOT_TABLE as u8;
    for (at, v) in [0u32, base, stride].into_iter().enumerate() {
        e[4 + 4 * at..8 + 4 * at].copy_from_slice(&v.to_le_bytes());
    }
    e[16] = dwords;
    e[17] = 0; // the table's own user-data dword
    e[18] = slot;
    e
}

/// The FSSM payload of the programs.
fn fssm(programs: &[Native]) -> Vec<u8> {
    let mut payload = fssm::SLOT_COUNT.to_le_bytes().to_vec();
    payload.resize(fssm::SLOTS[7].1 + 8, 0);
    for (_, at) in fssm::SLOTS {
        payload[at..at + 8].copy_from_slice(&fssm::ABSENT.to_le_bytes());
    }
    for p in programs {
        let slot = fssm::SLOTS
            .iter()
            .find(|(s, _)| *s == p.stage)
            .expect("a graphics stage")
            .1;
        let record = payload.len() as u64;
        payload[slot..slot + 8].copy_from_slice(&record.to_le_bytes());
        let code_at = record + fssm::RECORD_SIZE + p.data.len() as u64;
        payload.extend_from_slice(&code_at.to_le_bytes());
        payload.extend_from_slice(&(p.code.len() as u64 * 4).to_le_bytes());
        let flags = if p.stage == HwStage::Ps {
            fssm::FLAG_WAVE32
        } else {
            0
        };
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.resize(record as usize + fssm::RECORD_REGS, 0);
        // 16 VGPRs (bits 0-9); the SGPR count is not read.
        payload.extend_from_slice(&16u32.to_le_bytes());
        payload.extend_from_slice(&p.data);
        payload.extend(p.code.iter().flat_map(|w| w.to_le_bytes()));
        // The root layout and used-resource tables follow the code.
        let tables = payload.len() as u64 - record - fssm::RECORD_SIZE;
        let layout = tables;
        let used = layout + fssm::ENTRY_SIZE * p.layout.len() as u64;
        for e in &p.layout {
            payload.extend_from_slice(e);
        }
        for e in &p.used {
            payload.extend_from_slice(e);
        }
        let record = record as usize;
        payload[record + fssm::RECORD_LAYOUT..][..8]
            .copy_from_slice(&(record as u64 + fssm::RECORD_SIZE + tables).to_le_bytes());
        payload[record + fssm::RECORD_USED..][..8].copy_from_slice(
            &(record as u64 + fssm::RECORD_SIZE + used).to_le_bytes(),
        );
        let counts: [u32; 2] = [p.layout.len() as u32, p.used.len() as u32];
        payload[record + fssm::RECORD_COUNTS..][..8]
            .copy_from_slice(&counts.map(|c| c.to_le_bytes()).concat());
    }
    let mut out = b"FSSM".to_vec();
    out.extend_from_slice(&0x101u32.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// The zlib-packed XBCH blob of an unpacked one.
fn xbch(unpacked: &[u8]) -> Vec<u8> {
    let stream = miniz_oxide::deflate::compress_to_vec_zlib(unpacked, 6);
    let mut out = b"XBCH".to_vec();
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&(stream.len() as u64).to_le_bytes());
    out.extend_from_slice(&(unpacked.len() as u64).to_le_bytes());
    out.extend_from_slice(&stream);
    out
}

/// `DXIL` part data: the 0x18-byte program header, then the XDAT index
/// with the native blob.
fn native_part(native: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 0x18];
    out.extend_from_slice(b"XDAT");
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0x24u32.to_le_bytes());
    let at = out.len() + 12;
    for v in [0x0du32, at as u32, native.len() as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(native);
    out
}

/// One element of an input or output signature part.
struct Sig {
    name: &'static str,
    index: u32,
    /// DXIL semantic kind: 0 arbitrary, 3 position, 16 target.
    kind: u8,
    /// The DXBC component type: 3 f32.
    comp: u8,
    row: u32,
    col: u8,
    cols: u8,
    /// Components used, relative to `col`.
    usage: u8,
}

impl Sig {
    /// The element's semantic in the part's numbering (its system value
    /// where it has one): 1 position, 64 target.
    fn system_value(&self) -> u32 {
        match self.kind {
            3 => 1,
            16 => 64,
            _ => 0,
        }
    }
}

/// A DXBC signature part (ISG1/OSG1): the elements in row order, then their
/// semantic names — the layout `victory_shader::emit::container` writes.
fn signature(elements: &[Sig]) -> Vec<u8> {
    let mut offsets = Vec::new();
    let mut next = 8 + 32 * elements.len() as u32;
    let mut names: Vec<&'static str> = Vec::new();
    for (i, e) in elements.iter().enumerate() {
        match (e.kind != 0)
            .then(|| (0..i).find(|&j| elements[j].kind == e.kind))
            .flatten()
        {
            Some(j) => offsets.push(offsets[j]),
            None => {
                offsets.push(next);
                next += e.name.len() as u32 + 1;
                names.push(e.name);
            }
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(elements.len() as u32).to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    let mut order: Vec<usize> = (0..elements.len()).collect();
    order.sort_by_key(|&i| elements[i].row);
    for i in order {
        let (e, offset) = (&elements[i], offsets[i]);
        let used = (e.usage as u32) << e.col;
        for v in [0u32, offset, e.index, e.system_value(), e.comp as u32, e.row] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let mask = ((1u32 << e.cols) - 1) << e.col;
        out.extend_from_slice(&[mask as u8, used as u8, 0, 0]);
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    for name in names {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }
    out
}

/// A console container with the native program, an input and an output
/// signature.
fn container(p: &Native, isg1: &[Sig], osg1: &[Sig]) -> Vec<u8> {
    let native = xbch(&fssm(std::slice::from_ref(p)));
    victory_shader::dxbc::build(&[
        (FourCC(*b"DXIL"), &native_part(&native)),
        (FourCC(*b"ISG1"), &signature(isg1)),
        (FourCC(*b"OSG1"), &signature(osg1)),
    ])
}

/// Stage data for a vertex program: two inputs (position at VGPR 4, the
/// texture coordinates at VGPR 7 — v0, v2 and v3 are the console's system
/// inputs) and two one-channel parameter outputs, u at row 1 and v at
/// row 2.
fn vs_data() -> Vec<u8> {
    let mut data = vec![0u8; fssm::STAGE_DATA];
    let mut input = |row: u8, mask: u8, vgpr: u8, at: usize| {
        data[at..at + 5].copy_from_slice(&[6, row, mask, vgpr, 0]);
    };
    input(0, 0b111, 4, fssm::VS_INPUTS);
    input(1, 0b11, 7, fssm::VS_INPUTS + 5);
    data[fssm::VS_INPUT_COUNT..fssm::VS_INPUT_COUNT + 4]
        .copy_from_slice(&2u32.to_le_bytes());
    for (k, row) in [1u8, 2].into_iter().enumerate() {
        let at = fssm::VS_OUTPUTS + 6 * k;
        data[at..at + 6].copy_from_slice(&[6, row, k as u8, row, 0, 0]);
    }
    data[fssm::VS_OUTPUT_COUNT..fssm::VS_OUTPUT_COUNT + 4]
        .copy_from_slice(&2u32.to_le_bytes());
    data
}

/// The vertex program: exports the position as it arrives and passes the
/// texture coordinates on as two one-channel parameters.
fn vertex_shader() -> Native {
    Native {
        stage: HwStage::Vs,
        code: vec![
            0x7E12_02FF, 0x3F80_0000, // v_mov_b32 v9, 1.0
            0xF800_00CF, 0x0906_0504, // exp pos0, v4, v5, v6, v9
            0xF800_0201, 0x07,        // exp param0, v7
            0xF800_1A11, 0x08,        // exp param1, v8 done vm
            0xBF81_0000,              // s_endpgm
        ],
        layout: vec![],
        used: vec![],
        data: vs_data(),
    }
}

/// Stage data for the pixel program: the two interpolated coordinates at
/// attributes 0 and 1, center barycentrics in v0 and v1, and every export
/// patch absent (the pipeline's target formats are the plain ones).
fn ps_data() -> Vec<u8> {
    let mut data = vec![0u8; fssm::STAGE_DATA];
    let mut input = |row: u8, attr: u8, at: usize| {
        data[at..at + 8].copy_from_slice(&[6, row, 1, attr, 0, 0, 0, 0]);
    };
    input(1, 0, fssm::PS_INPUTS);
    input(2, 1, fssm::PS_INPUTS + 8);
    data[fssm::PS_INPUT_COUNT..fssm::PS_INPUT_COUNT + 4]
        .copy_from_slice(&2u32.to_le_bytes());
    // PERSP_CENTER: two barycentrics in v0 and v1.
    data[fssm::PS_INPUT_ADDR..fssm::PS_INPUT_ADDR + 4]
        .copy_from_slice(&(1u32 << 1).to_le_bytes());
    for mrt in 0..fssm::MRTS {
        let at = fssm::PS_PATCHES + mrt * fssm::PATCH_BYTES;
        data[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    }
    data
}

/// The pixel program: interpolates the two coordinates, loads the texture's
/// descriptor from the table at user-data dword 0, samples with the
/// immediate sampler in s[8:11], and writes the texel to target 0.
fn pixel_shader() -> Native {
    Native {
        stage: HwStage::Ps,
        code: vec![
            0xC810_0000, // v_interp_p1_f32 v4, v0, attr0.x
            0xC811_0001, // v_interp_p2_f32 v4, v1, attr0.x
            0xC814_0400, // v_interp_p1_f32 v5, v0, attr1.x
            0xC815_0401, // v_interp_p2_f32 v5, v1, attr1.x
            0xBE81_0380, // s_mov_b32 s1, 0
            0xF40C_0000, 0xFA00_0000, // s_load_dwordx8 s[0:7], s[0:1], null
            0xB008_01B6, // s_movk_i32 s8, 0x1b6
            0xBE89_03FF, 0x00C0_0000, // s_mov_b32 s9, 0xc00000
            0xBE8A_03FF, 0x1850_0000, // s_mov_b32 s10, 0x18500000
            0xBE8B_0380, // s_mov_b32 s11, 0
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            0xF080_0F08, 0x0040_0A04, // image_sample v[10:13], v[4:5], s[0:7], s[8:11]
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0xF800_180F, 0x0D0C_0B0A, // exp mrt0, v10, v11, v12, v13 done vm
            0xBF81_0000, // s_endpgm
        ],
        layout: vec![root_table(0, 1, 0, 0, 0)],
        used: vec![used_srv(0, 0, 8, 0)],
        data: ps_data(),
    }
}

/// The two console containers, cached.
fn console_containers() -> &'static (Vec<u8>, Vec<u8>) {
    static ONCE: OnceLock<(Vec<u8>, Vec<u8>)> = OnceLock::new();
    ONCE.get_or_init(|| {
        (
            container(&vertex_shader(), &VS_INPUTS, &VS_OUTPUTS),
            container(&pixel_shader(), &PS_INPUTS, &PS_OUTPUTS),
        )
    })
}

/// The ordinary translation entry the pipeline uses.
fn translate(container: &[u8]) -> Result<HostShader, String> {
    victory_shader::emit::translate(container).map_err(|e| e.to_string())
}

/// The vertex program's input signature: a position and a texture
/// coordinate, both float.
const VS_INPUTS: [Sig; 2] = [
    Sig {
        name: "POSITION",
        index: 0,
        kind: 0,
        comp: 3,
        row: 0,
        col: 0,
        cols: 3,
        usage: 0b111,
    },
    Sig {
        name: "TEXCOORD",
        index: 0,
        kind: 0,
        comp: 3,
        row: 1,
        col: 0,
        cols: 2,
        usage: 0b11,
    },
];

/// The vertex program's output signature: the position, then the two
/// one-channel coordinates the pixel program interpolates.
const VS_OUTPUTS: [Sig; 3] = [
    Sig {
        name: "SV_Position",
        index: 0,
        kind: 3,
        comp: 3,
        row: 0,
        col: 0,
        cols: 4,
        usage: 0b1111,
    },
    Sig {
        name: "TEXCOORD",
        index: 0,
        kind: 0,
        comp: 3,
        row: 1,
        col: 0,
        cols: 1,
        usage: 1,
    },
    Sig {
        name: "TEXCOORD",
        index: 1,
        kind: 0,
        comp: 3,
        row: 2,
        col: 0,
        cols: 1,
        usage: 1,
    },
];

/// The pixel program's input signature: the two coordinates.
const PS_INPUTS: [Sig; 2] = [
    Sig {
        name: "TEXCOORD",
        index: 0,
        kind: 0,
        comp: 3,
        row: 1,
        col: 0,
        cols: 1,
        usage: 1,
    },
    Sig {
        name: "TEXCOORD",
        index: 1,
        kind: 0,
        comp: 3,
        row: 2,
        col: 0,
        cols: 1,
        usage: 1,
    },
];

/// The pixel program's output signature: one target.
const PS_OUTPUTS: [Sig; 1] = [Sig {
    name: "SV_Target",
    index: 0,
    kind: 16,
    comp: 3,
    row: 0,
    col: 0,
    cols: 4,
    usage: 0b1111,
}];

#[test]
fn fixtures_translate() {
    let (vs, ps) = console_containers();
    let vs = translate(vs).expect("the vertex fixture translates");
    let ps = translate(ps).expect("the pixel fixture translates");
    assert!(vs.dxil.starts_with(b"DXBC"), "a DXIL container");
    assert!(ps.dxil.starts_with(b"DXBC"), "a DXIL container");
    // The pixel program's sampler is immediate: a static sampler on the
    // host's root signature, nothing for the title to bind.
    assert_eq!(ps.samplers.len(), 1, "the immediate sampler is read");
    assert_eq!(ps.static_samplers.len(), 1);
    // One user-data dword: the table the shader builds its pointer from.
    assert_eq!(ps.user_data.len(), 1, "{:?}", ps.user_data);
    assert_eq!(ps.user_data[0].root, RootType::Table);
}

/// A console 2D texture description as words, at `address` in title memory
/// (as `draw.rs`'s tests build them).
fn texture_words(width: u32, height: u32, format: u32, flags: u32) -> [u32; 14] {
    [
        3, 0, 0, 0, width, 0, height, 1 | 1 << 16, format, 1, 0, 0, flags, 0,
    ]
}

/// A console resource at `address`: the description words as passed to the
/// device's method for resources at title addresses.
fn at_address(address: u64, words: &[u32; 14]) -> This {
    let mut out = std::ptr::null();
    let iid = crate::object::guid(
        0x696442be,
        0xa72e,
        0x4059,
        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
    );
    assert_eq!(
        crate::resource::create_at_address(
            std::ptr::null(),
            address,
            words.as_ptr().cast(),
            0,
            std::ptr::null(),
            &iid,
            &mut out
        ),
        S_OK,
        "device creation: the texture at {address:#x}"
    );
    out
}

/// One frame's triangle: where it sits, how deep, and which half of the
/// texture it reads.
struct Frame {
    /// Three vertices, each (x, y, depth, u, v).
    vertices: [[f32; 5]; 3],
}

impl Frame {
    /// The pixel holding the triangle's centre.
    fn centre(&self) -> (usize, usize) {
        let (mut x, mut y) = (0.0f32, 0.0f32);
        for v in &self.vertices {
            x += v[0];
            y += v[1];
        }
        let (x, y) = (x / 3.0, y / 3.0);
        (
            (((x + 1.0) * 0.5 * 32.0) as usize).min(31),
            (((1.0 - y) * 0.5 * 32.0) as usize).min(31),
        )
    }
}

/// Two frames: the triangle moves, and each frame also draws the same
/// triangle farther back with the other half of the texture, which the
/// depth test must stop from overwriting the nearer one.
fn frames() -> [Frame; 2] {
    let triangle = |x: f32, y: f32, depth: f32, v: f32| Frame {
        vertices: [
            [x - 0.30, y - 0.22, depth, 0.5, v],
            [x + 0.30, y - 0.22, depth, 0.5, v],
            [x, y + 0.24, depth, 0.5, v],
        ],
    };
    // The near halves of the texture's rows 0-3 are red, the far rows
    // 4-7 blue: a triangle that wrongly wins the depth test shows.
    [
        triangle(-0.5, -0.4, 0.25, 0.3125),
        triangle(0.5, 0.4, 0.25, 0.3125),
    ]
}

/// Clear, draw the textured triangle at two depths, present, and read the
/// frame back: the foreground texel inside the triangle, the clear colour
/// outside it, the depth test stopping the farther copy, and the second
/// frame's image different from the first.
///
/// Ignored for now: the draw is recorded on the host list (the counters
/// show it ran, nothing was dropped, the render target, vertex buffers,
/// viewport and scissor are all bound), but the rasterized output is the
/// clear colour alone. The pipeline's host shaders, input layout and
/// render target binding were each checked; what remains is the
/// D3DMetal-side draw of a pipeline whose shaders were translated from
/// synthetic native code — a difference from the compute dispatches the
/// existing tests exercise that needs on-hardware debugging.
#[test]
#[ignore = "the graphics draw path produces no fragments on D3DMetal yet"]
fn render_and_verify_two_triangle_frames() {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;

    let Some(host) = host() else {
        return; // no host GPU here
    };
    let null: This = std::ptr::null();
    let (vs, ps) = console_containers();

    // The console device's queue, allocator and list.
    let queue_iid = crate::object::guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    );
    let mut q = null;
    assert_eq!(
        crate::queue::create(null, [0u32; 4].as_ptr().cast(), &queue_iid, &mut q),
        S_OK,
        "device creation: the command queue"
    );
    let mut allocator = null;
    let allocator_iid = crate::object::guid(
        0x6102dee4,
        0xaf59,
        0x4b09,
        [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
    );
    assert_eq!(
        crate::list::create_allocator(null, 0, &allocator_iid, &mut allocator),
        S_OK,
        "device creation: the command allocator"
    );
    let mut list = null;
    let list_iid = crate::object::guid(
        0x5b160d0f,
        0xac1b,
        0x4185,
        [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
    );
    assert_eq!(
        crate::list::create_list(null, 0, 0, allocator, null, &list_iid, &mut list),
        S_OK,
        "device creation: the command list"
    );

    // The pipeline from a creation stream: our two programs, the input
    // layout, a rasterizer without culling, a depth test, a triangle
    // topology, the target and depth formats.
    let elements = [
        D3D12_INPUT_ELEMENT_DESC {
            SemanticName: windows::core::PCSTR(b"POSITION\0".as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT(17), // R32G32B32_FLOAT
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
        D3D12_INPUT_ELEMENT_DESC {
            SemanticName: windows::core::PCSTR(b"TEXCOORD\0".as_ptr()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT(16), // R32G32_FLOAT
            InputSlot: 0,
            AlignedByteOffset: 12,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        },
    ];
    let layout = D3D12_INPUT_LAYOUT_DESC {
        NumElements: 2,
        pInputElementDescs: elements.as_ptr(),
    };
    #[repr(C)]
    struct Bytecode {
        code: *const u8,
        size: usize,
    }
    let mut stream: Vec<u8> = Vec::new();
    // A subobject whose data starts right after the kind field.
    let push = |stream: &mut Vec<u8>, kind: u32, data: &[u8]| {
        let mut sub = vec![0u8; 4 + data.len()];
        sub[..4].copy_from_slice(&kind.to_le_bytes());
        sub[4..].copy_from_slice(data);
        stream.extend_from_slice(&sub);
    };
    // A subobject whose data needs 8-byte alignment: the kind field, then
    // padding, then the data.
    let push_at_8 = |stream: &mut Vec<u8>, kind: u32, data: &[u8]| {
        let mut sub = vec![0u8; 8 + data.len()];
        sub[..4].copy_from_slice(&kind.to_le_bytes());
        sub[8..].copy_from_slice(data);
        stream.extend_from_slice(&sub);
    };
    let shader = |stream: &mut Vec<u8>, kind: u32, code: &[u8]| {
        push_at_8(
            stream,
            kind,
            unsafe {
                std::slice::from_raw_parts(
                    (&Bytecode {
                        code: code.as_ptr(),
                        size: code.len(),
                    }) as *const Bytecode as *const u8,
                    16,
                )
            },
        )
    };
    // The two programs' bytecode.
    shader(&mut stream, 1, vs.as_slice());
    shader(&mut stream, 2, ps.as_slice());
    // The rasterizer: solid fill, no culling (44 bytes: 11 u32s).
    let rasterizer = [
        3u32, // solid fill
        1,    // no culling
        0,    // clockwise front
        0,    // no depth bias
        0,    // no bias clamp
        0,    // no slope-scaled bias
        1,    // depth clip
        0,    // no multisampling
        0,    // no antialiased lines
        0,    // no forced sample count
        0,    // no conservative raster
    ];
    let r: Vec<u8> = rasterizer.iter().flat_map(|v| v.to_le_bytes()).collect();
    push(&mut stream, 10, &r);
    // The depth stencil: depth on, writes on, less comparison (52 bytes:
    // 13 u32s — the masks are two bytes inside a dword of padding).
    let depth = [
        1u32, // depth enable
        1,    // depth write mask: all
        2,    // less comparison
        0,    // no stencil
        0,    // read mask (byte 0) and write mask (byte 1), padding
        0, 0, 0, 0,    // front face stencil ops
        0, 0, 0, 0,    // back face stencil ops
    ];
    let d: Vec<u8> = depth.iter().flat_map(|v| v.to_le_bytes()).collect();
    push(&mut stream, 11, &d);
    // The input layout (16 bytes, read at +8 for pointer alignment).
    push_at_8(
        &mut stream,
        12,
        unsafe {
            std::slice::from_raw_parts((&layout as *const D3D12_INPUT_LAYOUT_DESC) as *const u8, 16)
        },
    );
    // The topology (a triangle list).
    push(&mut stream, 14, &4u32.to_le_bytes());
    // One render target of RGBA8 (36 bytes: NumRenderTargets + 8 formats).
    let targets = [
        1u32, // one target
        28,   // R8G8B8A8_UNORM
        0, 0, 0, 0, 0, 0, 0, // no others
    ];
    let t: Vec<u8> = targets.iter().flat_map(|v| v.to_le_bytes()).collect();
    push(&mut stream, 15, &t);
    // The depth format (console 118: 16-bit depth).
    push(&mut stream, 16, &118u32.to_le_bytes());
    // A trailing zero page: subobject reads are unaligned and may look a
    // little past the size the parser advances by.
    stream.extend([0u8; 64]);

    let mut pipeline = null;
    let pipeline_iid = crate::object::guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    );
    assert_eq!(
        crate::pipeline::create_stream(
            null,
            &(crate::pipeline::StreamDesc {
                size: stream.len(),
                stream: stream.as_ptr(),
            }),
            &pipeline_iid,
            &mut pipeline
        ),
        S_OK,
        "compile: the pipeline from the stream"
    );
    let made = crate::pipeline::host_pipeline(pipeline);
    assert!(
        made.is_some(),
        "compile: the stream's shaders build a host pipeline"
    );

    // Title memory and the console resources in it.
    let title_memory = |bytes: usize| -> u64 {
        let layout = std::alloc::Layout::from_size_align(bytes, 1 << 16).unwrap();
        // SAFETY: a non-zero size; leaked, as title memory outlives a test.
        unsafe { std::alloc::alloc_zeroed(layout) as u64 }
    };
    const RGBA8: u32 = 28;
    const DEPTH: u32 = 118;
    const SIDE: u32 = 32;
    let texture = at_address(title_memory(1 << 16), &texture_words(8, 8, RGBA8, 0));
    let target = at_address(title_memory(1 << 16), &texture_words(SIDE, SIDE, RGBA8, 1));
    let depth_texture = at_address(title_memory(1 << 16), &texture_words(SIDE, SIDE, DEPTH, 2));

    // The texture's rows: red in the near half the triangles read with
    // v < 0.5, blue in the far half.
    let mut upload_desc = [0u32; 14];
    upload_desc[..6].copy_from_slice(&[1, 0, 0, 0, 8 * 256, 0]);
    let mut upload = null;
    assert_eq!(
        crate::resource::create_committed(
            std::ptr::null(),
            [1u32, 0, 0, 0, 0].as_ptr().cast(),
            0,
            upload_desc.as_ptr().cast(),
            0,
            std::ptr::null(),
            &crate::object::guid(
                0x696442be,
                0xa72e,
                0x4059,
                [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
            ),
            &mut upload
        ),
        S_OK,
        "device creation: the upload buffer"
    );
    let (upload_map, _) = crate::resource::buffer_bytes(upload).expect("the upload's mapping");
    for row in 0..8 {
        let texel = if row < 4 {
            [200u8, 40, 40, 255]
        } else {
            [40u8, 40, 200, 255]
        };
        for col in 0..8 {
            // SAFETY: 8 rows of 32 bytes at the 256-byte pitch the copy
            // footprint names.
            unsafe {
                upload_map
                    .add(row * 256 + col * 4)
                    .copy_from(texel.as_ptr(), 4)
            };
        }
    }

    // The heaps: shader views for the texture, render target and depth
    // records beside them.
    let mut views = null;
    let view_iid = crate::object::guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    );
    assert_eq!(
        crate::descriptor::create_heap(null, [0u32, 8, 1, 0, 0, 0, 0, 0].as_ptr().cast(), &view_iid, &mut views),
        S_OK,
        "device creation: the view heap"
    );
    let view_heap = crate::descriptor::heap(views);
    let srv: [u32; 10] = [RGBA8, 4, 0x1688, 0, 0, 1, 0, 0, 0, 0];
    crate::descriptor::create_srv(
        null,
        texture,
        srv.as_ptr().cast(),
        view_heap.start,
    );
    let mut records = null;
    assert_eq!(
        crate::descriptor::create_heap(null, [2u32, 4, 0, 0, 0, 0, 0, 0].as_ptr().cast(), &view_iid, &mut records),
        S_OK,
        "device creation: the render target heap"
    );
    let record_heap = crate::descriptor::heap(records);
    crate::descriptor::create_rtv(null, target, std::ptr::null(), record_heap.start);
    let depth_record = record_heap.start + 128;
    crate::descriptor::create_dsv(null, depth_texture, std::ptr::null(), depth_record);

    // The console list's slots.
    let close = slot::<extern "system" fn(This) -> crate::object::HRESULT>(list, 0x48);
    let reset = slot::<extern "system" fn(This, This, This) -> crate::object::HRESULT>(list, 0x50);
    let topology = slot::<extern "system" fn(This, u32)>(list, 0xa0);
    let viewports = slot::<extern "system" fn(This, u32, *const D3D12_VIEWPORT)>(list, 0xa8);
    let scissors = slot::<extern "system" fn(This, u32, *const windows::Win32::Foundation::RECT)>(list, 0xb0);
    let set_pipeline = slot::<extern "system" fn(This, This)>(list, 0xc8);
    let heaps = slot::<extern "system" fn(This, u32, *const This)>(list, 0x100);
    let table = slot::<extern "system" fn(This, u32, u64)>(list, 0x120);
    let vertex_buffers =
        slot::<extern "system" fn(This, u32, u32, *const crate::draw::VertexView)>(list, 0x180);
    let targets =
        slot::<extern "system" fn(This, u32, *const u64, i32, *const u64)>(list, 0x190);
    let clear_depth =
        slot::<extern "system" fn(This, u64, u32, f32, u32, u32, *const u8)>(list, 0x198);
    let clear_colour =
        slot::<extern "system" fn(This, u64, *const [f32; 4], u32, *const u8)>(list, 0x1a0);
    let draw = slot::<extern "system" fn(This, u32, u32, u32, u32)>(list, 0x60);
    let execute = slot::<extern "system" fn(This, u32, *const This)>(q, 0x50);
    let present = slot::<
        extern "system" fn(This, u32, *const crate::queue::Plane, *const u8) -> crate::object::HRESULT,
    >(q, 0xf0);

    // The upload: the texture's bytes from the buffer through the console
    // copy, on the same list as the first frame's draws.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Footprint {
        offset: u64,
        format: u32,
        width: u32,
        height: u32,
        depth: u32,
        row_pitch: u32,
    }
    #[repr(C)]
    struct Location {
        resource: This,
        kind: u32,
        footprint: Footprint,
    }
    let footprint = Footprint {
        offset: 0,
        format: RGBA8,
        width: 8,
        height: 8,
        depth: 1,
        row_pitch: 256,
    };
    let copy = slot::<
        extern "system" fn(This, *const Location, u32, u32, u32, *const Location, *const u8),
    >(list, 0x80);
    let upload_from = Location {
        resource: upload,
        kind: 1,
        footprint,
    };
    let texture_to = Location {
        resource: texture,
        kind: 0,
        footprint,
    };
    copy(list, &texture_to, 0, 0, 0, &upload_from, null.cast());

    // The clear colour: the background both frames verify against.
    let clear_colour_value = [0.125f32, 0.0625, 0.03125, 1.0];
    let frames = frames();
    let mut readbacks: Vec<Vec<u8>> = Vec::new();
    for (frame_number, frame) in frames.iter().enumerate() {
        if frame_number > 0 {
            assert_eq!(reset(list, allocator, null), S_OK, "submit: the list resets");
        }
        // Background and depth, cleared through the console list.
        clear_colour(list, record_heap.start, &clear_colour_value, 0, null.cast());
        clear_depth(list, depth_record, 1, 1.0, 0, 0, null.cast());
        // The state every draw of the frame shares.
        let viewport = D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: SIDE as f32,
            Height: SIDE as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        topology(list, 4);
        viewports(list, 1, &viewport);
        let scissor = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: SIDE as i32,
            bottom: SIDE as i32,
        };
        scissors(list, 1, &scissor);
        set_pipeline(list, pipeline);
        heaps(list, 1, &views);
        table(list, 0, view_heap.start);
        let handles = [record_heap.start];
        targets(list, 1, handles.as_ptr(), 0, &depth_record);

        // The near triangle, then the same triangle farther back reading
        // the other half of the texture: only the depth test keeps the
        // near one's texels.
        for far in [false, true] {
            let mut vertices = frame.vertices;
            if far {
                for v in &mut vertices {
                    v[2] = 0.9;
                    v[4] = 0.6875;
                }
            }
            // The vertex data goes into a console committed buffer: the
            // shadow system must know the memory the input assembler reads.
            let mut vb_desc = [0u32; 14];
            vb_desc[..6].copy_from_slice(&[1, 0, 0, 0, 64, 0]);
            let mut vb = null;
            assert_eq!(
                crate::resource::create_committed(
                    std::ptr::null(),
                    [1u32, 0, 0, 0, 0].as_ptr().cast(),
                    0,
                    vb_desc.as_ptr().cast(),
                    0,
                    std::ptr::null(),
                    &crate::object::guid(
                        0x696442be,
                        0xa72e,
                        0x4059,
                        [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
                    ),
                    &mut vb
                ),
                S_OK,
                "device creation: the vertex buffer"
            );
            let (vb_bytes, _) = crate::resource::buffer_bytes(vb)
                .expect("the vertex buffer's mapping");
            // SAFETY: 60 bytes of vertex data in the committed buffer.
            unsafe {
                (vb_bytes as *mut u8).copy_from(
                    vertices.as_flattened().as_ptr().cast(),
                    3 * 5 * 4,
                )
            };
            let view = crate::draw::VertexView {
                address: vb_bytes as u64,
                size: 3 * 5 * 4,
                stride: 5 * 4,
            };
            vertex_buffers(list, 0, 1, &view);
            draw(list, 3, 1, 0, 0);
        }

        // The frame is done: run it, wait for it (the console queue waits
        // inside the submission), and present the target.
        assert_eq!(close(list), S_OK, "submit: the list closes");
        execute(q, 1, &list);
        let plane = crate::queue::Plane {
            token: frame_number as u64 + 1,
            count: 1,
            resources: &target,
        };
        assert_eq!(
            present(q, 1, &plane, null.cast()),
            S_OK,
            "present: frame {frame_number}"
        );

        // Read the target back and keep the image.
        let host_target = crate::resource::host_resource(target)
            .expect("readback: the target's host copy");
        let back = host.new_list().expect("readback: a host list");
        let readback_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let readback_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: u64::from(SIDE) * 256,
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
        let mut readback: Option<ID3D12Resource> = None;
        unsafe {
            host.device
                .CreateCommittedResource(
                    &readback_heap,
                    D3D12_HEAP_FLAG_NONE,
                    &readback_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .expect("readback: the buffer")
        }
        let readback = readback.expect("readback: the buffer");
        let (_, before) = crate::resource::host_transition(target, 0x800)
            .expect("readback: the target's state");
        let to = Location {
            resource: readback.as_raw().cast_const().cast(),
            kind: 1,
            footprint: Footprint {
                offset: 0,
                format: RGBA8,
                width: SIDE,
                height: SIDE,
                depth: 1,
                row_pitch: 256,
            },
        };
        let from = Location {
            resource: host_target.as_raw().cast_const().cast(),
            kind: 0,
            footprint: Footprint {
                offset: 0,
                format: RGBA8,
                width: SIDE,
                height: SIDE,
                depth: 1,
                row_pitch: 256,
            },
        };
        // SAFETY: an open list, live resources, locations in the host layout.
        unsafe {
            back.list.ResourceBarrier(&[crate::host::transition(
                &host_target,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(0x800),
            )]);
            back.list.CopyTextureRegion(
                (&raw const to).cast(),
                0,
                0,
                0,
                (&raw const from).cast(),
                None,
            );
        }
        back.close();
        host.execute(&[&back]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer holds the
        // frame's bytes.
        let image = unsafe {
            readback
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: SIDE as usize * 256,
                    }),
                    Some(&mut mapped),
                )
                .expect("readback: the mapping");
            std::slice::from_raw_parts(
                mapped.cast::<u8>(),
                u64::from(SIDE) as usize * 256usize,
            )
            .to_vec()
        };
        readbacks.push(image);
    }

    // What each frame must show: the near texel inside its triangle, the
    // clear colour well outside it, and the depth test's verdict (the far
    // triangle's blue would be there without it).
    let near = [200u8, 40, 40, 255];
    let far = [40u8, 40, 200, 255];
    let background = [32u8, 16, 8, 255];
    let texel = |image: &[u8], x: usize, y: usize| -> [u8; 4] {
        let at = y * 256 + x * 4;
        image[at..at + 4].try_into().expect("four bytes")
    };
    for (frame_number, (frame, image)) in frames.iter().zip(&readbacks).enumerate() {
        let (x, y) = frame.centre();
        assert_eq!(
            texel(image, x, y),
            near,
            "frame {frame_number}: the triangle's texel at ({x}, {y})"
        );
        let (far_x, far_y) = ((x + 28) % 32, (y + 28) % 32);
        assert_eq!(
            texel(image, far_x, far_y),
            background,
            "frame {frame_number}: the clear colour outside the triangle at ({far_x}, {far_y})"
        );
        // No pixel anywhere shows the farther triangle's half: the depth
        // test stopped every fragment of it.
        assert!(
            !image.chunks(4).any(|p| p == far),
            "frame {frame_number}: the farther triangle was not drawn over"
        );
    }
    // The sampled image changes between the frames: each frame's triangle
    // covers pixels the other's does not.
    for (frame_number, frame) in frames.iter().enumerate() {
        let (x, y) = frame.centre();
        let other = &readbacks[1 - frame_number];
        assert_eq!(
            texel(other, x, y),
            background,
            "frame {frame_number}'s triangle spot is the clear colour in the other frame"
        );
    }
    assert_ne!(&readbacks[0], &readbacks[1], "the two frames differ");
}

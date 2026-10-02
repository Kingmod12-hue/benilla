//! Craft mode: Minecraft-style block building inside the world. Not part of the 1.12 reference.
//!
//! Press `G` in the world to toggle build mode. In build mode the block under the crosshair
//! (screen centre) is the target: left click breaks a placed block, right click places the
//! selected block against whatever the ray hit (terrain, a building, another block), and `[` / `]`
//! cycle the block type. Blocks are one yard cubes on a world-aligned grid, solid to the body and
//! the camera (you can stand on them and build stairs), and saved per map to
//! `benilla-config/craft/blocks_map<id>.txt`, so they come back on the next login.
//!
//! Textures: `benilla-config/craft/textures/<name>.tga` (16x16 or larger, square), which
//! `IMPORT-MINECRAFT-TEXTURES.bat` copies out of the player's own Minecraft install. A missing
//! texture falls back to a flat generated colour so the mode works without it. The blocks are
//! client-side only: other players do not see them and the server does not know about them.

use std::collections::HashMap;
use std::path::PathBuf;

use avian3d::prelude::*;
use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::input::mouse::MouseButton;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use benilla_world::collision::{ColliderEpoch, WorldCollision};
use benilla_world::view::WorldCamera;
use benilla_world::world_map::CurrentMap;

use crate::char_select::InWorldGated;
use crate::net::SelfPlayer;
use crate::ui_script::{PointerOverUiPanel, UiInput, UiKeyboardCapture};

/// Edge length of one block, in yards.
const BLOCK: f32 = 1.0;
/// How far the crosshair reaches, in yards.
const REACH: f32 = 7.0;

/// One block type: the texture names for its sides, top and bottom, an optional tint for the top
/// (grass and leaves are grey in the source art and tinted by biome in their own game), and
/// whether it has see-through pixels.
struct Kind {
    label: &'static str,
    side: &'static str,
    top: &'static str,
    bottom: &'static str,
    top_tint: Option<[f32; 3]>,
    side_tint: Option<[f32; 3]>,
    cutout: bool,
    fallback: [u8; 3],
}

const GRASS: [f32; 3] = [0.49, 0.74, 0.32];
const LEAF: [f32; 3] = [0.35, 0.62, 0.22];

const KINDS: &[Kind] = &[
    Kind { label: "Grass", side: "grass_block_side", top: "grass_block_top", bottom: "dirt", top_tint: Some(GRASS), side_tint: None, cutout: false, fallback: [96, 160, 64] },
    Kind { label: "Dirt", side: "dirt", top: "dirt", bottom: "dirt", top_tint: None, side_tint: None, cutout: false, fallback: [134, 96, 67] },
    Kind { label: "Stone", side: "stone", top: "stone", bottom: "stone", top_tint: None, side_tint: None, cutout: false, fallback: [125, 125, 125] },
    Kind { label: "Cobblestone", side: "cobblestone", top: "cobblestone", bottom: "cobblestone", top_tint: None, side_tint: None, cutout: false, fallback: [110, 110, 110] },
    Kind { label: "Oak Planks", side: "oak_planks", top: "oak_planks", bottom: "oak_planks", top_tint: None, side_tint: None, cutout: false, fallback: [162, 130, 78] },
    Kind { label: "Oak Log", side: "oak_log", top: "oak_log_top", bottom: "oak_log_top", top_tint: None, side_tint: None, cutout: false, fallback: [102, 81, 50] },
    Kind { label: "Oak Leaves", side: "oak_leaves", top: "oak_leaves", bottom: "oak_leaves", top_tint: Some(LEAF), side_tint: Some(LEAF), cutout: true, fallback: [60, 120, 40] },
    Kind { label: "Bricks", side: "bricks", top: "bricks", bottom: "bricks", top_tint: None, side_tint: None, cutout: false, fallback: [150, 80, 65] },
    Kind { label: "Stone Bricks", side: "stone_bricks", top: "stone_bricks", bottom: "stone_bricks", top_tint: None, side_tint: None, cutout: false, fallback: [122, 121, 122] },
    Kind { label: "Sand", side: "sand", top: "sand", bottom: "sand", top_tint: None, side_tint: None, cutout: false, fallback: [219, 207, 163] },
    Kind { label: "Glass", side: "glass", top: "glass", bottom: "glass", top_tint: None, side_tint: None, cutout: true, fallback: [200, 230, 240] },
    Kind { label: "Crafting Table", side: "crafting_table_front", top: "crafting_table_top", bottom: "oak_planks", top_tint: None, side_tint: None, cutout: false, fallback: [120, 80, 50] },
    Kind { label: "TNT", side: "tnt_side", top: "tnt_top", bottom: "tnt_bottom", top_tint: None, side_tint: None, cutout: false, fallback: [200, 50, 40] },
    Kind { label: "Obsidian", side: "obsidian", top: "obsidian", bottom: "obsidian", top_tint: None, side_tint: None, cutout: false, fallback: [20, 16, 32] },
    Kind { label: "Glowstone", side: "glowstone", top: "glowstone", bottom: "glowstone", top_tint: None, side_tint: None, cutout: false, fallback: [250, 210, 120] },
    Kind { label: "Diamond Ore", side: "diamond_ore", top: "diamond_ore", bottom: "diamond_ore", top_tint: None, side_tint: None, cutout: false, fallback: [120, 200, 200] },
    Kind { label: "Diamond Block", side: "diamond_block", top: "diamond_block", bottom: "diamond_block", top_tint: None, side_tint: None, cutout: false, fallback: [100, 230, 220] },
    Kind { label: "Gold Block", side: "gold_block", top: "gold_block", bottom: "gold_block", top_tint: None, side_tint: None, cutout: false, fallback: [250, 210, 60] },
];

/// Marks a placed block and says where it sits on the grid.
#[derive(Component)]
pub(crate) struct CraftBlock(IVec3);

/// The build-mode HUD: the selected block's icon above the hotbar.
#[derive(Component)]
struct CraftHud;

/// Everything craft mode keeps: the shared cube mesh, one material and one icon per kind, the
/// blocks on the current map, and the input state.
#[derive(Resource, Default)]
struct Craft {
    ready: bool,
    mesh: Option<Handle<Mesh>>,
    materials: Vec<Handle<StandardMaterial>>,
    icons: Vec<Handle<Image>>,
    blocks: HashMap<IVec3, (u8, Entity)>,
    loaded_map: Option<u32>,
    build_mode: bool,
    selected: usize,
}

pub(crate) struct CraftPlugin;

impl Plugin for CraftPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Craft>().add_systems(
            Update,
            (prepare, sync_map, input, hud)
                .chain()
                .after(UiInput)
                .in_set(InWorldGated),
        );
    }
}

fn craft_dir() -> Option<PathBuf> {
    crate::local_state::home().map(|h| h.join("craft"))
}

fn save_path(map: u32) -> Option<PathBuf> {
    craft_dir().map(|d| d.join(format!("blocks_map{map}.txt")))
}

/// A square RGBA texture: the file's first frame (animated strips are taller than wide), or the
/// kind's flat colour with a little noise when the file is missing.
fn load_face(name: &str, fallback: [u8; 3]) -> (u32, Vec<u8>) {
    if let Some(path) = craft_dir().map(|d| d.join("textures").join(format!("{name}.tga"))) {
        if let Ok(bytes) = std::fs::read(&path) {
            match benilla_formats::tga_to_rgba(&bytes) {
                Ok((w, h, px)) if w > 0 && h >= w => {
                    let frame = (w * w * 4) as usize;
                    return (w, px[..frame].to_vec());
                }
                Ok(_) => warn!("craft: {} is not square, using a flat colour", path.display()),
                Err(e) => warn!("craft: {} did not decode ({e}), using a flat colour", path.display()),
            }
        }
    }
    let mut px = Vec::with_capacity(16 * 16 * 4);
    for i in 0..256u32 {
        // A cheap hash so the fallback is not a single flat colour.
        let n = (i.wrapping_mul(2654435761) >> 27) as i32 - 16;
        for c in fallback {
            px.push((c as i32 + n).clamp(0, 255) as u8);
        }
        px.push(255);
    }
    (16, px)
}

/// Scales `src` (size `s`) to `size` by nearest neighbour, multiplying RGB by `tint`.
fn resample(src: &[u8], s: u32, size: u32, tint: Option<[f32; 3]>) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let sx = x * s / size;
            let sy = y * s / size;
            let i = ((sy * s + sx) * 4) as usize;
            let mut p = [src[i], src[i + 1], src[i + 2], src[i + 3]];
            if let Some(t) = tint {
                for c in 0..3 {
                    p[c] = (p[c] as f32 * t[c]).round().clamp(0.0, 255.0) as u8;
                }
            }
            out.extend_from_slice(&p);
        }
    }
    out
}

fn pixel_sampler() -> ImageSampler {
    ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::ClampToEdge,
        address_mode_v: ImageAddressMode::ClampToEdge,
        mag_filter: ImageFilterMode::Nearest,
        min_filter: ImageFilterMode::Nearest,
        ..Default::default()
    })
}

fn rgba_image(w: u32, h: u32, data: Vec<u8>) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.sampler = pixel_sampler();
    image
}

/// The kind's atlas: side | top | bottom, left to right, each `size` square.
fn atlas(kind: &Kind) -> (Image, Image) {
    let (ss, side) = load_face(kind.side, kind.fallback);
    let (ts, top) = load_face(kind.top, kind.fallback);
    let (bs, bottom) = load_face(kind.bottom, kind.fallback);
    let size = ss.max(ts).max(bs).clamp(16, 128);
    let faces = [
        resample(&side, ss, size, kind.side_tint),
        resample(&top, ts, size, kind.top_tint),
        resample(&bottom, bs, size, None),
    ];
    let w = size * 3;
    let mut data = vec![0u8; (w * size * 4) as usize];
    for (f, face) in faces.iter().enumerate() {
        for y in 0..size {
            let src = (y * size * 4) as usize;
            let dst = ((y * w + f as u32 * size) * 4) as usize;
            data[dst..dst + (size * 4) as usize].copy_from_slice(&face[src..src + (size * 4) as usize]);
        }
    }
    let icon = rgba_image(size, size, faces[0].clone());
    (rgba_image(w, size, data), icon)
}

/// A unit cube centred on the origin whose faces sample the atlas thirds (side, top, bottom),
/// with Minecraft's fixed per-face shading in the vertex colour.
fn cube_mesh() -> Mesh {
    let h = BLOCK * 0.5;
    let third = 1.0 / 3.0;
    // (normal, the four corners counter-clockwise seen from outside, atlas third, shade)
    let faces: [([f32; 3], [[f32; 3]; 4], f32, f32); 6] = [
        ([0., 1., 0.], [[-h, h, h], [h, h, h], [h, h, -h], [-h, h, -h]], 1., 1.0),
        ([0., -1., 0.], [[-h, -h, -h], [h, -h, -h], [h, -h, h], [-h, -h, h]], 2., 0.5),
        ([1., 0., 0.], [[h, -h, h], [h, -h, -h], [h, h, -h], [h, h, h]], 0., 0.8),
        ([-1., 0., 0.], [[-h, -h, -h], [-h, -h, h], [-h, h, h], [-h, h, -h]], 0., 0.8),
        ([0., 0., 1.], [[-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h]], 0., 0.65),
        ([0., 0., -1.], [[h, -h, -h], [-h, -h, -h], [-h, h, -h], [h, h, -h]], 0., 0.65),
    ];
    let mut pos = Vec::new();
    let mut nrm = Vec::new();
    let mut uv = Vec::new();
    let mut col = Vec::new();
    let mut idx = Vec::new();
    for (n, corners, slot, shade) in faces {
        let base = pos.len() as u32;
        let u0 = slot * third;
        let u1 = u0 + third;
        // corners go bottom-left, bottom-right, top-right, top-left of the face
        let uvs = [[u0, 1.0], [u1, 1.0], [u1, 0.0], [u0, 0.0]];
        for (c, t) in corners.iter().zip(uvs) {
            pos.push(*c);
            nrm.push(n);
            uv.push(t);
            col.push([shade, shade, shade, 1.0]);
        }
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, nrm);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uv);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, col);
    mesh.insert_indices(Indices::U32(idx));
    mesh
}

/// Builds the mesh, materials and icons once, on the first in-world frame.
fn prepare(
    mut craft: ResMut<Craft>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut commands: Commands,
) {
    if craft.ready {
        return;
    }
    craft.ready = true;
    craft.mesh = Some(meshes.add(cube_mesh()));
    for kind in KINDS {
        let (tex, icon) = atlas(kind);
        let tex = images.add(tex);
        craft.icons.push(images.add(icon));
        craft.materials.push(materials.add(StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(tex),
            unlit: true,
            alpha_mode: if kind.cutout {
                AlphaMode::Mask(0.5)
            } else {
                AlphaMode::Opaque
            },
            cull_mode: if kind.cutout { None } else { Some(bevy::render::render_resource::Face::Back) },
            ..default()
        }));
    }
    // The selected-block icon, bottom centre above the hotbar; hidden until build mode.
    commands.spawn((
        CraftHud,
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(230.0),
            left: Val::Percent(50.0),
            margin: UiRect::left(Val::Px(-28.0)),
            width: Val::Px(56.0),
            height: Val::Px(56.0),
            border: UiRect::all(Val::Px(3.0)),
            ..default()
        },
        BorderColor::all(Color::WHITE),
        ImageNode::new(craft.icons[0].clone()),
        Visibility::Hidden,
        GlobalZIndex(10),
    ));
}

fn spawn_block(
    commands: &mut Commands,
    craft: &mut Craft,
    cell: IVec3,
    kind: u8,
) {
    let (Some(mesh), Some(material)) = (craft.mesh.clone(), craft.materials.get(kind as usize).cloned()) else {
        return;
    };
    let center = (cell.as_vec3() + Vec3::splat(0.5)) * BLOCK;
    let e = commands
        .spawn((
            CraftBlock(cell),
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::from_translation(center),
            RigidBody::Static,
            Collider::cuboid(BLOCK, BLOCK, BLOCK),
        ))
        .id();
    craft.blocks.insert(cell, (kind, e));
}

fn save(craft: &Craft) {
    let Some(map) = craft.loaded_map else { return };
    let Some(path) = save_path(map) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut out = String::new();
    for (cell, (kind, _)) in &craft.blocks {
        out.push_str(&format!("{} {} {} {}\n", cell.x, cell.y, cell.z, kind));
    }
    if let Err(e) = std::fs::write(&path, out) {
        warn!("craft: could not save {}: {e}", path.display());
    }
}

/// Loads the current map's blocks after login and on every worldport.
fn sync_map(
    mut commands: Commands,
    mut craft: ResMut<Craft>,
    map: Option<Res<CurrentMap>>,
    mut epoch: ResMut<ColliderEpoch>,
) {
    let Some(map) = map else { return };
    if !craft.ready || craft.loaded_map == Some(map.0) {
        return;
    }
    for (_, (_, e)) in craft.blocks.drain() {
        if let Ok(mut ec) = commands.get_entity(e) {
            ec.despawn();
        }
    }
    craft.loaded_map = Some(map.0);
    if let Some(text) = save_path(map.0).and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let v: Vec<i32> = line.split_whitespace().filter_map(|t| t.parse().ok()).collect();
            if v.len() == 4 && (v[3] as usize) < KINDS.len() {
                spawn_block(&mut commands, &mut craft, IVec3::new(v[0], v[1], v[2]), v[3] as u8);
            }
        }
        info!("craft: loaded {} blocks for map {}", craft.blocks.len(), map.0);
    }
    epoch.bump();
}

#[allow(clippy::too_many_arguments)] // one Bevy system's input set
fn input(
    mut commands: Commands,
    mut craft: ResMut<Craft>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    typing: Res<UiKeyboardCapture>,
    over_ui: Res<PointerOverUiPanel>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    me: Query<&Transform, With<SelfPlayer>>,
    blocks: Query<&CraftBlock>,
    world: WorldCollision,
    mut epoch: ResMut<ColliderEpoch>,
) {
    if !craft.ready || craft.loaded_map.is_none() {
        return;
    }
    if !typing.typing {
        if keys.just_pressed(KeyCode::KeyG) {
            craft.build_mode = !craft.build_mode;
            info!("craft: build mode {}", if craft.build_mode { "on" } else { "off" });
        }
        if craft.build_mode {
            if keys.just_pressed(KeyCode::BracketRight) {
                craft.selected = (craft.selected + 1) % KINDS.len();
            }
            if keys.just_pressed(KeyCode::BracketLeft) {
                craft.selected = (craft.selected + KINDS.len() - 1) % KINDS.len();
            }
        }
    }
    if !craft.build_mode || over_ui.0 {
        return;
    }
    let breaking = mouse.just_pressed(MouseButton::Left);
    let placing = mouse.just_pressed(MouseButton::Right);
    if !breaking && !placing {
        return;
    }
    let Ok(cam) = camera.single() else { return };
    let origin = cam.translation();
    let Ok(dir) = Dir3::new(cam.forward().as_vec3()) else { return };
    let Some(hit) = world.ray_body(origin, dir, REACH) else { return };

    if breaking {
        if let Ok(block) = blocks.get(hit.entity) {
            let cell = block.0;
            if let Some((_, e)) = craft.blocks.remove(&cell) {
                if let Ok(mut ec) = commands.get_entity(e) {
                    ec.despawn();
                }
                epoch.bump();
                save(&craft);
            }
        }
        return;
    }

    // Placing: the cell on the near side of the face the crosshair hit.
    let point = origin + *dir * hit.distance;
    let normal = hit.normal.normalize_or_zero();
    let cell = ((point + normal * (BLOCK * 0.5)) / BLOCK).floor().as_ivec3();
    if craft.blocks.contains_key(&cell) {
        return;
    }
    // Never inside the player's own body.
    if let Ok(body) = me.single() {
        let c = (cell.as_vec3() + Vec3::splat(0.5)) * BLOCK;
        let d = c - body.translation;
        let horizontal = Vec2::new(d.x, d.z).length();
        if horizontal < BLOCK * 0.5 + 0.35 && d.y > -BLOCK * 0.5 && d.y < 2.2 + BLOCK * 0.5 {
            return;
        }
    }
    let kind = craft.selected as u8;
    spawn_block(&mut commands, &mut craft, cell, kind);
    epoch.bump();
    save(&craft);
}

fn hud(craft: Res<Craft>, mut q: Query<(&mut Visibility, &mut ImageNode), With<CraftHud>>) {
    if !craft.is_changed() {
        return;
    }
    for (mut vis, mut image) in &mut q {
        *vis = if craft.build_mode {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if let Some(icon) = craft.icons.get(craft.selected) {
            if image.image != *icon {
                image.image = icon.clone();
            }
        }
    }
}

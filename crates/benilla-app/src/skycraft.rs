//! SkyCraft host: real Minecraft running beside benilla, as SkyCraft (Minecraft in Skyrim) does.
//! Not part of the 1.12 reference.
//!
//! Minecraft (26.3 + Fabric + the SkyCraft mod, `skycraft-fabric-0.1.0.jar`) runs hidden and owns
//! the player's movement physics; benilla owns the world, the server and the picture. The two talk
//! through SkyCraft's shared-memory protocol (`protocol/skycraft_protocol.h` in that project,
//! version 10 for the 0.1.0 release), with benilla in the role of the Skyrim SKSE plugin.
//!
//! Stage 1 (this file): the link, the coordinate mapping, collision export (WoW's terrain, buildings
//! and doodads as Minecraft collision), and the follower: once Minecraft acknowledges the
//! teleport, the avatar stands where Minecraft's player stands, and the movement keys are mirrored
//! to Minecraft. The look stays benilla's: its camera yaw and pitch are Minecraft's.
//!
//! Enabled by `BENILLA_SKYCRAFT=1` (PLAY-SKYCRAFT.bat). Mapping: 1 block = `SKYCRAFT_SCALE` yards (1.3), MC axes = Bevy
//! axes (both right-handed, Y up), so MC (x, y, z) = Bevy (x, y, z) / yards_per_block().

use std::collections::HashMap;

use bevy::input::keyboard::KeyboardInput;
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

use benilla_world::collision::{ColliderEpoch, GroundDecalSurface, WorldCollision};
use benilla_world::view::WorldCamera;
use benilla_world::world_map::CurrentMap;

use crate::char_select::InWorldGated;
use crate::net::{TeleportMessage, WorldportMessage};
use crate::player::Player;
use crate::ui_script::{UiInput, UiKeyboardCapture};

/// Yards per Minecraft block: `SKYCRAFT_SCALE`, 1.3 by default, which puts Minecraft's sprint
/// (5.6 blocks/s) at WoW's run speed (7 yd/s) and its walk at about 5.6 yd/s.
fn yards_per_block() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("SKYCRAFT_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| (0.5..=3.0).contains(v))
            .unwrap_or(1.3)
    })
}

// ---- protocol (SkyCraft v10/v11 share these layouts) ------------------------------------------
const MAGIC: u32 = 0x4359_4B53;
const OFF_SKY_STATE: usize = 0x100;
const OFF_MC_STATE: usize = 0x200;
const OFF_INPUT_RING: usize = 0x1000;
const OFF_EVENT_RING: usize = 0x17000;
const OFF_COLLISION_RING: usize = 0x20000;
const COLLISION_RING_BYTES: usize = 32 << 20;
const OFF_OVERLAY_PIXELS: usize = OFF_COLLISION_RING + COLLISION_RING_BYTES;
const OVERLAY_SLOT_BYTES: usize = 3840 * 2160 * 4;
const OFF_RENDER_RING: usize = OFF_OVERLAY_PIXELS + OVERLAY_SLOT_BYTES * 3;
const RENDER_RING_BYTES: usize = 64 << 20;
const MAPPING_BYTES: usize = OFF_RENDER_RING + RENDER_RING_BYTES;
const OFF_OVERLAY_CTL: usize = 0x300;
const OFF_ACTOR_TABLE: usize = 0x12000;
const OFF_WORLD_ENTITIES: usize = 0x1C000;

const RING_HEAD: usize = 0x00;
const RING_TAIL: usize = 0x40;
const RING_DATA: usize = 0x80;
const INPUT_RING_ENTRIES: u64 = 4096;
const COL_DATA_BYTES: u64 = (COLLISION_RING_BYTES - RING_DATA) as u64;

const SKY_IN_GAME: u32 = 1;
const SKY_MENU_OPEN: u32 = 2;
const SKY_LOADING: u32 = 4;
const MC_IN_WORLD: u32 = 1;
const MC_DEAD: u32 = 1 << 5;

const IN_KEY: u16 = 1;
const IN_RELEASE_ALL: u16 = 6;

const COL_PAD: u32 = 0;
const COL_CLEAR: u32 = 1;
const COL_REGION: u32 = 2;
const COL_TRIS: u32 = 3;
const TRI_TERRAIN: u32 = 1 << 3;

/// Blocks per collision region edge (SkyCraft's REGION_SIZE).
const REGION: i32 = 8;
/// Regions streamed around the player: horizontal radius and vertical reach.
const RADIUS_XZ: i32 = 4;
const RADIUS_Y: i32 = 2;
/// Regions harvested per frame.
const REGIONS_PER_FRAME: usize = 3;
/// How far above a region the terrain is looked for, so buried regions fill solid (blocks).
const TERRAIN_LOOKUP_ABOVE: f32 = 96.0;
/// A sent region is re-sent this long after the world's colliders changed (seconds).
const RESEND_AFTER: f32 = 1.5;
const MC_TIMEOUT_MS: u64 = 3000;

fn protocol_version() -> u32 {
    std::env::var("SKYCRAFT_PROTOCOL")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(10)
}

pub(crate) fn enabled() -> bool {
    std::env::var("BENILLA_SKYCRAFT").is_ok_and(|v| v.trim() == "1")
}

// ---- the shared mapping -------------------------------------------------------------------------

/// The mapping's base address. Raw memory shared with another process: every access below goes
/// through volatile or atomic reads and writes at the protocol's fixed offsets.
#[derive(Clone, Copy)]
struct Mapping {
    base: *mut u8,
}

// SAFETY: the mapping lives for the process; access is serialized by the protocol's own
// seqlocks and single-producer rings, and Bevy runs these systems on one thread at a time.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

#[cfg(windows)]
fn tick_ms() -> u64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
}
#[cfg(not(windows))]
fn tick_ms() -> u64 {
    0
}

impl Mapping {
    #[cfg(windows)]
    fn create() -> Option<Self> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
        };
        let name: Vec<u16> = "Local\\SkyCraft_v1\0".encode_utf16().collect();
        let size = MAPPING_BYTES as u64;
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                (size >> 32) as u32,
                (size & 0xFFFF_FFFF) as u32,
                name.as_ptr(),
            );
            if handle.is_null() {
                warn!("skycraft: CreateFileMapping failed");
                return None;
            }
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if view.Value.is_null() {
                warn!("skycraft: MapViewOfFile failed");
                return None;
            }
            let m = Mapping {
                base: view.Value as *mut u8,
            };
            m.init();
            Some(m)
        }
    }
    #[cfg(not(windows))]
    fn create() -> Option<Self> {
        warn!("skycraft: the shared-memory link is Windows-only");
        None
    }

    fn zero(&self, off: usize, len: usize) {
        unsafe { std::ptr::write_bytes(self.base.add(off), 0, len) };
    }

    fn init(&self) {
        self.zero(0, 0x100);
        self.zero(OFF_SKY_STATE, 0x40);
        self.zero(OFF_OVERLAY_CTL, 0x100);
        self.zero(OFF_INPUT_RING, RING_DATA);
        self.zero(OFF_COLLISION_RING, RING_DATA);
        self.zero(OFF_ACTOR_TABLE, 0x40);
        self.zero(OFF_EVENT_RING, RING_DATA);
        self.zero(OFF_WORLD_ENTITIES, 0x40);
        self.zero(OFF_RENDER_RING, RING_DATA);
        self.write_u32(4, protocol_version());
        self.write_u32(8, std::process::id());
        self.write_u64(0x10, tick_ms());
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        self.write_u32(0, MAGIC);
        info!(
            "skycraft: shared memory Local\\SkyCraft_v1 ready ({} MB, protocol {})",
            MAPPING_BYTES >> 20,
            protocol_version()
        );
    }

    fn atomic_u64(&self, off: usize) -> &std::sync::atomic::AtomicU64 {
        unsafe { &*(self.base.add(off) as *const std::sync::atomic::AtomicU64) }
    }
    fn atomic_u32(&self, off: usize) -> &std::sync::atomic::AtomicU32 {
        unsafe { &*(self.base.add(off) as *const std::sync::atomic::AtomicU32) }
    }
    fn write_u32(&self, off: usize, v: u32) {
        unsafe { std::ptr::write_volatile(self.base.add(off) as *mut u32, v) };
    }
    fn write_u64(&self, off: usize, v: u64) {
        unsafe { std::ptr::write_volatile(self.base.add(off) as *mut u64, v) };
    }
    fn write_bytes(&self, off: usize, data: &[u8]) {
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.base.add(off), data.len()) };
    }
    fn read_bytes(&self, off: usize, out: &mut [u8]) {
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(off), out.as_mut_ptr(), out.len()) };
    }

    fn heartbeat(&self) {
        use std::sync::atomic::Ordering;
        self.atomic_u64(0x10).store(tick_ms(), Ordering::Release);
    }
    fn mc_pid(&self) -> u32 {
        self.atomic_u32(0xC).load(std::sync::atomic::Ordering::Acquire)
    }
    fn mc_alive(&self) -> bool {
        let beat = self.atomic_u64(0x18).load(std::sync::atomic::Ordering::Acquire);
        beat != 0 && tick_ms().saturating_sub(beat) < MC_TIMEOUT_MS
    }

    /// Seqlock write of the 0x40-byte SkyState (`state` excludes the leading seq word).
    fn write_sky_state(&self, state: &[u8; 0x3C]) {
        use std::sync::atomic::Ordering;
        let seq = self.atomic_u32(OFF_SKY_STATE);
        let s = seq.load(Ordering::Relaxed) & !1;
        seq.store(s.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.write_bytes(OFF_SKY_STATE + 4, state);
        seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Seqlock read of McState (0xC8 bytes).
    fn read_mc_state(&self) -> Option<[u8; 0xC8]> {
        use std::sync::atomic::Ordering;
        let seq = self.atomic_u32(OFF_MC_STATE);
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let mut buf = [0u8; 0xC8];
            self.read_bytes(OFF_MC_STATE, &mut buf);
            std::sync::atomic::fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == s1 {
                return Some(buf);
            }
        }
        None
    }

    fn push_input(&self, kind: u16, code: u16, a: i32, b: i32, c: i32) {
        use std::sync::atomic::Ordering;
        let head_a = self.atomic_u64(OFF_INPUT_RING + RING_HEAD);
        let head = head_a.load(Ordering::Relaxed);
        let tail = self.atomic_u64(OFF_INPUT_RING + RING_TAIL).load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= INPUT_RING_ENTRIES {
            return;
        }
        let slot = OFF_INPUT_RING + RING_DATA + ((head & (INPUT_RING_ENTRIES - 1)) as usize) * 16;
        let mut e = [0u8; 16];
        e[0..2].copy_from_slice(&kind.to_le_bytes());
        e[2..4].copy_from_slice(&code.to_le_bytes());
        e[4..8].copy_from_slice(&a.to_le_bytes());
        e[8..12].copy_from_slice(&b.to_le_bytes());
        e[12..16].copy_from_slice(&c.to_le_bytes());
        self.write_bytes(slot, &e);
        head_a.store(head + 1, Ordering::Release);
    }

    /// One collision-ring message; false when the ring is full (try again next frame).
    fn write_collision(&self, kind: u32, payload: &[u8]) -> bool {
        use std::sync::atomic::Ordering;
        let head_a = self.atomic_u64(OFF_COLLISION_RING + RING_HEAD);
        let mut head = head_a.load(Ordering::Relaxed);
        let tail = self.atomic_u64(OFF_COLLISION_RING + RING_TAIL).load(Ordering::Acquire);
        let msg = ((8 + payload.len() as u64) + 7) & !7;
        if msg > COL_DATA_BYTES / 2 {
            warn!("skycraft: collision message too large ({msg} bytes)");
            return true; // drop it rather than retry forever
        }
        let mut pos = head % COL_DATA_BYTES;
        let pad = if pos + msg > COL_DATA_BYTES {
            COL_DATA_BYTES - pos
        } else {
            0
        };
        if COL_DATA_BYTES - head.wrapping_sub(tail) < msg + pad {
            return false;
        }
        let data = OFF_COLLISION_RING + RING_DATA;
        if pad > 0 {
            self.write_u32(data + pos as usize, COL_PAD);
            self.write_u32(data + pos as usize + 4, 0);
            head += pad;
            pos = 0;
        }
        self.write_u32(data + pos as usize, kind);
        self.write_u32(data + pos as usize + 4, payload.len() as u32);
        self.write_bytes(data + pos as usize + 8, payload);
        head_a.store(head + msg, Ordering::Release);
        true
    }

    /// Stage 1 draws nothing of Minecraft's: consume its event and render rings so it never stalls.
    fn drain_guest_rings(&self) {
        use std::sync::atomic::Ordering;
        for off in [OFF_EVENT_RING, OFF_RENDER_RING] {
            let head = self.atomic_u64(off + RING_HEAD).load(Ordering::Acquire);
            self.atomic_u64(off + RING_TAIL).store(head, Ordering::Release);
        }
    }
}

// ---- state ------------------------------------------------------------------------------------

/// Minecraft's player as last read, in Bevy space.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct McPlayer {
    pub(crate) feet: Vec3,
    pub(crate) eye: Vec3,
    pub(crate) on_ground: bool,
    pub(crate) sprinting: bool,
    /// Minecraft's effective vertical FOV (degrees), sprint widening included.
    pub(crate) fov_deg: f32,
    /// Walk-bob phase and amplitude, as Minecraft's `bobView` reads them.
    pub(crate) bob_phase: f32,
    pub(crate) bob_amount: f32,
}

/// What the player controller reads: when `Some`, the avatar stands where Minecraft's player does.
#[derive(Resource, Default)]
pub(crate) struct ExternalPilot {
    pub(crate) follow: Option<McPlayer>,
}

#[derive(Resource, Default)]
struct SkyHost {
    link: Option<Mapping>,
    tried: bool,
    mc_pid: u32,
    teleport_seq: u32,
    teleport_pending: bool,
    epoch: u32,
    epoch_sent: u32,
    map: Option<u32>,
    sent: HashMap<IVec3, f32>,
    dirty_since: Option<f32>,
    collider_epoch: u64,
    keys_down: Vec<u16>,
    typing_was: bool,
    last_log: f32,
    in_world_logged: bool,
    last_flags: u32,
    /// Minecraft's FOV while not sprinting: the base its sprint widening is measured from.
    base_fov: f32,
}

pub(crate) struct SkyCraftPlugin;

impl Plugin for SkyCraftPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ExternalPilot>();
        if !enabled() {
            return;
        }
        info!("skycraft: enabled (BENILLA_SKYCRAFT=1)");
        app.init_resource::<SkyHost>().add_systems(
            Update,
            (host_frame, forward_keys, stream_collision)
                .chain()
                .after(UiInput)
                .before(crate::player::PlayerControlSet)
                .in_set(InWorldGated),
        )
        .add_systems(
            Update,
            minecraft_camera
                .after(crate::player::PlayerControlSet)
                .in_set(InWorldGated),
        );
    }
}

fn sdl_scancode(key: KeyCode) -> Option<u16> {
    Some(match key {
        KeyCode::KeyW => 26,
        KeyCode::KeyA => 4,
        KeyCode::KeyS => 22,
        KeyCode::KeyD => 7,
        KeyCode::Space => 44,
        KeyCode::ShiftLeft => 225,
        KeyCode::ControlLeft => 224,
        _ => return None,
    })
}

fn to_mc(v: Vec3) -> [f64; 3] {
    let s = yards_per_block() as f64;
    [v.x as f64 / s, v.y as f64 / s, v.z as f64 / s]
}
fn from_mc(x: f64, y: f64, z: f64) -> Vec3 {
    Vec3::new(x as f32, y as f32, z as f32) * yards_per_block()
}

fn f64_at(b: &[u8], off: usize) -> f64 {
    f64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// The link's per-frame work: heartbeat, SkyState out, McState in, the follower and teleports.
#[allow(clippy::too_many_arguments)]
fn host_frame(
    mut host: ResMut<SkyHost>,
    mut pilot: ResMut<ExternalPilot>,
    player: Res<Player>,
    map: Option<Res<CurrentMap>>,
    camera: Query<&GlobalTransform, With<WorldCamera>>,
    window: Query<&Window, With<PrimaryWindow>>,
    typing: Res<UiKeyboardCapture>,
    loading: Res<crate::loading_screen::LoadingScreen>,
    mut teleports: MessageReader<TeleportMessage>,
    mut worldports: MessageReader<WorldportMessage>,
    time: Res<Time>,
) {
    if !host.tried {
        host.tried = true;
        host.link = Mapping::create();
        host.epoch = 1;
        // Never 0, which a fresh Minecraft reports before its first teleport.
        host.teleport_seq = (tick_ms() % 100_000) as u32 + 2;
    }
    pilot.follow = None;
    let Some(link) = host.link else {
        return;
    };
    link.heartbeat();
    link.drain_guest_rings();

    // A (re)started Minecraft starts over: fresh collision and a teleport to us.
    let pid = link.mc_pid();
    let alive = link.mc_alive();
    let mut new_guest = false;
    if alive && pid != host.mc_pid {
        info!("skycraft: Minecraft linked (pid {pid})");
        new_guest = true;
    }
    let map_id = map.map(|m| m.0);
    let mut world_changed = false;
    if map_id != host.map {
        world_changed = host.map.is_some();
        host.map = map_id;
    }
    if teleports.read().count() > 0 || worldports.read().count() > 0 {
        host.teleport_pending = true;
    }
    if new_guest || world_changed {
        host.mc_pid = pid;
        host.epoch = host.epoch.wrapping_add(1).max(1);
        host.sent.clear();
        host.teleport_pending = true;
        host.in_world_logged = false;
    }
    if host.teleport_pending && player.active {
        host.teleport_seq = host.teleport_seq.wrapping_add(1).max(1);
        host.teleport_pending = false;
    }

    // The look: benilla's camera, as Minecraft yaw/pitch (degrees; yaw 0 = +Z, pitch + = down).
    let (yaw, pitch) = camera
        .single()
        .map(|t| {
            let f = t.forward().as_vec3();
            (
                (-f.x).atan2(f.z).to_degrees(),
                (-f.y).clamp(-1.0, 1.0).asin().to_degrees(),
            )
        })
        .unwrap_or((0.0, 0.0));
    let (vw, vh) = window
        .single()
        .map(|w| (w.physical_width(), w.physical_height()))
        .unwrap_or((1280, 720));

    let mut flags = 0;
    if player.active {
        flags |= SKY_IN_GAME;
    }
    if typing.typing {
        flags |= SKY_MENU_OPEN;
    }
    if loading.covering() {
        flags |= SKY_LOADING;
    }
    let p = to_mc(player.pos);
    let mut s = [0u8; 0x3C];
    s[0..4].copy_from_slice(&flags.to_le_bytes());
    s[4..8].copy_from_slice(&host.map.unwrap_or(0).wrapping_add(1).to_le_bytes());
    s[8..12].copy_from_slice(&host.epoch.to_le_bytes());
    s[12..20].copy_from_slice(&p[0].to_le_bytes());
    s[20..28].copy_from_slice(&p[1].to_le_bytes());
    s[28..36].copy_from_slice(&p[2].to_le_bytes());
    s[36..40].copy_from_slice(&yaw.to_le_bytes());
    s[40..44].copy_from_slice(&pitch.to_le_bytes());
    s[44..48].copy_from_slice(&host.teleport_seq.to_le_bytes());
    s[48..52].copy_from_slice(&vw.to_le_bytes());
    s[52..56].copy_from_slice(&vh.to_le_bytes());
    s[56..60].copy_from_slice(&12.0f32.to_le_bytes());
    link.write_sky_state(&s);

    if !alive {
        return;
    }
    let Some(mc) = link.read_mc_state() else {
        return;
    };
    let mc_flags = u32_at(&mc, 4);
    let ack = u32_at(&mc, 0x30);
    let feet = from_mc(f64_at(&mc, 0x08), f64_at(&mc, 0x10), f64_at(&mc, 0x18));
    let eye = from_mc(f64_at(&mc, 0x50), f64_at(&mc, 0x58), f64_at(&mc, 0x60));
    let in_world = mc_flags & MC_IN_WORLD != 0;
    if mc_flags != host.last_flags {
        info!(
            "skycraft: mc flags {:#x} -> {mc_flags:#x} (sneaking {}, sprinting {})",
            host.last_flags,
            mc_flags & (1 << 3) != 0,
            mc_flags & (1 << 4) != 0
        );
        host.last_flags = mc_flags;
    }
    let now = time.elapsed_secs();
    if now - host.last_log > 5.0 {
        host.last_log = now;
        info!(
            "skycraft: mc flags {mc_flags:#x} ack {ack}/{} feet {feet:?} wow {:?}",
            host.teleport_seq, player.pos
        );
    }
    if in_world && mc_flags & MC_DEAD == 0 && ack == host.teleport_seq && player.active {
        if !host.in_world_logged {
            host.in_world_logged = true;
            info!("skycraft: following Minecraft's player");
        }
        let f32_at = |o: usize| f32::from_le_bytes(mc[o..o + 4].try_into().unwrap());
        pilot.follow = Some(McPlayer {
            feet,
            eye,
            on_ground: mc_flags & (1 << 2) != 0,
            sprinting: mc_flags & (1 << 4) != 0,
            fov_deg: f32_at(0x40),
            bob_phase: f32_at(0x44),
            bob_amount: f32_at(0x48),
        });
    }
}

/// Mirrors the movement keys to Minecraft (benilla still sees them too, for its own move flags).
fn forward_keys(
    mut host: ResMut<SkyHost>,
    mut keys: MessageReader<KeyboardInput>,
    typing: Res<UiKeyboardCapture>,
) {
    let Some(link) = host.link else {
        keys.clear();
        return;
    };
    let typing_now = typing.typing;
    let mut down = std::mem::take(&mut host.keys_down);
    if typing_now && !host.typing_was {
        link.push_input(IN_RELEASE_ALL, 0, 0, 0, 0);
        down.clear();
    }
    for ev in keys.read() {
        let Some(code) = sdl_scancode(ev.key_code) else {
            continue;
        };
        let pressed = ev.state == ButtonState::Pressed;
        if typing_now && pressed {
            continue;
        }
        let was = down.contains(&code);
        if code >= 224 && pressed != was {
            info!("skycraft: key {code} {}", if pressed { "down" } else { "up" });
        }
        if pressed && !was {
            down.push(code);
            link.push_input(IN_KEY, code, 1, 0, 0);
        } else if !pressed && was {
            down.retain(|c| *c != code);
            link.push_input(IN_KEY, code, 0, 0, 0);
        }
    }
    host.keys_down = down;
    host.typing_was = typing_now;
}

// ---- collision export ----------------------------------------------------------------------------

fn region_of(p: Vec3) -> IVec3 {
    let b = p / yards_per_block();
    IVec3::new(
        (b.x.floor() as i32).div_euclid(REGION),
        (b.y.floor() as i32).div_euclid(REGION),
        (b.z.floor() as i32).div_euclid(REGION),
    )
}

/// Streams WoW's collision near the player to Minecraft, a few regions a frame, nearest first.
fn stream_collision(
    mut host: ResMut<SkyHost>,
    player: Res<Player>,
    world: WorldCollision,
    terrain: Query<(), With<GroundDecalSurface>>,
    colliders: Res<ColliderEpoch>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs();
    if host.link.as_ref().is_none_or(|l| !l.mc_alive()) || !player.active {
        return;
    }
    // A new epoch: tell Minecraft to drop everything first.
    if host.epoch_sent != host.epoch {
        let epoch = host.epoch;
        if !host.link.unwrap().write_collision(COL_CLEAR, &epoch.to_le_bytes()) {
            return;
        }
        host.epoch_sent = epoch;
        host.sent.clear();
    }
    // Colliders streamed in or out: re-send what we already sent, after things settle.
    if colliders.get() != host.collider_epoch {
        host.collider_epoch = colliders.get();
        host.dirty_since = Some(now);
    }
    if let Some(t) = host.dirty_since {
        if now - t > RESEND_AFTER {
            host.dirty_since = None;
            host.sent.retain(|_, sent_at| *sent_at > t);
        }
    }

    let center = region_of(player.pos);
    let mut want: Vec<IVec3> = Vec::new();
    for dy in -RADIUS_Y..=RADIUS_Y {
        for dz in -RADIUS_XZ..=RADIUS_XZ {
            for dx in -RADIUS_XZ..=RADIUS_XZ {
                let r = center + IVec3::new(dx, dy, dz);
                if !host.sent.contains_key(&r) {
                    want.push(r);
                }
            }
        }
    }
    want.sort_by_key(|r| {
        let d = *r - center;
        d.x * d.x + d.z * d.z + 4 * d.y * d.y
    });
    let epoch = host.epoch;
    for r in want.into_iter().take(REGIONS_PER_FRAME) {
        let (tris, region) = harvest(r, epoch, &world, &terrain);
        let link = host.link.unwrap();
        if !link.write_collision(COL_TRIS, &tris) || !link.write_collision(COL_REGION, &region) {
            break;
        }
        host.sent.insert(r, now);
    }
    // Forget far regions so they are sent again when we come back.
    host.sent.retain(|r, _| {
        let d = *r - center;
        d.x.abs() <= RADIUS_XZ + 2 && d.z.abs() <= RADIUS_XZ + 2 && d.y.abs() <= RADIUS_Y + 2
    });
}

fn region_header(r: IVec3, epoch: u32, count: u32) -> Vec<u8> {
    let min = r * REGION;
    let max = min + IVec3::splat(REGION - 1);
    let mut out = Vec::with_capacity(32);
    for v in [min.x, min.y, min.z, max.x, max.y, max.z] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out
}

/// One region: its triangles (MC space) for the player's smooth collider, and an 8x8x8-sub-voxel
/// occupancy per block: terrain filled solid below the surface, everything else as a shell.
fn harvest(
    r: IVec3,
    epoch: u32,
    world: &WorldCollision,
    terrain: &Query<(), With<GroundDecalSurface>>,
) -> (Vec<u8>, Vec<u8>) {
    let size = REGION as f32;
    let min_b = (r * REGION).as_vec3();
    let max_b = min_b + Vec3::splat(size);
    // Look a long way up for the terrain over this region, to fill buried regions.
    let lo = min_b * yards_per_block();
    let hi = Vec3::new(max_b.x, max_b.y + TERRAIN_LOOKUP_ABOVE, max_b.z) * yards_per_block();
    let faces = world.faces_near_body((lo + hi) * 0.5, (hi - lo) * 0.5 + Vec3::splat(0.05), 200_000);

    // Sub-voxel grid of the region: 64 x 64 x 64 bits as [y][z] -> u64 (x).
    const N: usize = (REGION as usize) * 8;
    let mut solid = vec![0u64; N * N];
    let set = |solid: &mut Vec<u64>, x: usize, y: usize, z: usize| {
        solid[y * N + z] |= 1u64 << x;
    };
    // Terrain height per sub-column (in region sub-voxel units from the bottom), highest wins.
    let mut ground = vec![f32::NEG_INFINITY; N * N];

    let mut tri_bytes = Vec::new();
    let mut tri_count = 0u32;
    for f in &faces {
        let v = f.world_verts().map(|p| p / yards_per_block());
        let is_terrain = terrain.contains(f.entity);
        let tmin = v[0].min(v[1]).min(v[2]);
        let tmax = v[0].max(v[1]).max(v[2]);
        let overlaps = tmax.x >= min_b.x
            && tmin.x <= max_b.x
            && tmax.z >= min_b.z
            && tmin.z <= max_b.z
            && tmax.y >= min_b.y
            && tmin.y <= max_b.y;
        if overlaps {
            for p in v {
                for c in [p.x, p.y, p.z] {
                    tri_bytes.extend_from_slice(&c.to_le_bytes());
                }
            }
            let flags = if is_terrain { TRI_TERRAIN } else { 0 };
            tri_bytes.extend_from_slice(&flags.to_le_bytes());
            tri_count += 1;
        }
        // Rasterize the triangle into sub-voxels (sample spacing under half a sub-voxel).
        let e1 = v[1] - v[0];
        let e2 = v[2] - v[0];
        if !overlaps && !is_terrain {
            continue;
        }
        let steps = ((e1.length().max(e2.length()) * 16.0).ceil() as usize).clamp(1, 512);
        let inv = 1.0 / steps as f32;
        for i in 0..=steps {
            for j in 0..=(steps - i) {
                let p = v[0] + e1 * (i as f32 * inv) + e2 * (j as f32 * inv);
                let local = (p - min_b) * 8.0;
                let (x, z) = (local.x.floor(), local.z.floor());
                if x < 0.0 || z < 0.0 || x >= N as f32 || z >= N as f32 {
                    continue;
                }
                let (xi, zi) = (x as usize, z as usize);
                if is_terrain {
                    let g = &mut ground[zi * N + xi];
                    *g = g.max(local.y);
                }
                let y = local.y.floor();
                if y >= 0.0 && y < N as f32 {
                    set(&mut solid, xi, y as usize, zi);
                }
            }
        }
    }
    // Terrain: everything below the surface is solid.
    for z in 0..N {
        for x in 0..N {
            let g = ground[z * N + x];
            if g == f32::NEG_INFINITY {
                continue;
            }
            let top = (g.floor() as i64).min(N as i64 - 1);
            for y in 0..=top.max(-1) {
                set(&mut solid, x, y as usize, z);
            }
        }
    }

    let mut tris = region_header(r, epoch, tri_count);
    tris.extend_from_slice(&tri_bytes);

    // Blocks with any solid sub-voxel: {x, y, z, pad, bits[8]} with bits[y] bit (z*8 + x).
    let mut blocks = Vec::new();
    let mut count = 0u32;
    let base = r * REGION;
    for by in 0..REGION as usize {
        for bz in 0..REGION as usize {
            for bx in 0..REGION as usize {
                let mut bits = [0u64; 8];
                let mut any = false;
                for sy in 0..8 {
                    let mut layer = 0u64;
                    for sz in 0..8 {
                        let row = solid[(by * 8 + sy) * N + bz * 8 + sz];
                        let byte = (row >> (bx * 8)) & 0xFF;
                        layer |= byte << (sz * 8);
                    }
                    any |= layer != 0;
                    bits[sy] = layer;
                }
                if any {
                    let p = base + IVec3::new(bx as i32, by as i32, bz as i32);
                    for c in [p.x, p.y, p.z, 0] {
                        blocks.extend_from_slice(&c.to_le_bytes());
                    }
                    for l in bits {
                        blocks.extend_from_slice(&l.to_le_bytes());
                    }
                    count += 1;
                }
            }
        }
    }
    let mut region = region_header(r, epoch, count);
    region.extend_from_slice(&blocks);
    (tris, region)
}

// ---- camera ------------------------------------------------------------------------------------

/// In first person, the view is Minecraft's: its eye (the sneak dip), its walk bob, and its FOV
/// changes (sprinting widens it), applied as a ratio to benilla's own FOV.
fn minecraft_camera(
    pilot: Res<ExternalPilot>,
    mut host: ResMut<SkyHost>,
    mut camera: Query<(&mut Transform, &mut Projection), With<WorldCamera>>,
) {
    let Some(mc) = pilot.follow else {
        return;
    };
    let Ok((mut t, mut projection)) = camera.single_mut() else {
        return;
    };
    if mc.fov_deg > 1.0 && (!mc.sprinting || host.base_fov <= 1.0) {
        host.base_fov = if host.base_fov <= 1.0 {
            mc.fov_deg
        } else {
            host.base_fov + (mc.fov_deg - host.base_fov) * 0.1
        };
    }
    // Third person (the camera pulled back from the head) keeps benilla's own camera.
    if t.translation.distance(mc.eye) > 2.0 {
        return;
    }
    let g = -mc.bob_phase * std::f32::consts::PI;
    let h = mc.bob_amount;
    let right = t.right().as_vec3();
    let up = t.up().as_vec3();
    t.translation = mc.eye - right * (g.sin() * h * 0.5) + up * ((g.cos() * h).abs());
    let roll = (g.sin() * h * 3.0).to_radians();
    let nod = ((g - 0.2).cos() * h).abs() * 5.0;
    let fwd = t.forward();
    t.rotate_axis(fwd, -roll);
    let side = t.right();
    t.rotate_axis(side, -nod.to_radians());
    if host.base_fov > 1.0 && mc.fov_deg > 1.0 {
        if let Projection::Perspective(p) = &mut *projection {
            p.fov = benilla_world::view::CAM_FOVY * (mc.fov_deg / host.base_fov).clamp(0.5, 1.5);
        }
    }
}

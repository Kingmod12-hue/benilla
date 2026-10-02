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

use bevy::asset::RenderAssetUsages;
use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::{AccumulatedMouseScroll, MouseScrollUnit};
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::window::PrimaryWindow;

use benilla_world::collision::{ColliderEpoch, GroundDecalSurface, WorldCollision};
use benilla_world::view::WorldCamera;
use benilla_world::world_map::CurrentMap;

use crate::char_select::InWorldGated;
use crate::net::{TeleportMessage, WorldportMessage};
use crate::player::Player;
use crate::ui_script::{PointerOverUi, UiInput, UiKeyFeed, UiKeyboardCapture};

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
const OFF_OVERLAY_SLOT_HDR: usize = 0x340;
const OVERLAY_DIRTY: u32 = 1 << 2;
const OFF_ACTOR_TABLE: usize = 0x12000;
const OFF_WORLD_ENTITIES: usize = 0x1C000;

const RING_HEAD: usize = 0x00;
const RING_TAIL: usize = 0x40;
const RING_DATA: usize = 0x80;
const INPUT_RING_ENTRIES: u64 = 4096;
const EVENT_RING_ENTRIES: u64 = 512;
const MAX_ACTORS: usize = 256;
const ACTOR_HOSTILE: u32 = 1;
const ACTOR_DEAD: u32 = 1 << 1;
const ACTOR_IN_COMBAT: u32 = 1 << 3;
const EV_HIT_ACTOR: u32 = 1;
const EV_PLAYER_DIED: u32 = 2;
const IN_HURT: u16 = 7;
/// Stand-ins exist this far out (blocks), as SkyCraft's.
const ACTOR_RANGE_BLOCKS: f32 = 80.0;
const COL_DATA_BYTES: u64 = (COLLISION_RING_BYTES - RING_DATA) as u64;

const SKY_IN_GAME: u32 = 1;
const SKY_MENU_OPEN: u32 = 2;
const SKY_LOADING: u32 = 4;
const MC_IN_WORLD: u32 = 1;
const MC_SCREEN_OPEN: u32 = 1 << 1;
const MC_DEAD: u32 = 1 << 5;

const IN_KEY: u16 = 1;
const IN_MOUSE_BUTTON: u16 = 2;
const IN_SCROLL: u16 = 3;
const IN_CURSOR: u16 = 4;
const IN_TEXT: u16 = 5;
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

/// QueryPerformanceCounter and its frequency: the clock Minecraft stamps its physics ticks with.
#[cfg(windows)]
fn qpc() -> (i64, i64) {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    let (mut now, mut freq) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut now);
        QueryPerformanceFrequency(&mut freq);
    }
    (now, freq)
}
#[cfg(not(windows))]
fn qpc() -> (i64, i64) {
    (0, 0)
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

    /// Nothing of Minecraft's world is drawn yet: consume its render ring so it never stalls.
    /// One Minecraft event (32 bytes), oldest first.
    fn pop_event(&self) -> Option<[u8; 32]> {
        use std::sync::atomic::Ordering;
        let head = self.atomic_u64(OFF_EVENT_RING + RING_HEAD).load(Ordering::Acquire);
        let tail_a = self.atomic_u64(OFF_EVENT_RING + RING_TAIL);
        let mut tail = tail_a.load(Ordering::Relaxed);
        if tail >= head {
            return None;
        }
        if head - tail > EVENT_RING_ENTRIES {
            tail = head - EVENT_RING_ENTRIES;
        }
        let mut e = [0u8; 32];
        self.read_bytes(
            OFF_EVENT_RING + RING_DATA + ((tail & (EVENT_RING_ENTRIES - 1)) as usize) * 32,
            &mut e,
        );
        tail_a.store(tail + 1, Ordering::Release);
        Some(e)
    }

    /// The actor table (seqlock): WoW's units as Minecraft's hittable stand-ins.
    fn write_actors(&self, records: &[[u8; 64]]) {
        use std::sync::atomic::Ordering;
        let seq = self.atomic_u32(OFF_ACTOR_TABLE);
        let s = seq.load(Ordering::Relaxed) & !1;
        seq.store(s + 1, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        let n = records.len().min(MAX_ACTORS);
        self.write_u32(OFF_ACTOR_TABLE + 4, n as u32);
        for (i, r) in records.iter().take(n).enumerate() {
            self.write_bytes(OFF_ACTOR_TABLE + 0x40 + i * 64, r);
        }
        seq.store(s + 2, Ordering::Release);
    }

    fn drain_guest_rings(&self) {
        use std::sync::atomic::Ordering;
        for off in [OFF_RENDER_RING] {
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
    /// Hold mouselook (the camera's TurnOrAction channel) while Minecraft drives and the cursor
    /// is not wanted.
    pub(crate) mouselook: bool,
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
    /// The camera's yaw/pitch before the walk bob, recorded by [`minecraft_camera`]: what goes
    /// back to Minecraft, so the bob never feeds into the look (the wobble).
    look: Option<(f32, f32)>,
    /// Minecraft's player drives ours this frame.
    driving: bool,
    /// A Minecraft screen (inventory, chat, pause) is open.
    screen_open: bool,
    /// Escape gave the cursor back to WoW (its menus); a click in the world takes it again.
    esc_free: bool,
    buttons_down: Vec<u16>,
    scroll_carry: f32,
    cursor_sent: (i32, i32),
    /// The overlay triple buffer's front slot (ours), 2 after a reset.
    overlay_front: u32,
    overlay: Option<(Handle<Image>, UVec2)>,
    /// This frame's stand-ins: form id (the GUID's low half) to the unit.
    actors: HashMap<u32, (Entity, u64, u32, u32)>,
    /// Our own health last frame, for mirroring WoW's hits onto Minecraft's hearts.
    last_health: Option<u32>,
}

pub(crate) struct SkyCraftPlugin;

impl Plugin for SkyCraftPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ExternalPilot>();
        if !enabled() {
            return;
        }
        info!("skycraft: enabled (BENILLA_SKYCRAFT=1)");
        app.init_resource::<SkyHost>()
            .add_systems(
                Update,
                claim_input
                    .in_set(UiInput)
                    .after(UiKeyFeed)
                    .before(crate::bindings::BindingSet)
                    .in_set(InWorldGated),
            )
            .add_systems(Update, show_overlay.after(host_frame))
            .add_systems(Update, combat.after(host_frame).in_set(InWorldGated))
            .add_systems(
            Update,
            (host_frame, stream_collision)
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

/// SDL scancodes (USB HID usage ids), what SkyCraft's input ring carries.
fn sdl_scancode(key: KeyCode) -> Option<u16> {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [
        Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9, Digit0,
    ];
    const FKEYS: [KeyCode; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    if let Some(i) = LETTERS.iter().position(|k| *k == key) {
        return Some(4 + i as u16);
    }
    if let Some(i) = DIGITS.iter().position(|k| *k == key) {
        return Some(30 + i as u16);
    }
    if let Some(i) = FKEYS.iter().position(|k| *k == key) {
        return Some(58 + i as u16);
    }
    Some(match key {
        Enter | NumpadEnter => 40,
        Escape => 41,
        Backspace => 42,
        Tab => 43,
        Space => 44,
        Minus => 45,
        Equal => 46,
        BracketLeft => 47,
        BracketRight => 48,
        Backslash => 49,
        Semicolon => 51,
        Quote => 52,
        Backquote => 53,
        Comma => 54,
        Period => 55,
        Slash => 56,
        CapsLock => 57,
        Insert => 73,
        Home => 74,
        PageUp => 75,
        Delete => 76,
        End => 77,
        PageDown => 78,
        ArrowRight => 79,
        ArrowLeft => 80,
        ArrowDown => 81,
        ArrowUp => 82,
        ControlLeft => 224,
        ShiftLeft => 225,
        ControlRight => 228,
        ShiftRight => 229,
        _ => return None,
    })
}

/// Keys Minecraft drives the player with: mirrored, and WoW still sees them (its move flags).
fn shared_key(key: KeyCode) -> bool {
    matches!(
        key,
        KeyCode::KeyW
            | KeyCode::KeyA
            | KeyCode::KeyS
            | KeyCode::KeyD
            | KeyCode::Space
            | KeyCode::ShiftLeft
            | KeyCode::ControlLeft
    )
}

/// Keys that are Minecraft's alone while it drives: the hotbar, inventory, drop, swap hands, its
/// chat and commands, its camera (F5). WoW's bindings never see them.
fn minecraft_key(key: KeyCode) -> bool {
    use KeyCode::*;
    matches!(
        key,
        Digit1
            | Digit2
            | Digit3
            | Digit4
            | Digit5
            | Digit6
            | Digit7
            | Digit8
            | Digit9
            | KeyE
            | KeyQ
            | KeyF
            | KeyT
            | Slash
            | F5
    )
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
    host.driving = false;
    host.screen_open = false;
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
        if new_guest {
            // A fresh writer starts its triple buffer over (SkyCraft's ResetOverlay).
            link.atomic_u32(OFF_OVERLAY_CTL)
                .store(0, std::sync::atomic::Ordering::Release);
            host.overlay_front = 2;
        }
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
    let (yaw, pitch) = host.look.take().map(Ok).unwrap_or_else(|| camera
        .single()
        .map(|t| {
            let f = t.forward().as_vec3();
            (
                (-f.x).atan2(f.z).to_degrees(),
                (-f.y).clamp(-1.0, 1.0).asin().to_degrees(),
            )
        }))
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
    let f32_at = |o: usize| f32::from_le_bytes(mc[o..o + 4].try_into().unwrap());
    let mut feet = from_mc(f64_at(&mc, 0x08), f64_at(&mc, 0x10), f64_at(&mc, 0x18));
    let mut eye = from_mc(f64_at(&mc, 0x50), f64_at(&mc, 0x58), f64_at(&mc, 0x60));
    let mut bob_phase = f32_at(0x44);
    let mut bob_amount = f32_at(0x48);
    // Interpolate Minecraft's 20 Hz physics ticks on our own frame clock, as Minecraft's renderer
    // does with its partial tick: the interpolated fields above were taken at Minecraft's frame,
    // whose phase against ours wanders (the judder).
    let tick_qpc = i64::from_le_bytes(mc[0x68..0x70].try_into().unwrap());
    let tick_ms_mc = f32_at(0xB8);
    let (now_qpc, freq) = qpc();
    if tick_qpc > 0 && freq > 0 && tick_ms_mc > 1.0 && f64_at(&mc, 0x88) != 0.0 {
        let elapsed_ms = (now_qpc - tick_qpc) as f64 * 1000.0 / freq as f64;
        let a = (elapsed_ms / tick_ms_mc as f64).clamp(0.0, 1.0);
        let lerp = |o: usize| f64_at(&mc, o) + (f64_at(&mc, o + 0x18) - f64_at(&mc, o)) * a;
        feet = from_mc(lerp(0x70), lerp(0x78), lerp(0x80));
        let af = a as f32;
        let eye_h = f32_at(0xA0) + (f32_at(0xA4) - f32_at(0xA0)) * af;
        // Only in first person does the camera sit at the eye; keep Minecraft's own otherwise.
        if u32_at(&mc, 0xC0) == 0 {
            eye = feet + Vec3::Y * eye_h * yards_per_block();
        }
        let (walk_o, walk) = (f32_at(0xA8), f32_at(0xAC));
        bob_phase = walk + (walk - walk_o) * af;
        bob_amount = f32_at(0xB0) + (f32_at(0xB4) - f32_at(0xB0)) * af;
    }
    let in_world = mc_flags & MC_IN_WORLD != 0;
    host.screen_open = in_world && mc_flags & MC_SCREEN_OPEN != 0;
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
        host.driving = true;
        pilot.follow = Some(McPlayer {
            feet,
            eye,
            on_ground: mc_flags & (1 << 2) != 0,
            sprinting: mc_flags & (1 << 4) != 0,
            fov_deg: f32_at(0x40),
            bob_phase,
            bob_amount,
        });
    }
}

/// Minecraft's input while it drives, claimed between the UI's key feed and WoW's bindings:
/// the movement keys are mirrored (WoW keeps them for its move flags), Minecraft's own keys
/// (hotbar, inventory, drop, ...) and, with a Minecraft screen up, every key go to Minecraft only.
/// The mouse buttons and wheel are Minecraft's while the look is held or a screen is open, and the
/// cursor position while a screen is open. Holding Alt, or Escape (WoW's menu), frees the cursor
/// for WoW; a click in the world takes it back.
#[allow(clippy::too_many_arguments)]
fn claim_input(
    mut host: ResMut<SkyHost>,
    mut pilot: ResMut<ExternalPilot>,
    mut keyboard: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut scroll: ResMut<AccumulatedMouseScroll>,
    mut capture: ResMut<UiKeyboardCapture>,
    over_ui: Res<PointerOverUi>,
    window: Query<&Window, With<PrimaryWindow>>,
) {
    let Some(link) = host.link else {
        keyboard.clear();
        pilot.mouselook = false;
        return;
    };
    let typing = capture.typing;
    let screen = host.screen_open && host.driving;
    let mut down = std::mem::take(&mut host.keys_down);
    if !host.driving || (typing && !host.typing_was) {
        if !down.is_empty() || !host.buttons_down.is_empty() {
            link.push_input(IN_RELEASE_ALL, 0, 0, 0, 0);
        }
        down.clear();
        host.buttons_down.clear();
    }
    host.typing_was = typing;
    if !host.driving {
        keyboard.clear();
        host.keys_down = down;
        pilot.mouselook = false;
        return;
    }

    // ---- keys ----
    for ev in keyboard.read() {
        let pressed = ev.state == ButtonState::Pressed;
        if typing && pressed {
            continue;
        }
        if pressed && !screen && ev.key_code == KeyCode::Escape {
            // WoW's Escape (its menu ladder) runs; the cursor is WoW's until a world click.
            host.esc_free = !host.esc_free;
            continue;
        }
        let ours = screen || minecraft_key(ev.key_code);
        if !ours && !shared_key(ev.key_code) {
            continue;
        }
        if ours && pressed {
            capture.consumed.push(ev.key_code);
        }
        if screen && pressed {
            if let Some(text) = &ev.text {
                for ch in text.chars().filter(|c| !c.is_control()) {
                    link.push_input(IN_TEXT, 0, ch as i32, 0, 0);
                }
            }
        }
        let Some(code) = sdl_scancode(ev.key_code) else {
            continue;
        };
        let was = down.contains(&code);
        if pressed && !was {
            down.push(code);
            link.push_input(IN_KEY, code, 1, 0, 0);
        } else if !pressed && was {
            down.retain(|c| *c != code);
            link.push_input(IN_KEY, code, 0, 0, 0);
        }
    }
    host.keys_down = down;

    // ---- the cursor's owner ----
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if host.esc_free && !alt && !over_ui.0 && buttons.just_pressed(MouseButton::Left) {
        host.esc_free = false;
        // This click only takes the cursor back.
        pilot.mouselook = true;
        return;
    }
    let look = !screen && !typing && !alt && !host.esc_free;
    pilot.mouselook = look;
    let route = look || screen;

    // ---- mouse buttons ----
    for (button, sdl) in [
        (MouseButton::Left, 1u16),
        (MouseButton::Middle, 2),
        (MouseButton::Right, 3),
    ] {
        let held = host.buttons_down.contains(&sdl);
        if route && buttons.just_pressed(button) && !held {
            host.buttons_down.push(sdl);
            link.push_input(IN_MOUSE_BUTTON, sdl, 1, 0, 0);
        } else if held && (!buttons.pressed(button) || !route) {
            host.buttons_down.retain(|b| *b != sdl);
            link.push_input(IN_MOUSE_BUTTON, sdl, 0, 0, 0);
        }
    }

    // ---- wheel: Minecraft's hotbar instead of WoW's zoom ----
    if route && scroll.delta.y != 0.0 {
        let notches = match scroll.unit {
            MouseScrollUnit::Line => scroll.delta.y,
            MouseScrollUnit::Pixel => scroll.delta.y / 100.0,
        };
        host.scroll_carry += notches;
        while host.scroll_carry >= 1.0 {
            host.scroll_carry -= 1.0;
            link.push_input(IN_SCROLL, 0, 120, 0, 0);
        }
        while host.scroll_carry <= -1.0 {
            host.scroll_carry += 1.0;
            link.push_input(IN_SCROLL, 0, -120, 0, 0);
        }
        scroll.delta = Vec2::ZERO;
    }

    // ---- the cursor, in overlay pixels, while a screen is open ----
    if screen {
        if let Ok(w) = window.single() {
            if let Some(p) = w.physical_cursor_position() {
                let (ow, oh) = host
                    .overlay
                    .as_ref()
                    .map(|(_, s)| (s.x as f32, s.y as f32))
                    .unwrap_or((w.physical_width() as f32, w.physical_height() as f32));
                let x = (p.x * ow / w.physical_width().max(1) as f32) as i32;
                let y = (p.y * oh / w.physical_height().max(1) as f32) as i32;
                if (x, y) != host.cursor_sent {
                    host.cursor_sent = (x, y);
                    link.push_input(IN_CURSOR, 0, x, y, 0);
                }
            }
        }
    }
}

// ---- combat ------------------------------------------------------------------------------------

/// Minecraft's weapons against WoW's units, and WoW's hits on Minecraft's hearts.
///
/// Nearby units go to Minecraft as invisible hittable stand-ins (the actor table). A hit Minecraft
/// reports lands on the server as the GM `.damage` command on that unit (the account is GM; the
/// server's own damage path gives the kill credit, loot and XP). Damage we take in WoW is passed to
/// Minecraft as a fraction of our health against its 20.
#[allow(clippy::too_many_arguments)]
fn combat(
    mut host: ResMut<SkyHost>,
    units: Query<
        (
            Entity,
            &crate::net::NetEntity,
            &crate::net::Guid,
            &GlobalTransform,
            Option<&crate::net::ObjectStore>,
            Option<&crate::entities::CollisionHeight>,
        ),
        Without<crate::net::SelfPlayer>,
    >,
    me: Query<(&crate::net::ObjectStore, &crate::net::Guid), With<crate::net::SelfPlayer>>,
    reactions: crate::target::ReactionInputs,
    names: Res<crate::names::NameCache>,
    net: Res<crate::net::NetCommands>,
    mut selection: ResMut<crate::target::Selection>,
    player: Res<Player>,
) {
    let Some(link) = host.link else {
        return;
    };
    if !host.driving {
        while link.pop_event().is_some() {}
        if !host.actors.is_empty() {
            host.actors.clear();
            link.write_actors(&[]);
        }
        host.last_health = None;
        return;
    }
    let me = me.single().ok();
    let self_store = me.map(|(s, _)| s);
    let self_guid = me.map(|(_, g)| g.0);
    let factions = reactions.factions.as_deref();
    let ypb = yards_per_block();

    // ---- the stand-ins ----
    let mut records: Vec<[u8; 64]> = Vec::new();
    let mut actors = HashMap::new();
    let mut attacker = 0u32;
    let mut attacker_d = f32::MAX;
    for (entity, ne, guid, tf, store, height) in &units {
        if !matches!(ne.kind, benilla_protocol::EntityKind::Unit) {
            continue;
        }
        let pos = tf.translation();
        let d = pos.distance(player.pos);
        if d > ACTOR_RANGE_BLOCKS * ypb || records.len() >= MAX_ACTORS {
            continue;
        }
        let Some(store) = store else {
            continue;
        };
        let f = &store.0;
        let form = (guid.0 & 0xFFFF_FFFF) as u32;
        let mut flags = 0;
        if crate::target::can_attack(Some(store), factions, &reactions.reputations, self_store) {
            flags |= ACTOR_HOSTILE;
        }
        if f.unit_reads_dead() {
            flags |= ACTOR_DEAD;
        }
        if f.unit_flags() & (1 << 19) != 0 {
            flags |= ACTOR_IN_COMBAT;
        }
        if flags & ACTOR_DEAD == 0 && f.unit_target().is_some() && f.unit_target() == self_guid
            && flags & ACTOR_IN_COMBAT != 0 && d < attacker_d
        {
            attacker = form;
            attacker_d = d;
        }
        let level = f.unit_level().unwrap_or(1);
        let mc = to_mc(pos);
        let fwd = tf.rotation() * Vec3::NEG_Z;
        let yaw = (-fwd.x).atan2(fwd.z).to_degrees();
        let scale = ne.scale.max(0.1);
        let radius = f.unit_bounding_radius().max(0.4) * scale;
        let width = (radius * 2.0 / ypb).clamp(0.3, 6.0);
        let tall = height.map(|h| h.0).filter(|h| *h > 0.1).unwrap_or(2.0 * scale);
        let tall = (tall / ypb).clamp(0.3, 12.0);
        let max = f.unit_max_health().unwrap_or(0);
        let frac = if max > 0 {
            (f.unit_health().unwrap_or(0) as f32 / max as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut r = [0u8; 64];
        r[0..4].copy_from_slice(&form.to_le_bytes());
        r[4..8].copy_from_slice(&flags.to_le_bytes());
        r[8..12].copy_from_slice(&(mc[0] as f32).to_le_bytes());
        r[12..16].copy_from_slice(&(mc[1] as f32).to_le_bytes());
        r[16..20].copy_from_slice(&(mc[2] as f32).to_le_bytes());
        r[20..24].copy_from_slice(&yaw.to_le_bytes());
        r[24..28].copy_from_slice(&width.to_le_bytes());
        r[28..32].copy_from_slice(&tall.to_le_bytes());
        r[32..36].copy_from_slice(&frac.to_le_bytes());
        r[36..38].copy_from_slice(&(level.min(u16::MAX as u32) as u16).to_le_bytes());
        if let Some(name) = names.peek_unit(guid.0, Some(store)) {
            let mut end = name.len().min(23);
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            r[40..40 + end].copy_from_slice(&name.as_bytes()[..end]);
        }
        records.push(r);
        actors.insert(form, (entity, guid.0, level, flags));
    }
    link.write_actors(&records);
    host.actors = actors;

    // ---- Minecraft's hits on them ----
    while let Some(ev) = link.pop_event() {
        let kind = u32_at(&ev, 0);
        let form = u32_at(&ev, 4);
        let mc_damage = f32::from_le_bytes(ev[8..12].try_into().unwrap());
        match kind {
            EV_HIT_ACTOR => {
                let Some(&(entity, guid, level, flags)) = host.actors.get(&form) else {
                    continue;
                };
                // Only what WoW lets us attack: no killing quest givers or guards of our faction.
                if flags & ACTOR_HOSTILE == 0 || flags & ACTOR_DEAD != 0 {
                    continue;
                }
                // Minecraft's damage (a diamond sword hits for 7) against WoW's health, which
                // grows with level: a few hits for a mob of your own level, as in Minecraft.
                let damage = (mc_damage * (2.0 + level as f32)).round().max(1.0) as u32;
                if selection.guid != Some(guid) {
                    selection.last = selection.guid;
                    selection.target = Some(entity);
                    selection.guid = Some(guid);
                }
                let _ = net.0.send(crate::net::ClientCommand::SetSelection { guid });
                let _ = net.0.send(crate::net::ClientCommand::Chat {
                    kind: crate::net::ChatKind::Say,
                    target: None,
                    text: format!(".damage {damage}"),
                    language: None,
                });
                info!("skycraft: Minecraft hit {guid:#x} for {mc_damage:.1} -> .damage {damage}");
            }
            EV_PLAYER_DIED => info!("skycraft: the Minecraft player died"),
            _ => {}
        }
    }

    // ---- WoW's hits on us ----
    if let Some(store) = self_store {
        let hp = store.0.unit_health().unwrap_or(0);
        let max = store.0.unit_max_health().unwrap_or(0);
        if let Some(last) = host.last_health {
            if hp < last && last > 0 && max > 0 {
                let mc = (last - hp) as f32 / max as f32 * 20.0;
                // SkyCraft's hurt input is in Skyrim damage x100; the mod divides by 5.
                let kind = if attacker != 0 { 0 } else { 3 };
                link.push_input(IN_HURT, kind, (mc * 5.0 * 100.0) as i32, attacker as i32, 0);
                info!("skycraft: WoW hit us for {} -> {mc:.1} Minecraft", last - hp);
            }
        }
        host.last_health = Some(hp);
    }
}

// ---- overlay ------------------------------------------------------------------------------------

#[derive(Component)]
struct McOverlay;

/// Minecraft's HUD, hand and screens, drawn over the world: the newest frame of SkyCraft's overlay
/// triple buffer, copied into a full-window UI image. Minecraft's pixels are premultiplied; Bevy's
/// UI blends straight alpha, so they are divided back out.
fn show_overlay(
    mut commands: Commands,
    mut host: ResMut<SkyHost>,
    mut images: ResMut<Assets<Image>>,
    mut nodes: Query<(&mut Visibility, &mut ImageNode), With<McOverlay>>,
) {
    use std::sync::atomic::Ordering;
    let visible = host.driving;
    for (mut v, _) in &mut nodes {
        *v = if visible { Visibility::Inherited } else { Visibility::Hidden };
    }
    let Some(link) = host.link else {
        return;
    };
    if !visible {
        return;
    }
    let ctl = link.atomic_u32(OFF_OVERLAY_CTL);
    if ctl.load(Ordering::Acquire) & OVERLAY_DIRTY == 0 {
        return;
    }
    let old = ctl.swap(host.overlay_front, Ordering::AcqRel);
    host.overlay_front = old & 3;
    let slot = host.overlay_front as usize;
    if slot > 2 {
        return;
    }
    let hdr = OFF_OVERLAY_SLOT_HDR + slot * 0x40;
    let w = link.atomic_u32(hdr).load(Ordering::Relaxed);
    let h = link.atomic_u32(hdr + 4).load(Ordering::Relaxed);
    let bottom_up = link.atomic_u32(hdr + 8).load(Ordering::Relaxed) & 1 != 0;
    if w == 0 || h == 0 || w > 3840 || h > 2160 {
        return;
    }
    let size = UVec2::new(w, h);
    if host.overlay.as_ref().map(|(_, s)| *s) != Some(size) {
        let image = Image::new_fill(
            Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[0, 0, 0, 0],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        let handle = images.add(image);
        if nodes.is_empty() {
            commands.spawn((
                McOverlay,
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                ImageNode::new(handle.clone()),
                GlobalZIndex(5),
            ));
        } else {
            for (_, mut node) in &mut nodes {
                node.image = handle.clone();
            }
        }
        if let Some((old, _)) = host.overlay.take() {
            images.remove(&old);
        }
        info!("skycraft: overlay {w}x{h}");
        host.overlay = Some((handle, size));
    }
    let Some((handle, _)) = host.overlay.as_ref() else {
        return;
    };
    let Some(mut image) = images.get_mut(handle) else {
        return;
    };
    let Some(data) = image.data.as_mut() else {
        return;
    };
    let row = w as usize * 4;
    let src = OFF_OVERLAY_PIXELS + slot * OVERLAY_SLOT_BYTES;
    for y in 0..h as usize {
        let sy = if bottom_up { h as usize - 1 - y } else { y };
        let dst = &mut data[y * row..(y + 1) * row];
        link.read_bytes(src + sy * row, dst);
        for px in dst.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a == 0 {
                px[0] = 0;
                px[1] = 0;
                px[2] = 0;
            } else if a < 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
    }
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
    // The look before the bob is what goes back to Minecraft next frame.
    let f = t.forward().as_vec3();
    host.look = Some((
        (-f.x).atan2(f.z).to_degrees(),
        (-f.y).clamp(-1.0, 1.0).asin().to_degrees(),
    ));
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

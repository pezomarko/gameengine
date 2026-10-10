//! Windowed client: winit event loop, fixed 64 Hz simulation with interpolated rendering,
//! mouse look, PVS-culled drawing, optional benchmark mode. Offline it runs the local
//! simulation; with `--connect` it predicts the own entity, reconciles against the zone and
//! interpolates everyone else (PROTOCOL.md 7). Two viewports share the simulation
//! (VOCABULARY.md 9): first person aims from the eyes, third person aims the camera ray at a
//! world point and re-aims it from the eyes ("camera-to-muzzle re-aim").

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use web_time::Instant;

use glam::{Mat4, Vec2, Vec3};
use gm_bsp::Bsp;
use gm_core::build::{ContentPack, Sheet};
use gm_core::collide::{Aabb, sweep_boxes};
use gm_core::movement::{MoveInput, MoveVars, PlayerState, player_move, yaw_vectors};
use gm_core::sim::{Input as SimInput, buttons, view_dir};
use gm_core::tick::TickRate;
use gm_core::trace::{CollisionWorld, Hull};
use gm_hub_proto::names;
use gm_hub_proto::player::PlayerRequest;
use gm_model::ModelId;
use gm_net::client::ClientState;
use gm_net::control::{
    BodyKind, BuildChoice, EncounterState, FromClient, FromZone, GmNews, Look, Order, SquadEntry,
    StallEntry,
};
use gm_net::snapshot::{EntityKind, SpawnInfo};
use gm_net::transport::fnv1a64;
use winit::application::ApplicationHandler;
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
#[cfg(not(target_arch = "wasm32"))]
use winit::window::CursorGrabMode;
use winit::window::{CursorIcon, Window, WindowId};

use crate::avatars::{Avatars, Body, DOLL, OWN, stall_boxes, stall_keeper};
use crate::bag::{Bag, BagAction};
use crate::character::{CharacterAction, CharacterPage, CharacterView};
use crate::characters::CharacterDraw;
use crate::content::Content;
use crate::front::{Action, Auto, Front, PANEL_UNITS};
use crate::gm::{GmAction, GmPage, GmView};
use crate::hub::{Account, Hub, HubApi, ticket_addr};
use crate::hud::{self, Hud};
use crate::menu::{Chat, GameMenu, MenuAction, Offers, Said};
use crate::net::{NetClient, NetEvent, ZoneAddr};
use crate::people::{Here, People, PeopleAction, Social};
use crate::render::{EntityDraw, Gpu, Renderer, view_proj};
use crate::script::UiScript;
use crate::settings::Settings;
use crate::stats::FrameStats;
#[cfg(not(target_arch = "wasm32"))]
use crate::stats::{print_bench, print_bench_avatars};
use crate::touch::{self, Button as TouchButton, Event as TouchEvent, Fingers, Zone};
use crate::ui::{self, Key, Ui, UiInput, UiState};
use crate::world::{self, WorldMesh};
use crate::{Error, Options};
use gm_net::control::TRAINER_REACH;

const MAX_STEPS_PER_FRAME: u32 = 8;
/// Behind a screen with no zone being played, the camera turns this fast (CLIENT.md 2).
const BACKDROP_DEG_PER_S: f32 = 4.0;
/// How far the selector's camera stands from the body it shows whole (CLIENT.md 4.2).
const DOLL_OPEN_DISTANCE: f32 = 46.0;
/// While a zone is played the hub is asked something this often, so that the session is
/// still there when the zone is left (it ends after a day of silence).
const SESSION_TOUCH_SECS: u64 = 600;
/// Two presses this close in time and place are a double click.
const DOUBLE_CLICK_SECS: f32 = 0.4;
const DOUBLE_CLICK_PIXELS: f32 = 6.0;
const BENCH_YAW_DEG_PER_S: f32 = 20.0;
/// With a crowd the bench camera swings across it instead of turning away from it.
const BENCH_CROWD_SWING_DEG: f32 = 22.0;
/// Third-person camera: behind, slightly right and above the eyes (VOCABULARY.md 9).
/// A blow within this many seconds of the last keeps the combo counter going (MODES.md 4.6).
const COMBO_SECS: f32 = 2.0;
/// The recoil's punch on the view falls to a third in this long (MODES.md 3.3).
const PUNCH_DECAY_SECS: f32 = 0.06;
const CAMERA_BACK: f32 = 110.0;
const CAMERA_RIGHT: f32 = 24.0;
const CAMERA_UP: f32 = 12.0;
/// How far the camera ray is resolved for the re-aim.
const AIM_REACH: f32 = 4096.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewport {
    First,
    Third,
}

/// Offline simulation: the local player against the map, no server.
pub struct Sim {
    pub rate: TickRate,
    pub accumulator: f32,
    pub prev: PlayerState,
    pub curr: PlayerState,
    pub yaw: f32,
    pub pitch: f32,
    pub vars: MoveVars,
}

impl Sim {
    pub fn new(bsp: &Bsp) -> Sim {
        Sim::at(bsp, None)
    }

    /// Start at `start` (`x, y, z, yaw`), or at the map's start.
    pub fn at(bsp: &Bsp, start: Option<[f32; 4]>) -> Sim {
        let (origin, yaw) = match start {
            Some([x, y, z, yaw]) => (Vec3::new(x, y, z), yaw),
            None => bsp
                .player_start()
                .unwrap_or((Vec3::new(0.0, 0.0, 64.0), 0.0)),
        };
        let st = PlayerState::new(origin);
        Sim {
            rate: TickRate::COMBAT,
            accumulator: 0.0,
            prev: st,
            curr: st,
            yaw,
            pitch: 0.0,
            vars: MoveVars::QUAKE,
        }
    }

    /// Advance the simulation by `frame_dt` seconds of wall time in fixed ticks. Returns the
    /// number of ticks run.
    pub fn advance(&mut self, bsp: &Bsp, input: &MoveInput, frame_dt: f32) -> u32 {
        let dt = self.rate.dt();
        self.accumulator += frame_dt.min(0.25);
        let mut steps = 0;
        while self.accumulator >= dt && steps < MAX_STEPS_PER_FRAME {
            self.prev = self.curr;
            player_move(bsp, &self.vars, &mut self.curr, input, dt);
            self.accumulator -= dt;
            steps += 1;
        }
        if steps == MAX_STEPS_PER_FRAME {
            self.accumulator = 0.0;
        }
        steps
    }

    /// Eye position interpolated between the last two ticks.
    pub fn eye(&self) -> Vec3 {
        self.prev.origin.lerp(self.curr.origin, self.alpha())
            + Vec3::new(0.0, 0.0, self.curr.hull.eye_height())
    }

    pub fn origin(&self) -> Vec3 {
        self.prev.origin.lerp(self.curr.origin, self.alpha())
    }

    fn alpha(&self) -> f32 {
        (self.accumulator / self.rate.dt()).clamp(0.0, 1.0)
    }
}

/// Networked play: the zone connection plus the shared prediction state.
pub(crate) struct Online {
    net: NetClient,
    /// `Welcome` arrived; the client state is built when `Content` follows.
    welcome: Option<(u32, u16)>,
    client: Option<ClientState>,
    pack: Option<ContentPack>,
    team: u8,
    build_name: String,
    accumulator: f32,
    prev_origin: Vec3,
    curr_origin: Vec3,
    last_snapshot: Instant,
    rate: TickRate,
    /// Everyone in the zone: name, team, avatar model.
    names: HashMap<u32, (String, u8, Option<ModelId>)>,
    /// What each body holds (LOOK.md 6.2), by the pack's prop list.
    looks: HashMap<u32, Look>,
    /// The prop keys the zone's content names: what a `Look` indexes.
    props: Vec<String>,
    /// Who drives each body (COMPANIONS.md 2.1), as the zone announced it.
    kinds: HashMap<u32, BodyKind>,
    /// The own squad, in slot order, as the zone last told it.
    squad: Vec<SquadEntry>,
    /// Encounter, loot, trial and order messages: when, what, in which colour.
    messages: VecDeque<(Instant, String, [f32; 4])>,
    /// The market: open stalls and their keepers (ECONOMY.md 7).
    stalls: Vec<StallEntry>,
    /// The zone granted the game master's page (GM.md 1), and its tuning as last told.
    gm: bool,
    tuning: gm_core::tuning::Tuning,
    gm_note: String,
    /// The party as the hub has it, and who asked what (PARTY.md 4).
    social: Social,
    kills: u32,
    deaths: u32,
    /// Blows the own hand landed, as the zone said (target, amount, absorbed): the
    /// numbers the next frame floats over the bodies it finds (LOOK.md 13.8).
    /// And what the own hand's Regen gave other bodies back (target, amount).
    heals: Vec<(u32, u16)>,
    /// The hash of the map this connection plays on: the one loaded when it began, and
    /// from the zone's `Welcome` on the zone's (once that map is here).
    map_hash: u64,
    respec_note: String,
    /// A travel ticket to act on: reconnect to another zone, reloading its map.
    pending_travel: Option<(String, ZoneAddr, Vec<u8>)>,
    zone_name: String,
    /// Events taken from the connection and not handled yet: while the zone's map is
    /// still on its way (a browser fetches it), everything after `Welcome` waits here.
    backlog: VecDeque<NetEvent>,
    /// The zone's map is being fetched (a browser): its hash. It is the connection's
    /// map only once it has arrived.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    awaiting_map: Option<u64>,
}

/// How long a message stays on the HUD, and how many are kept.
const MESSAGE_SECS: f32 = 9.0;
const MESSAGES_KEPT: usize = 6;

impl Online {
    fn say(&mut self, text: String, colour: [f32; 4]) {
        log::info!("{text}");
        self.messages.push_back((Instant::now(), text, colour));
        while self.messages.len() > MESSAGES_KEPT {
            self.messages.pop_front();
        }
    }

    fn build_name_of(&self, pack: &ContentPack, build: &gm_core::build::Build) -> String {
        pack.builds
            .iter()
            .find(|b| &b.build == build)
            .map_or_else(|| "custom".to_string(), |b| b.name.clone())
    }
}

#[derive(Default)]
struct Input {
    keys: HashSet<KeyCode>,
    /// Keys pressed since the last simulation tick (consumed by the next tick).
    just_pressed: HashSet<KeyCode>,
    mouse: HashSet<MouseButton>,
    mouse_dx: f32,
    mouse_dy: f32,
    /// A finger's stick (MODES.md 5.6): forward and side, while one holds it; the keys
    /// are taken first.
    stick: Option<(f32, f32)>,
    /// The gun mode (MODES.md 3.7): the weapon in hand (0 the gun, 1 the pistol, 2 the
    /// knife) and whether the scope is up (the secondary button toggles it).
    held: u8,
    scoped: bool,
}

impl Input {
    fn down(&self, k: KeyCode) -> bool {
        self.keys.contains(&k)
    }

    fn axes(&self) -> (f32, f32) {
        let axis = |neg, pos| (self.down(pos) as i32 - self.down(neg) as i32) as f32;
        let keys = (
            axis(KeyCode::KeyS, KeyCode::KeyW) + axis(KeyCode::ArrowDown, KeyCode::ArrowUp),
            axis(KeyCode::KeyA, KeyCode::KeyD) + axis(KeyCode::ArrowLeft, KeyCode::ArrowRight),
        );
        match self.stick {
            Some(stick) if keys == (0.0, 0.0) => stick,
            _ => keys,
        }
    }

    fn move_input(&self, yaw: f32) -> MoveInput {
        let (forward, side) = self.axes();
        MoveInput {
            yaw,
            forward,
            side,
            jump: self.down(KeyCode::Space),
        }
    }

    /// The frame's input. `dodge` is the active slot (1-based) Space plays instead of a
    /// jump (MODES.md 4.6): the kit's dash, while it is ready. In the gun mode (MODES.md
    /// 3.7) `1 2 3` take the gun, the pistol and the knife in hand, `4`–`7` are the
    /// actives, Ctrl crouches, Shift walks, `R` reloads and the secondary button is the
    /// scope.
    fn sim_input(
        &mut self,
        yaw: f32,
        pitch: f32,
        dodge: Option<u8>,
        gun: bool,
        rpg: Option<crate::rpg::RpgFrame>,
    ) -> SimInput {
        // The RPG mode (MODES.md 5.5): `1` the primary, `2` the secondary, `3`–`6` the
        // actives, Shift guards, Space jumps; the axes are the walk's when none is held.
        if let Some(r) = rpg {
            let mut b = r.buttons;
            if self.down(KeyCode::Space) {
                b |= buttons::JUMP;
            }
            if self.down(KeyCode::ShiftLeft) || self.down(KeyCode::ShiftRight) {
                b |= buttons::GUARD;
            }
            self.just_pressed.clear();
            return SimInput {
                buttons: b,
                yaw,
                pitch,
                forward: r.forward,
                side: r.side,
                ability: r.ability,
                held: 0,
                target: r.target,
            };
        }
        let (mut forward, mut side) = self.axes();
        let mut b = 0u16;
        let dodged = dodge.is_some() && self.just_pressed.contains(&KeyCode::Space);
        if self.down(KeyCode::Space) && dodge.is_none() {
            b |= buttons::JUMP;
        }
        if self.just_pressed.contains(&KeyCode::KeyR) {
            b |= buttons::RELOAD;
        }
        if self.just_pressed.contains(&KeyCode::KeyF) {
            b |= buttons::USE;
        }
        if self.mouse.contains(&MouseButton::Left) {
            b |= buttons::PRIMARY;
        }
        let ctrl = self.down(KeyCode::ControlLeft)
            || self.down(KeyCode::ControlRight)
            || self.down(KeyCode::KeyC);
        let shift = self.down(KeyCode::ShiftLeft) || self.down(KeyCode::ShiftRight);
        let mut ability;
        if gun {
            for (i, k) in [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3]
                .iter()
                .enumerate()
            {
                if self.just_pressed.contains(k) {
                    self.held = i as u8;
                    self.scoped = false;
                }
            }
            ability = [
                KeyCode::Digit4,
                KeyCode::Digit5,
                KeyCode::Digit6,
                KeyCode::Digit7,
            ]
            .iter()
            .position(|k| self.just_pressed.contains(k))
            .map_or(0, |i| i as u8 + 1);
            if ctrl {
                b |= buttons::CROUCH;
            }
            if shift {
                forward *= 0.5;
                side *= 0.5;
            }
            if self.scoped {
                b |= buttons::SCOPE;
            }
        } else {
            if self.mouse.contains(&MouseButton::Right) {
                b |= buttons::SECONDARY;
            }
            // Guard is on C as well: a browser keeps Ctrl+W for itself (WEB.md 3.4).
            if ctrl {
                b |= buttons::GUARD;
            }
            if shift {
                b |= buttons::ABILITY1;
            }
            ability = [
                KeyCode::Digit1,
                KeyCode::Digit2,
                KeyCode::Digit3,
                KeyCode::Digit4,
            ]
            .iter()
            .position(|k| self.just_pressed.contains(k))
            .map_or(0, |i| i as u8 + 1);
        }
        if dodged && let Some(d) = dodge {
            ability = d;
        }
        self.just_pressed.clear();
        SimInput {
            buttons: b,
            yaw,
            pitch,
            forward,
            side,
            ability,
            held: if gun { self.held } else { 0 },
            target: 0,
        }
    }
}

struct Active {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// The format frames are drawn in: the surface's, or its sRGB twin as a view format
    /// where the surface itself is not sRGB (a WebGPU canvas).
    view_format: wgpu::TextureFormat,
    gpu: Gpu,
    renderer: Renderer,
    avatars: Avatars,
    /// The leaves the visible faces were last gathered from.
    drawn_from: Vec<usize>,
}

/// The view model to draw this frame (LOOK.md 6.4), decided by the online frame and
/// pushed once the avatars' frame has begun.
#[derive(Clone, Copy)]
struct ViewModel {
    slot: usize,
    /// The template's `fit_view` for the prop (LOOK.md 6.4), identity without one.
    fit: Mat4,
    eye: Vec3,
    yaw: f32,
    pitch: f32,
    stride: f32,
    kick: f32,
    /// −1 drawn back in a windup, +1 across at the end of a cut, 0 at rest.
    swing: f32,
    /// How far along the reload in hand is, 0 when none (MODES.md 3.2).
    reload: f32,
    light: [f32; 3],
}

struct App {
    opts: Options,
    /// The view model's kick (1 at a launch, decaying) and stride phase (LOOK.md 6.4).
    view_kick: f32,
    view_swing: f32,
    view_stride: f32,
    view_model: Option<ViewModel>,
    /// The content bundle: the manifest, the props by key (CONTENT.md 6).
    content: Content,
    /// `--prop FILE` or `--prop KEY` offline: the own body holds it (the fitting room of
    /// CONTENT.md 9), once it is on the GPU.
    offline_prop: Option<usize>,
    /// Offline (`--off KEY`): the prop in the own body's left hand, to look at.
    offline_off: Option<usize>,
    /// The window, from its creation: the renderer on it (`active`) may come later.
    window: Option<Arc<Window>>,
    bsp: Bsp,
    mesh: Option<WorldMesh>,
    active: Option<Active>,
    sim: Sim,
    online: Option<Online>,
    input: Input,
    viewport: Viewport,
    stats: FrameStats,
    last_frame: Instant,
    last_title: Instant,
    started: Instant,
    title_frame: usize,
    grabbed: bool,
    adapter_info: Option<wgpu::AdapterInfo>,
    faces_total: usize,
    exit_requested: bool,
    acquire_timeouts: u32,
    entities: Vec<EntityDraw>,
    /// Bodies to draw this frame (players, the own body in third person).
    bodies: Vec<Body>,
    /// Models the zone revoked since the last frame.
    revoked: Vec<ModelId>,
    bench_yaw0: f32,
    /// The aim resolved by the last third-person tick, for the HUD.
    aim: (f32, f32),
    /// A map to switch to before the next frame (a zone change), with its hash.
    pending_map: Option<(Bsp, u64)>,
    palette: world::Palette,
    /// The script's aim (`--script fight`): the nearest living enemy as of the last frame,
    /// its id, position and velocity, and when it was seen.
    script_target: Option<(u32, Vec3, Vec3, Instant)>,
    last_report: Instant,
    report_frame: usize,
    /// The browser: the GPU device and a fetched map arrive here from their tasks.
    #[cfg(target_arch = "wasm32")]
    pending_gpu: PendingSlot<(wgpu::Surface<'static>, Gpu)>,
    #[cfg(target_arch = "wasm32")]
    pending_fetch: PendingSlot<(Bsp, u64)>,
    /// Milliseconds from navigation to the first frame presented (WEB.md 9).
    #[cfg(target_arch = "wasm32")]
    first_frame_ms: f64,
    /// A replay being watched instead of a game being played (ANTICHEAT.md 3.4).
    #[cfg(not(target_arch = "wasm32"))]
    playback: Option<crate::playback::Playback>,
    /// The fight's effects between frames, the triangles they made this frame, and the
    /// names over the bodies (LOOK.md 13).
    effects: crate::fx::Effects,
    fx: crate::fx::FxMesh,
    tags: Vec<Tag>,
    /// The numbers over the bodies this frame (LOOK.md 13.8).
    pops: Vec<crate::fx::Pop>,
    /// The combo counter (MODES.md 4.6): blows the own hand landed within two seconds of
    /// each other, and when the last landed.
    combo: (u32, Option<Instant>),
    /// The recoil's punch on the view (MODES.md 3.3), (yaw, pitch) degrees, decaying.
    view_punch: (f32, f32),
    /// How far the eye has sunk into a crouch (MODES.md 3.4), units, eased.
    eye_drop: f32,
    /// The scope's zoom this frame (1 without one): the field of view is divided by it
    /// and so is the mouse.
    zoom: f32,
    /// The RPG mode's target, walk and waiting action (MODES.md 5).
    rpg: crate::rpg::Rpg,
    /// The last frame's view-projection and the window's size: what a click is
    /// unprojected through.
    last_vp: Option<(glam::Mat4, (f32, f32))>,
    /// The cursor the window shows (MODES.md 5.5): set again only when it changes.
    cursor_icon: CursorIcon,
    /// A right press in the RPG mode: when and where, and the camera's yaw and pitch
    /// then, to tell a tap on the target (the secondary) from a drag of the orbit.
    rpg_right: Option<(Instant, (f32, f32), f32, f32)>,
    /// The fingers on the screen (MODES.md 5.6), the controls drawn for them this frame
    /// (where a finger may land on one), and a tap's primary, held a moment.
    fingers: Fingers,
    touch_buttons: Vec<(TouchButton, ui::Rect)>,
    tap_fire: Option<Instant>,
    /// The yaw each body was drawn facing last frame (LOOK.md 13.9), by its key.
    facings: HashMap<u32, f32>,
    /// Per squad slot: the companion's health as last sent, and whether it lives.
    squad_view: Vec<(Option<u16>, bool)>,
    /// The creature being fought: name, health, maximum.
    target_view: Option<(String, u16, u16)>,
    /// The hub this client talks to, if it talks to one (CLIENT.md 2), and the session.
    hub: Option<Hub>,
    account: Option<Account>,
    /// The screens before the game (CLIENT.md 4.1 to 4.3), and whether one is up.
    front: Option<Front>,
    front_up: bool,
    /// A person started the client and there is no hub to talk to: the screen that says
    /// so is up (it names the settings file, and what is wrong if a hub was named).
    title: Option<crate::menu::Title>,
    /// The game menu when it is open, and the chat (CLIENT.md 4.4, 5).
    menu: Option<GameMenu>,
    chat: Chat,
    /// The inventory or a stall, when one is open (ITEMS.md 6), and the character being
    /// played: the one the hub is asked about.
    bag: Option<Bag>,
    /// The page of people, a trade or the tavern (PARTY.md 8).
    people: Option<People>,
    gm_page: Option<GmPage>,
    /// The character's page (MATRIX.md 9.1): `K`, or the menu.
    character_page: Option<CharacterPage>,
    /// The party on the HUD: each other member's name, and its health when it is here.
    /// The party's other members: `None` when no body of that name is here, else the
    /// health the wire carries for it, when it does.
    party_view: Vec<(String, Option<Option<u16>>)>,
    /// What is heard (SOUND.md), and where the own body was at the last offline frame
    /// (its ground travel for the steps).
    sound: crate::sound::Sound,
    sound_from: Option<Vec3>,
    character: Option<gm_hub_proto::protocol::CharacterId>,
    /// The toolkit's memory, and what happened since the last frame for it.
    ui: UiState,
    ui_input: UiInput,
    /// The pointer, in pixels, and the last press (for the double click).
    cursor: (f32, f32),
    last_press: Option<(Instant, (f32, f32))>,
    shift: bool,
    /// Ctrl (or the command key) is held and Alt is not: what a shortcut is made with. (Ctrl
    /// with Alt is how some keyboards type `@`.)
    command: bool,
    /// A paste on its way: the clipboard is read on a thread of its own (its owner may
    /// take seconds to answer), and what it held is typed by the frame that finds it here.
    #[cfg(not(target_arch = "wasm32"))]
    pasting: Option<std::sync::mpsc::Receiver<String>>,
    /// Zones being left: each says its goodbye on its own thread while the frames go on;
    /// kept until it has, or for a second.
    leaving: Vec<(NetClient, Instant)>,
    /// When the hub last heard from this client while it plays: a session lives while it
    /// is used (HUB.md 3.1), and a zone is played without a word to the hub.
    hub_touched: Instant,
    settings: Settings,
    /// What the settings file holds: a change is written within a second (CLIENT.md 8).
    settings_kept: Settings,
    #[cfg(not(target_arch = "wasm32"))]
    settings_path: Option<std::path::PathBuf>,
    /// Somebody plays the person (CLIENT.md 9).
    ui_script: Option<UiScript>,
    /// Whether a screen was up in the last frame: the pointer follows the change.
    was_up: bool,
    /// The last frame drew the screens: a UI script acts on what was drawn, so it waits
    /// through frames that drew nothing (a window that is covered, a surface being made).
    ui_drawn: bool,
    /// When the pointer was last asked for, and whether the browser had given it as of
    /// the last frame (a browser gives and takes it by itself, WEB.md 3.4).
    grab_asked: Option<Instant>,
    #[cfg(target_arch = "wasm32")]
    was_locked: bool,
    /// What the page was last told about the screens (CLIENT.md 4.1).
    #[cfg(target_arch = "wasm32")]
    told: String,
    /// What the page was last told of the text field with the keyboard (WEB.md 3.6): its
    /// value, or `None` for no field; and where the fields were, in CSS pixels.
    #[cfg(target_arch = "wasm32")]
    told_field: Option<String>,
    #[cfg(target_arch = "wasm32")]
    told_fields: Vec<f32>,
    /// The hash of the map that is loaded: what a zone's `Welcome` is compared with.
    map_hash: u64,
    /// The zone's connection ended and a person is at the client: the reason, for the
    /// characters screen.
    zone_ended: Option<String>,
}

/// A value a browser task delivers to the frame that waits for it.
#[cfg(target_arch = "wasm32")]
type PendingSlot<T> = std::rc::Rc<std::cell::RefCell<Option<Result<T, String>>>>;

/// How the client reaches its zone: directly, or with a ticket from the hub.
struct Entry {
    zone: ZoneAddr,
    token: Vec<u8>,
    zone_name: String,
}

fn online(opts: &Options, sim: &Sim, map_hash: u64, entry: Entry) -> Result<Online, Error> {
    log::info!("connecting to the zone as {}", opts.name);
    Ok(Online {
        net: NetClient::connect(
            entry.zone,
            opts.name.clone(),
            opts.build.clone(),
            opts.team,
            entry.token,
        )?,
        welcome: None,
        client: None,
        pack: None,
        team: 0,
        build_name: String::new(),
        accumulator: 0.0,
        prev_origin: sim.curr.origin,
        curr_origin: sim.curr.origin,
        last_snapshot: Instant::now(),
        rate: TickRate::COMBAT,
        names: HashMap::new(),
        looks: HashMap::new(),
        props: Vec::new(),
        kinds: HashMap::new(),
        squad: Vec::new(),
        social: Social::default(),
        messages: VecDeque::new(),
        stalls: Vec::new(),
        gm: false,
        tuning: Default::default(),
        gm_note: String::new(),
        kills: 0,
        deaths: 0,
        heals: Vec::new(),
        map_hash,
        respec_note: String::new(),
        pending_travel: None,
        zone_name: entry.zone_name,
        backlog: VecDeque::new(),
        awaiting_map: None,
    })
}

/// What the command line (or the page) already said of the way in (CLIENT.md 2): with a
/// user and a password the screens are taken by themselves as far as it reaches.
fn auto_of(opts: &Options) -> Option<Auto> {
    (!opts.user.is_empty()).then(|| Auto {
        email: opts.user.clone(),
        password: opts.password.clone(),
        register: opts.register,
        character: opts.character.clone(),
        preset: opts.build.clone(),
        zone: opts.zone.clone(),
    })
}

/// What a client is started with besides its map (CLIENT.md 2).
struct Start {
    online: Option<Online>,
    hub: Option<Hub>,
    front: Option<Front>,
    settings: Settings,
    map_hash: u64,
    ui_script: Option<UiScript>,
    /// The content bundle (CONTENT.md 6), or a client without one.
    content: Content,
}

/// The prop a look names, on the GPU (LOOK.md 6): loaded from the bundle when first seen;
/// nothing while it loads, for a look the pack does not name, or without a renderer.
fn held_prop(
    content: &mut Content,
    active: Option<&mut Active>,
    props: &[String],
    look: Look,
) -> Option<usize> {
    prop_slot(content, active, props, look.held)
}

/// The off hand's prop (LOOK.md 6.5), the same way.
fn off_prop(
    content: &mut Content,
    active: Option<&mut Active>,
    props: &[String],
    look: Look,
) -> Option<usize> {
    prop_slot(content, active, props, look.off)
}

/// The prop of an ability of the hub's pack (the selector's body, CLIENT.md 4.2: no zone
/// has sent a prop list yet): the manifest's prop for the ability's key, as a slot of the
/// character renderer, when the bundle has it.
fn ability_prop(
    content: &mut Content,
    active: Option<&mut Active>,
    pack: Option<&ContentPack>,
    ability: Option<u16>,
) -> Option<usize> {
    let key = &pack?.abilities.get(ability? as usize)?.key;
    let prop = content
        .manifest
        .as_ref()?
        .abilities
        .iter()
        .find(|a| &a.key == key)?
        .prop
        .clone()?;
    let active = active?;
    let (gpu, characters) = (&active.gpu, &mut active.renderer.characters);
    content.prop(&prop, |model| Some(characters.add_model(gpu, model)))
}

fn prop_slot(
    content: &mut Content,
    active: Option<&mut Active>,
    props: &[String],
    index: u16,
) -> Option<usize> {
    let key = props.get(index as usize)?;
    let active = active?;
    let (gpu, characters) = (&active.gpu, &mut active.renderer.characters);
    content.prop(key, |model| Some(characters.add_model(gpu, model)))
}

/// Where the view model of a prop sits (LOOK.md 6.4): the `fit_view` of the template
/// whose model it is, as a matrix in model space; identity for a prop no template fits.
fn view_fit_of(content: &Content, key: &str) -> Mat4 {
    content
        .manifest
        .as_ref()
        .and_then(|m| {
            m.templates
                .iter()
                .find(|t| t.model.as_deref() == Some(key))
                .and_then(|t| t.fit_view.as_ref())
        })
        .map_or(Mat4::IDENTITY, gm_model::pose::view_fit)
}

/// The prop of the weapon in the own hand in the gun mode (MODES.md 3.7): the ability
/// switched to, by its key in the pack, through the manifest; `None` in another mode or
/// for a hand with nothing. The zone says the same with a `Look`, a round trip later.
fn gun_hand_prop(
    content: &Content,
    pack: Option<&ContentPack>,
    c: &gm_net::client::ClientState,
) -> Option<String> {
    let kit = &c.sheet.kit;
    if kit.mode != gm_core::vocab::Mode::Gun {
        return None;
    }
    let slot = c.mover.in_hand(kit)? as usize;
    let def = kit.abilities.get(slot)?.id.0.checked_sub(1)? as usize;
    let key = &pack?.abilities.get(def)?.key;
    content
        .manifest
        .as_ref()?
        .abilities
        .iter()
        .find(|a| &a.key == key)?
        .prop
        .clone()
}

/// How far along the reload of the firearm in the own hand is (MODES.md 3.2): 0 when
/// none is under way, rising to 1 as it ends.
fn reload_progress(c: &gm_net::client::ClientState) -> f32 {
    let Some((f, g)) = c.mover.gun_in_hand(&c.sheet.kit) else {
        return kit_progress(c);
    };
    let Some(until) = g.reload_until else {
        return kit_progress(c);
    };
    let left = gm_core::sim::tick_delta(until, c.tick).max(0) as f32;
    (1.0 - left / f.reload.max(1) as f32).clamp(0.0, 1.0)
}

/// How far along a kit's use is (MODES.md 11.3), 0 when none: the view model is lowered
/// and worked as for a reload.
fn kit_progress(c: &gm_net::client::ClientState) -> f32 {
    let Some(until) = c.mover.kit_until else {
        return 0.0;
    };
    let whole = TickRate::COMBAT.ms_to_ticks(gm_core::sim::KIT_USE_MS);
    let left = gm_core::sim::tick_delta(until, c.tick).max(0) as f32;
    (1.0 - left / whole.max(1) as f32).clamp(0.0, 1.0)
}

fn app(opts: Options, bsp: Bsp, palette: world::Palette, sim: Sim, start: Start) -> App {
    let mesh = world::build(&bsp, &palette);
    let faces_total = mesh
        .face_ranges
        .iter()
        .filter(|r| r.index_count > 0)
        .count();
    let viewport = if opts.third_person {
        Viewport::Third
    } else {
        Viewport::First
    };
    let touch = opts.touch;
    let mut app = App {
        view_kick: 0.0,
        view_swing: 0.0,
        view_stride: 0.0,
        view_model: None,
        content: start.content,
        offline_prop: None,
        offline_off: None,
        front_up: start.front.is_some() && start.online.is_none(),
        title: None,
        hub: start.hub,
        account: None,
        front: start.front,
        menu: None,
        chat: Chat::default(),
        bag: None,
        people: None,
        gm_page: None,
        character_page: None,
        party_view: Vec::new(),
        sound: crate::sound::Sound::new(
            start.settings.volume,
            start.settings.mute,
            opts.sound_dump.clone(),
        ),
        sound_from: None,
        character: None,
        ui: UiState::default(),
        ui_input: UiInput::default(),
        cursor: (0.0, 0.0),
        last_press: None,
        shift: false,
        command: false,
        #[cfg(not(target_arch = "wasm32"))]
        pasting: None,
        leaving: Vec::new(),
        hub_touched: Instant::now(),
        settings_kept: start.settings.clone(),
        #[cfg(not(target_arch = "wasm32"))]
        settings_path: None,
        settings: start.settings,
        ui_script: start.ui_script,
        map_hash: start.map_hash,
        was_up: false,
        ui_drawn: false,
        grab_asked: None,
        #[cfg(target_arch = "wasm32")]
        was_locked: false,
        #[cfg(target_arch = "wasm32")]
        told: String::new(),
        #[cfg(target_arch = "wasm32")]
        told_field: None,
        #[cfg(target_arch = "wasm32")]
        told_fields: Vec::new(),
        zone_ended: None,
        opts,
        window: None,
        bsp,
        mesh: Some(mesh),
        active: None,
        sim,
        online: start.online,
        input: Input::default(),
        viewport,
        stats: FrameStats::new(),
        last_frame: Instant::now(),
        last_title: Instant::now(),
        started: Instant::now(),
        title_frame: 0,
        grabbed: false,
        adapter_info: None,
        faces_total,
        exit_requested: false,
        acquire_timeouts: 0,
        entities: Vec::new(),
        bodies: Vec::new(),
        revoked: Vec::new(),
        bench_yaw0: 0.0,
        aim: (0.0, 0.0),
        pending_map: None,
        palette,
        script_target: None,
        last_report: Instant::now(),
        report_frame: 0,
        #[cfg(target_arch = "wasm32")]
        pending_gpu: Default::default(),
        #[cfg(target_arch = "wasm32")]
        pending_fetch: Default::default(),
        #[cfg(target_arch = "wasm32")]
        first_frame_ms: 0.0,
        #[cfg(not(target_arch = "wasm32"))]
        playback: None,
        effects: Default::default(),
        fx: Default::default(),
        tags: Vec::new(),
        pops: Vec::new(),
        combo: (0, None),
        view_punch: (0.0, 0.0),
        eye_drop: 0.0,
        zoom: 1.0,
        rpg: crate::rpg::Rpg::new(),
        last_vp: None,
        cursor_icon: CursorIcon::Default,
        rpg_right: None,
        fingers: Fingers::expecting(touch),
        touch_buttons: Vec::new(),
        tap_fire: None,
        facings: HashMap::new(),
        squad_view: Vec::new(),
        target_view: None,
    };
    app.bench_yaw0 = app.sim.yaw;
    app
}

/// Open the replay named on the command line and point the options at its map.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_replay(opts: &mut Options) -> Result<Option<crate::playback::Playback>, Error> {
    let Some(path) = opts.replay.clone() else {
        return Ok(None);
    };
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let playback = crate::playback::Playback::open(&bytes, opts.follow.as_deref(), opts.from)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let header = &playback.replay.header;
    opts.map = opts.maps_dir.join(format!("{}.bsp", header.map));
    match std::fs::read(&opts.map) {
        Ok(map) if fnv1a64(&map) == header.map_hash => {}
        Ok(_) => log::warn!(
            "{} is not the build of the map the replay was recorded on: bodies may stand in walls",
            opts.map.display()
        ),
        Err(e) => return Err(format!("the replay's map {}: {e}", opts.map.display()).into()),
    }
    log::info!(
        "replay {}: {} on {}, {:.1} s, {} frames, following {}",
        path.display(),
        header.zone,
        header.map,
        playback.seconds(),
        playback.replay.frames.len(),
        playback.name(playback.follow)
    );
    Ok(Some(playback))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn run(mut opts: Options) -> Result<(), Error> {
    let playback = open_replay(&mut opts)?;
    let map_bytes =
        std::fs::read(&opts.map).map_err(|e| format!("loading {}: {e}", opts.map.display()))?;
    let bsp = Bsp::load(&opts.map).map_err(|e| format!("loading {}: {e}", opts.map.display()))?;
    log::info!(
        "loaded {} ({} faces, {} leaves)",
        opts.map.display(),
        bsp.faces.len(),
        bsp.leaves.len()
    );
    let palette = world::load_palette(&opts.palette);
    let sim = Sim::at(&bsp, opts.start);
    // A run that somebody scripted (a benchmark, a replay, a crowd) is not a person's: the
    // person's settings do not send it to a login screen, and do not change what it
    // measures either (a ticked "fullscreen"). It reads settings only when it names them.
    let personal = opts.bench_frames.is_none()
        && opts.seconds == 0.0
        && opts.script.is_none()
        && opts.replay.is_none()
        && opts.crowd == 0
        && opts.start.is_none()
        && !opts.offline
        && opts.connect.is_none();
    let settings_path = match &opts.settings {
        Some(path) => Some(path.clone()),
        None if personal => crate::settings::default_path(),
        None => None,
    };
    let settings = settings_path
        .as_deref()
        .map(Settings::load)
        .unwrap_or_default();
    // Where the hub is: the command line, else the person's settings, else what the build
    // ships beside the program (CLIENT.md 2). `--offline` and `--connect` ask for none.
    // A hub that is named and cannot be used is said on a screen, like one that is not
    // named at all: nobody started this from a terminal.
    let resolve = |hub: &str| {
        use std::net::ToSocketAddrs;
        hub.to_socket_addrs()
            .ok()
            .and_then(|mut found| found.next())
    };
    let read = |cert: &std::path::Path| {
        std::fs::read(cert).map_err(|e| {
            format!(
                "The hub's certificate ({}) cannot be read: {e}.",
                cert.display()
            )
        })
    };
    let named = |hub: &str, cert: std::path::PathBuf, by: &str| match resolve(hub) {
        Some(addr) => read(&cert).map(|der| Some((addr, der))),
        None => Err(format!("The hub {by} names ({hub}) cannot be found.")),
    };
    let hub_at: Result<Option<(std::net::SocketAddr, Vec<u8>)>, String> = match opts.hub {
        // A command line that is wrong ends the program, with the reason.
        Some(addr) => {
            let cert = std::fs::read(&opts.hub_cert)
                .map_err(|e| format!("reading hub certificate {}: {e}", opts.hub_cert.display()))?;
            Ok(Some((addr, cert)))
        }
        None if !personal => Ok(None),
        None if !settings.hub.is_empty() => named(
            &settings.hub,
            std::path::PathBuf::from(&settings.hub_cert),
            "the settings file",
        ),
        None => match crate::settings::site_hub(&crate::install_root()) {
            Some((hub, cert)) => named(&hub, cert, "this build"),
            None => Ok(None),
        },
    };
    // A person, and no hub to talk to: say so on a screen instead of walking off
    // (CLIENT.md 2).
    let title = match &hub_at {
        Ok(Some(_)) => None,
        _ if !personal => None,
        found => Some(crate::menu::Title {
            settings: settings_path
                .as_deref()
                .map_or_else(String::new, |p| p.display().to_string()),
            why: found.as_ref().err().cloned(),
        }),
    };
    let hub_at = hub_at.unwrap_or(None);
    // The command line's password has done its work once the screens have it.
    let auto = auto_of(&opts);
    opts.password.clear();
    let map_hash = fnv1a64(&map_bytes);
    let ui_script = opts.ui_script.as_deref().map(UiScript::parse).transpose()?;
    let content = Content::load(&crate::install_root().join(&opts.assets));
    let start = match (hub_at, opts.connect) {
        // Through the hub: the screens, or the command line in their place.
        (Some((addr, cert_der)), _) if playback.is_none() => {
            let hub = Hub::new(&ZoneAddr {
                addr: Some(addr),
                cert_der,
                web: None,
            })?;
            let front = Front::new(
                Box::new(hub.clone()),
                settings.email.clone(),
                settings.character.clone(),
                auto,
            );
            Start {
                online: None,
                hub: Some(hub),
                front: Some(front),
                settings,
                map_hash,
                ui_script,
                content,
            }
        }
        (_, Some(addr)) => {
            let cert = std::fs::read(&opts.cert)
                .map_err(|e| format!("reading zone certificate {}: {e}", opts.cert.display()))?;
            let entry = Entry {
                zone: ZoneAddr {
                    addr: Some(addr),
                    cert_der: cert,
                    web: None,
                },
                token: Vec::new(),
                zone_name: String::new(),
            };
            Start {
                online: Some(online(&opts, &sim, map_hash, entry)?),
                hub: None,
                front: None,
                settings,
                map_hash,
                ui_script,
                content,
            }
        }
        _ => Start {
            online: None,
            hub: None,
            front: None,
            settings,
            map_hash,
            ui_script,
            content,
        },
    };

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = app(opts, bsp, palette, sim, start);
    app.playback = playback;
    app.title = title;
    app.settings_path = settings_path;
    event_loop.run_app(&mut app)?;
    if let Some(o) = &mut app.online {
        o.net.close();
    }
    // Goodbyes still on their way are waited for: the zone frees the body now.
    for (mut net, _) in app.leaving.drain(..) {
        net.close();
    }
    if let Some(hub) = &app.hub {
        if app.account.is_some() {
            log::info!("logging out of the hub");
        }
        hub.logout();
    }
    app.keep_settings();

    let report = app.stats.report();
    let scripted = app.opts.bench_frames.is_some() || app.opts.seconds > 0.0;
    if let (true, Some(info)) = (scripted, &app.adapter_info) {
        let faces = app.active.as_ref().map_or(0, |a| a.renderer.faces_drawn);
        let draws = app.active.as_ref().map_or(1, |a| a.renderer.draw_calls);
        print_bench(&report, info, "windowed", faces, app.faces_total, draws);
    } else if report.frames > 0 {
        let (_, peak) = crate::stats::rss_bytes();
        log::info!(
            "{} frames, {:.1} fps average, peak RSS {} bytes ({:.1} MiB)",
            report.frames,
            report.fps_avg,
            peak,
            peak as f64 / 1048576.0
        );
    }
    if let Some(a) = &app.active {
        print_bench_avatars(&report, &a.avatars, &a.renderer);
    }
    log::info!("{}", app.sound.report());
    if scripted || app.opts.sound_dump.is_some() {
        println!("{}", app.sound.report());
    }
    if let Some(said) = app.sound.finish() {
        log::info!("{said}");
    }
    if let Some(o) = &app.online
        && let Some(c) = &o.client
    {
        let s = c.stats;
        log::info!(
            "net: {} snapshots, {} gaps (max {}), {} corrections ({} unexplained, max {:.1} u), {} inputs sent, delay {} ticks",
            s.snapshots,
            s.gaps,
            s.max_gap,
            s.corrections,
            s.corrections_unexplained,
            s.max_correction,
            s.inputs_sent,
            c.delay_ticks
        );
    }
    if app.exit_requested {
        return Err("exited on error".into());
    }
    Ok(())
}

/// A map and its coloured lightmaps from the page's assets, with the hash of the `.bsp`.
/// With `wanted` (the hash the zone told, WEB.md 5) the URLs carry it as a stamp: a browser
/// that kept last build's map in its cache has nothing under this name and asks the site
/// (found 2026-10-06: a map rebuilt on the site, the old one played from the LAN).
#[cfg(target_arch = "wasm32")]
async fn fetch_map(assets: &str, name: &str, wanted: Option<u64>) -> Result<(Bsp, u64), String> {
    /// A map that has not arrived in this long is not coming (the zone waits 60 s for a
    /// client's first input).
    const FETCH_MS: u32 = 45_000;
    let stamp = wanted.map_or(String::new(), |h| format!("?v={h:016x}"));
    let url = format!("{assets}/maps/{name}.bsp{stamp}");
    let fetch = async {
        let bytes = crate::web::fetch_bytes(&url)
            .await?
            .ok_or_else(|| format!("{url}: no such map on this site"))?;
        let mut bsp = Bsp::parse(&bytes).map_err(|e| format!("{url}: {e}"))?;
        if let Some(lit) =
            crate::web::fetch_bytes(&format!("{assets}/maps/{name}.lit{stamp}")).await?
        {
            bsp.attach_lit(&lit).map_err(|e| format!("{url}: {e}"))?;
        }
        Ok((bsp, fnv1a64(&bytes)))
    };
    crate::web::timeout_ms(FETCH_MS, fetch)
        .await
        .unwrap_or_else(|| Err(format!("{url}: the download took too long")))
}

/// The browser client (WEB.md 3, 5): options from the page, assets by `fetch`, the login
/// awaited, then the event loop on the page's own.
#[cfg(target_arch = "wasm32")]
pub async fn run_web() -> Result<(), String> {
    use crate::web::{PageOptions, parse_hash, tell_page};
    use gm_net::control::WebAddr;
    use winit::platform::web::EventLoopExtWebSys;

    let page = PageOptions::read();
    let web_addr = |url: &str, hash: &str| -> Result<Option<WebAddr>, String> {
        let Some(url) = page.string(url) else {
            return Ok(None);
        };
        let cert_sha256 = match page.string(hash) {
            Some(h) => Some(parse_hash(&h).ok_or("a certificate hash must be 64 hex digits")?),
            None => None,
        };
        Ok(Some(WebAddr { url, cert_sha256 }))
    };
    let mut opts = Options {
        name: page.string("name").unwrap_or_else(|| "web".into()),
        build: page.string("build"),
        team: page.number("team").unwrap_or(0.0) as u8,
        third_person: page.flag("third-person"),
        user: page.string("user").unwrap_or_default(),
        password: page.take("password").unwrap_or_default(),
        register: page.flag("register"),
        character: page.string("character").unwrap_or_default(),
        seconds: page.number("seconds").unwrap_or(0.0) as f32,
        script: page.string("script"),
        report: page.flag("report"),
        travel_to: page.string("travel-to"),
        travel_after: page.number("travel-after").unwrap_or(0.0) as f32,
        cache_mb: page.number("cache-mb").unwrap_or(128.0) as u64,
        hub_web: web_addr("hub", "hub-cert")?,
        connect_web: web_addr("connect", "cert")?,
        assets: page.string("assets").unwrap_or_else(|| "assets".into()),
        ui_script: page.string("ui-script"),
        touch: page.flag("touch"),
        ..Options::default()
    };
    if let Some(zone) = page.string("zone") {
        opts.zone = zone;
    }
    if let Some(mb) = page.number("vram-mb") {
        opts.vram_mb = mb as u64;
    }
    let map = page.string("map").unwrap_or_else(|| "test_room".into());
    if !valid_map_name(&map) {
        return Err(format!("not a map name: {map:?}"));
    }

    tell_page("status", "loading the map");
    let (bsp, map_hash) = fetch_map(&opts.assets, &map, None).await?;
    let palette = world::palette_from_bytes(
        crate::web::fetch_bytes(&format!("{}/textures/palette.lmp", opts.assets))
            .await?
            .as_deref(),
    );
    let sim = Sim::at(&bsp, opts.start);
    let settings = Settings::load();
    let ui_script = opts.ui_script.as_deref().map(UiScript::parse).transpose()?;
    let content = Content::fetch(&opts.assets).await;
    // Through the hub: the screens, or the page's options in their place (the page's form
    // gave the email and the password, CLIENT.md 4.1).
    let through_hub = opts.hub_web.is_some() && opts.connect_web.is_none();
    let start = if through_hub {
        let hub = Hub::new(&ZoneAddr {
            addr: None,
            cert_der: Vec::new(),
            web: opts.hub_web.clone(),
        })
        .map_err(|e| e.to_string())?;
        let front = Front::new(
            Box::new(hub.clone()),
            settings.email.clone(),
            settings.character.clone(),
            auto_of(&opts),
        );
        // The password has done its work; nothing keeps it (WEB.md 5).
        opts.password.clear();
        Start {
            online: None,
            hub: Some(hub),
            front: Some(front),
            settings,
            map_hash,
            ui_script,
            content,
        }
    } else {
        let online = match opts.connect_web.clone() {
            Some(web) => {
                tell_page("status", "connecting to the zone");
                let entry = Entry {
                    zone: ZoneAddr {
                        addr: None,
                        cert_der: Vec::new(),
                        web: Some(web),
                    },
                    token: Vec::new(),
                    zone_name: String::new(),
                };
                Some(online(&opts, &sim, map_hash, entry).map_err(|e| e.to_string())?)
            }
            None => None,
        };
        Start {
            online,
            hub: None,
            front: None,
            settings,
            map_hash,
            ui_script,
            content,
        }
    };
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    // Frames come from `requestAnimationFrame`: each redraw asks for the next.
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.spawn_app(app(opts, bsp, palette, sim, start));
    Ok(())
}

/// Third-person camera position: pulled in by a point trace so it never enters a wall.
pub(crate) fn third_person_camera(
    world: &dyn CollisionWorld,
    eye: Vec3,
    yaw: f32,
    pitch: f32,
) -> Vec3 {
    let fwd = view_dir(yaw, pitch);
    let (_, right) = yaw_vectors(yaw);
    let desired = eye - fwd * CAMERA_BACK + right * CAMERA_RIGHT + Vec3::Z * CAMERA_UP;
    let tr = world.trace(Hull::Point, eye, desired);
    if tr.fraction < 1.0 {
        let back = (eye - tr.end).normalize_or_zero();
        tr.end + back * 6.0
    } else {
        desired
    }
}

/// Camera-to-muzzle re-aim (VOCABULARY.md 9): resolve the camera ray to the first world or
/// player hit, then aim at that point from the eyes. Returns `(yaw, pitch)` for the input.
fn re_aim(
    world: &dyn CollisionWorld,
    bodies: &[Aabb],
    camera: Vec3,
    cam_yaw: f32,
    cam_pitch: f32,
    eye: Vec3,
) -> (f32, f32) {
    let dir = view_dir(cam_yaw, cam_pitch);
    let far = camera + dir * AIM_REACH;
    let tr = world.trace(Hull::Point, camera, far);
    let mut point = if tr.fraction < 1.0 { tr.end } else { far };
    let bt = sweep_boxes(Hull::Point, camera, point, bodies);
    if bt.fraction < 1.0 && !bt.start_solid {
        point = bt.end;
    }
    let to = point - eye;
    // A hit between the camera and the body: aim straight along the camera.
    let aim = if to.dot(dir) < 16.0 {
        dir
    } else {
        to.normalize_or_zero()
    };
    let yaw = aim.y.atan2(aim.x).to_degrees().rem_euclid(360.0);
    let pitch = (-aim.z).clamp(-1.0, 1.0).asin().to_degrees();
    (yaw, pitch)
}

/// The faces visible from any of `leaves`, each listed once.
fn visible_from(bsp: &Bsp, leaves: &[usize]) -> Vec<u32> {
    let mut seen = vec![false; bsp.faces.len()];
    let mut out = Vec::new();
    for &leaf in leaves {
        for f in bsp.visible_faces(leaf) {
            if !seen[f as usize] {
                seen[f as usize] = true;
                out.push(f);
            }
        }
    }
    out
}

/// What a map says its air is (SOUND.md 3): the worldspawn's `gm_ambience`, when it has one.
fn ambience_of(bsp: &Bsp) -> Option<&str> {
    bsp.entities
        .iter()
        .find(|e| e.classname() == "worldspawn")
        .and_then(|e| e.get("gm_ambience"))
}

/// A map's name as a zone may send it: letters, digits, `_` and `-`, at most 64. It is joined
/// to a directory natively and put into a URL in a browser.
fn valid_map_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What the script assumes of the weapons it holds (the v1 content's are close to it): a
/// bolt's speed and the wind-up before it leaves, a melee weapon's reach from the eye, and
/// how far the first active (an area around the caster for most builds) reaches.
const SCRIPT_BOLT_SPEED: f32 = 1400.0;
const SCRIPT_BOLT_WINDUP: f32 = 0.15;
const SCRIPT_MELEE_REACH: f32 = 84.0;
const SCRIPT_AREA_REACH: f32 = 150.0;

/// `--script fight` (WEB.md 8): face the nearest enemy and walk at it; in reach strike and
/// use the first active (an area for most builds, which a guard does not turn away as it
/// does a swing); further off loose the ranged secondary where the enemy will be. With
/// nobody in sight, walk and turn. `target` is the enemy's position and velocity.
fn fight_input(eye: Vec3, target: Option<(Vec3, Vec3)>, sim: &mut Sim, tick: u32) -> SimInput {
    let mut input = SimInput {
        buttons: 0,
        yaw: sim.yaw,
        pitch: 0.0,
        forward: 1.0,
        side: 0.0,
        ability: 0,
        held: 0,
        target: 0,
    };
    match target {
        Some((at, velocity)) => {
            let distance = (at - eye).length().max(1.0);
            let melee = distance < SCRIPT_MELEE_REACH;
            // A bolt is aimed where the enemy will be when it arrives.
            let aim_at = if melee {
                at
            } else {
                at + velocity * (SCRIPT_BOLT_WINDUP + distance / SCRIPT_BOLT_SPEED)
            };
            let to = aim_at - eye;
            let reach = to.length().max(1.0);
            sim.yaw = to.y.atan2(to.x).to_degrees().rem_euclid(360.0);
            sim.pitch = (-to.z / reach).clamp(-1.0, 1.0).asin().to_degrees();
            input.yaw = sim.yaw;
            input.pitch = sim.pitch;
            if melee {
                input.buttons |= buttons::PRIMARY;
                if distance < 48.0 {
                    input.forward = 0.0;
                }
            } else if distance < 900.0 {
                input.buttons |= buttons::SECONDARY;
            }
            // The first active as often as it comes back, the second now and then.
            if distance < SCRIPT_AREA_REACH && tick.is_multiple_of(32) {
                input.ability = if tick.is_multiple_of(256) { 2 } else { 1 };
            }
        }
        None => {
            sim.yaw = (sim.yaw + 0.6).rem_euclid(360.0);
            sim.pitch = 0.0;
            input.yaw = sim.yaw;
        }
    }
    input
}

fn role_name(role: u8) -> &'static str {
    match role {
        0 => "heal",
        1 => "tank",
        2 => "scout",
        _ => "dps",
    }
}

fn order_name(order: &Order) -> &'static str {
    match order {
        Order::Follow => "follow",
        Order::Hold => "hold",
        Order::MoveTo(_) => "move",
        Order::Attack(_) => "attack",
    }
}

/// The HUD of one frame (COMPANIONS.md 6): the own bars, the squad panel, the creature
/// being fought and the messages in every viewport.
/// What the HUD draws besides the own bars: the squad's health, the creature being
/// fought, and the scale it is all drawn at.
pub(crate) struct HudView<'a> {
    /// The names over the bodies in sight (LOOK.md 13).
    pub tags: &'a [Tag],
    /// The numbers floating over the bodies (LOOK.md 13.8).
    pub pops: &'a [crate::fx::Pop],
    /// The frame is seen from the own eyes: the own numbers have no head to float over
    /// and sit under the aim instead.
    pub first_person: bool,
    /// The own body was just hurt: 1 running down to 0, the frame's red edge.
    pub hurt: f32,
    pub squad: &'a [(Option<u16>, bool)],
    /// The other members of the party: name; `None` when no body of that name is here,
    /// else the health the wire carries for it, when it does.
    pub party: &'a [(String, Option<Option<u16>>)],
    pub target: Option<&'a (String, u16, u16)>,
    pub scale: f32,
    /// The bundle's manifest: where the icons' keys come from (LOOK.md 3).
    pub manifest: Option<&'a gm_model::manifest::Manifest>,
    /// Seconds since the client started: the hotbar's flashes run on it.
    pub time: f32,
    /// The own name, for the portrait frame.
    pub own_name: &'a str,
    /// The combo counter (MODES.md 4.6): hits in the chain, and seconds since the last.
    pub combo: (u32, f32),
    /// The gun mode (MODES.md 3.8): whether the body crouches (the cone's base), and the
    /// scope's zoom this frame (1 without).
    pub crouched: bool,
    pub zoom: f32,
    /// The scope is up: the cone the crosshair shows shrinks with it (MODES.md 10.2).
    pub scoped: bool,
    /// A finger's controls (MODES.md 5.6): the buttons and which are held, and the
    /// stick's centre and knob while one holds it.
    pub touch: TouchHud<'a>,
}

#[derive(Default)]
pub(crate) struct TouchHud<'a> {
    pub buttons: &'a [(TouchButton, ui::Rect)],
    pub held: Vec<TouchButton>,
    pub stick: Option<((f32, f32), (f32, f32))>,
}

/// A name over a body (LOOK.md 13): where its head is, what it is called, the colour of
/// its side, and its health as a part of the whole when the frame knows both.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tag {
    pub at: Vec3,
    pub name: String,
    pub ink: [f32; 4],
    pub health: Option<f32>,
}

/// A body turns toward its way of travel this fast when it is drawn (LOOK.md 13.9).
const FACING_TURN_DEG_PER_SEC: f32 = 720.0;
/// A body runs facing its travel only while the travel is within this of where it
/// looks: a side-step is a run to the side; backing off is a backpedal, the eyes on
/// whom it backs off from.
const FACING_BACKPEDAL_DEG: f32 = 100.0;

/// Where a body is drawn facing (LOOK.md 13.9): running or in the air, its way of
/// travel, so that a side-step is a run to the side and not a slide of a body running
/// ahead; in every other stance where it looks, which is where its blow lands. `drawn` is
/// the yaw it was drawn at last frame: the turn is quick, not a snap.
///
/// An RPG body (`free`, MODES.md 5.1) does not turn with its camera: it runs facing its
/// travel whichever way that is (no backpedal: `S` walks it toward the camera), turns to
/// where it looks only for an action (the zone fires there, or at its target while the
/// turn holds it, in which case the caller passes that yaw and `free` false), and in
/// every other stance stands as it was left.
pub(crate) fn facing(
    drawn: Option<f32>,
    view_yaw: f32,
    vel: Vec3,
    anim: u8,
    free: bool,
    dt: f32,
) -> f32 {
    use gm_core::sim::anim;
    let flat = vel.truncate();
    let mut target = view_yaw;
    if matches!(anim, anim::RUN | anim::AIR) && flat.length() > 10.0 {
        let travel = flat.y.atan2(flat.x).to_degrees();
        let off = (travel - view_yaw + 180.0).rem_euclid(360.0) - 180.0;
        if free || off.abs() <= FACING_BACKPEDAL_DEG {
            target = travel;
        }
    } else if free && !anim::acts(anim) {
        if let Some(drawn) = drawn {
            return drawn;
        }
    }
    let Some(drawn) = drawn else {
        return target.rem_euclid(360.0);
    };
    let delta = (target - drawn + 180.0).rem_euclid(360.0) - 180.0;
    let step = FACING_TURN_DEG_PER_SEC * dt.max(0.0);
    (drawn + delta.clamp(-step, step)).rem_euclid(360.0)
}

/// The wedge and the times of an ability's swing, when its script begins with one: what
/// the effects draw (`fx::Swing`), from the numbers the zone resolves the hit with.
pub(crate) fn swing_of(ability: &gm_core::vocab::Ability, dt: f32) -> Option<crate::fx::Swing> {
    match ability.steps.first() {
        Some(gm_core::vocab::Step {
            verb: gm_core::vocab::Verb::MeleeArc(arc),
            at,
        }) => Some(crate::fx::Swing {
            reach: arc.reach,
            arc_deg: arc.arc_deg,
            windup: (at + arc.timing.windup) as f32 * dt,
            active: arc.timing.active as f32 * dt,
        }),
        _ => None,
    }
}

/// The colour of what deals a damage: its element's (the aspects' colours), or steel's
/// for a blow.
pub(crate) fn damage_ink(damage: &gm_core::vocab::DamagePacket) -> [f32; 4] {
    match damage.dtype.element() {
        Some(e) => {
            let c = crate::avatars::ASPECT_COLOURS[e as usize];
            [c[0], c[1], c[2], 1.0]
        }
        None => [0.92, 0.94, 1.0, 1.0],
    }
}

/// One cell of the hotbar (LOOK.md 3.2), as `--report` and the gate read it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HotbarCell {
    pub key: &'static str,
    pub ability: String,
    /// `ready`, `cooling`, `unaffordable`, `silenced`, `active`.
    pub state: &'static str,
    /// Of the cooldown, 0 (just used) to 1 (ready).
    pub ready: f32,
    /// Seconds until ready, while cooling.
    pub left_secs: f32,
    /// The stage a chain's window has reached (MODES.md 4.3): 0 outside one, else 2, 3...
    pub stage: u8,
}

/// The other bodies as the RPG mode reads them (MODES.md 5): where they stand now, their
/// frame, and whether they are enemies (another team; in the wild, everyone).
fn rpg_bodies_of(c: &ClientState, my_team: u8) -> Vec<crate::rpg::Body> {
    c.others_at(c.render_tick(0.0))
        .iter()
        .filter(|e| e.kind == EntityKind::Player && e.alive())
        .filter_map(|e| match e.spawn {
            SpawnInfo::Player { frame, team, .. } => Some(crate::rpg::Body {
                id: e.id,
                origin: e.pos,
                frame: crate::rpg::frame_of(frame),
                crouched: e.flags & gm_net::snapshot::flags::CROUCHED != 0,
                enemy: team != my_team || team == gm_core::sim::TEAM_WILD,
            }),
            _ => None,
        })
        .collect()
}

/// The active slot (1-based) that is a dash with an untouchable window (MODES.md 4.4),
/// while it is ready: what Space plays in the action mode.
pub(crate) fn dodge_slot(
    kit: &gm_core::build::Kit,
    mover: &gm_core::sim::Mover,
    now: u32,
) -> Option<u8> {
    kit.actives.iter().enumerate().find_map(|(i, slot)| {
        let slot = (*slot)? as usize;
        let ab = kit.abilities.get(slot)?;
        let dash = matches!(
            ab.steps.first().map(|s| &s.verb),
            Some(gm_core::vocab::Verb::MoveSelf(ms))
                if matches!(ms.kind, gm_core::vocab::MoveKind::Dash { .. }) && ms.iframes > 0
        );
        (dash && gm_core::sim::tick_delta(now, mover.cooldowns[slot]) >= 0).then_some(i as u8 + 1)
    })
}

/// The hotbar's cells for the own body: what each key does and its state now.
pub(crate) fn hotbar(o: &Online) -> Vec<HotbarCell> {
    let (Some(c), Some(pack)) = (&o.client, &o.pack) else {
        return Vec::new();
    };
    let kit = &c.sheet.kit;
    let build = &c.sheet.build;
    let now = c.tick;
    let mut cells = Vec::new();
    let mut cell = |key: &'static str, slot: Option<u8>, def: Option<u16>| {
        let (Some(slot), Some(def)) = (slot, def) else {
            return;
        };
        let (mut slot, mut def) = (slot as usize, def as usize);
        // A chain's window (MODES.md 4.3): the cell is the next stage's.
        let mut stage = 0;
        if let Some((from, next, _)) = c.mover.chain
            && from as usize == slot
        {
            let mut at = from;
            let mut n = 1u8;
            while at != next && n < 12 {
                match kit.chain_next.get(at as usize).copied().flatten() {
                    Some(k) => {
                        at = k;
                        n += 1;
                    }
                    None => break,
                }
            }
            stage = n;
            slot = next as usize;
            def = kit.abilities[slot].id.0.saturating_sub(1) as usize;
        }
        let (Some(ab), Some(d)) = (kit.abilities.get(slot), pack.abilities.get(def)) else {
            return;
        };
        let left = gm_core::sim::tick_delta(c.mover.cooldowns[slot], now).max(0) as f32;
        let whole = ab.cooldown.ticks.max(1) as f32;
        let ready = (1.0 - left / whole).clamp(0.0, 1.0);
        let elemental = kit.elemental.get(slot).copied().unwrap_or(false);
        let running = c
            .mover
            .script
            .as_ref()
            .is_some_and(|s| s.ability as usize == slot)
            || (key == "C" && c.mover.guard != gm_core::sim::GuardState::None);
        let state = if running {
            "active"
        } else if elemental && c.mover.statuses.silenced() {
            "silenced"
        } else if left > 0.0 {
            "cooling"
        } else if c.mover.stamina < ab.cost.stamina as f32 || c.mover.focus < ab.cost.focus as f32 {
            "unaffordable"
        } else {
            "ready"
        };
        cells.push(HotbarCell {
            key,
            ability: d.key.clone(),
            state,
            ready,
            left_secs: left * c.rate.dt(),
            stage,
        });
    };
    if kit.mode == gm_core::vocab::Mode::Gun {
        // The gun mode (MODES.md 3.7): the three weapons, the one in hand lit, then the
        // actives on 4 to 7.
        let knife = kit
            .knife
            .map(|k| kit.abilities[k as usize].id.0.saturating_sub(1));
        cell("1", kit.primary, Some(build.primary));
        cell("2", kit.secondary, Some(build.secondary));
        cell("3", kit.knife, knife);
        for (i, key) in ["4", "5", "6", "7"].into_iter().enumerate() {
            cell(
                key,
                kit.actives.get(i).copied().flatten(),
                build.actives.get(i).copied(),
            );
        }
        if let Some(held) = cells.get_mut(c.mover.held as usize)
            && held.state == "ready"
        {
            held.state = "active";
        }
        return cells;
    }
    // The RPG mode's keys (MODES.md 5.5): the kit on 1 to 6, the guard on Shift.
    let rpg = kit.mode == gm_core::vocab::Mode::Rpg;
    let keys: [&'static str; 7] = if rpg {
        ["1", "2", "Shift", "3", "4", "5", "6"]
    } else {
        ["LMB", "RMB", "C", "1", "2", "3", "4"]
    };
    cell(keys[0], kit.primary, Some(build.primary));
    cell(keys[1], kit.secondary, Some(build.secondary));
    cell(keys[2], kit.guard, build.guard);
    for (i, key) in keys[3..].iter().enumerate() {
        cell(
            key,
            kit.actives.get(i).copied().flatten(),
            build.actives.get(i).copied(),
        );
    }
    cells
}

/// The frame's size in device pixels: the window's natively. In a browser the page owns
/// the canvas's CSS size and winit never sets its backing size (WEB.md 3.5), so a canvas
/// on a phone of 2.6 device pixels a CSS pixel was drawn at a third of its pixels, and
/// fingers (reported in device pixels) landed outside the frame: the backing is the CSS
/// size times the device pixel ratio, which configuring the surface sets on the canvas.
fn frame_size(window: &Window) -> winit::dpi::PhysicalSize<u32> {
    #[cfg(target_arch = "wasm32")]
    if let Some(size) = crate::web::canvas_device_size() {
        return size;
    }
    window.inner_size()
}

/// The hotbar's cells (LOOK.md 3.2): `n` squares of 40 dots, 3 apart, bottom centre.
pub(crate) fn hotbar_rects(size: (f32, f32), s: f32, n: usize) -> Vec<ui::Rect> {
    let side = 40.0 * s;
    let gap = 3.0 * s;
    let total = n as f32 * (side + gap) - gap;
    let x0 = ((size.0 - total) * 0.5).round();
    let y0 = size.1 - 16.0 - side;
    (0..n)
        .map(|i| ui::Rect::new(x0 + i as f32 * (side + gap), y0, side, side))
        .collect()
}

/// The controls a finger may land on (MODES.md 5.6): the menu top right; in the action
/// and gun modes the jump and the secondary bottom right, over the hotbar's line; and
/// the hotbar's cells while a kit is played. Nothing without a finger seen.
pub(crate) fn touch_controls(
    size: (f32, f32),
    s: f32,
    online: Option<&Online>,
    rpg: bool,
) -> Vec<(TouchButton, ui::Rect)> {
    let (w, h) = size;
    let mut out = Vec::new();
    let menu = 36.0 * s;
    out.push((
        TouchButton::Menu,
        ui::Rect::new(w - 16.0 - menu, 16.0, menu, menu),
    ));
    let side = 60.0 * s;
    let gap = 12.0 * s;
    let y = h - 16.0 - side;
    if rpg {
        out.push((
            TouchButton::Secondary,
            ui::Rect::new(w - 16.0 - side, y, side, side),
        ));
    } else {
        out.push((
            TouchButton::Jump,
            ui::Rect::new(w - 16.0 - side, y, side, side),
        ));
        out.push((
            TouchButton::Secondary,
            ui::Rect::new(w - 16.0 - side - gap - side, y, side, side),
        ));
    }
    if let Some(o) = online {
        let n = hotbar(o).len();
        for (i, r) in hotbar_rects(size, s, n).into_iter().enumerate() {
            out.push((TouchButton::Hot(i as u8), r));
        }
    }
    out
}

/// Where a world point is on a screen of `size` pixels; `None` behind the camera.
pub(crate) fn project(view_proj: glam::Mat4, size: (f32, f32), point: Vec3) -> Option<(f32, f32)> {
    let clip = view_proj * point.extend(1.0);
    if clip.w <= 1.0 {
        return None;
    }
    let (x, y) = (clip.x / clip.w, clip.y / clip.w);
    Some(((x + 1.0) * 0.5 * size.0, (1.0 - y) * 0.5 * size.1))
}

pub(crate) fn build_hud(hud: &mut Hud, online: Option<&Online>, vp: glam::Mat4, view: HudView<'_>) {
    let HudView {
        tags,
        pops,
        first_person,
        hurt,
        squad: squad_view,
        party,
        target,
        scale: s,
        manifest,
        time,
        own_name,
        combo,
        crouched,
        zoom,
        scoped,
        touch,
    } = view;
    let (w, h) = hud.size;
    // The HUD's words: the text face of the bundle, the small one without it.
    let cap = hud.cap();
    let line = (cap + 5.0) * s;
    let skinned = hud.skinned;
    // A finger's controls (MODES.md 5.6): the stick where the finger landed, and the
    // buttons in the corners, lit while held. The hotbar's cells are their own buttons.
    if let Some((centre, knob)) = touch.stick {
        let reach = touch::STICK_DOTS * s;
        let c = Vec2::new(centre.0, centre.1);
        hud.wedge(c, reach, 0.0, 1.0, [1.0, 1.0, 1.0, 0.12]);
        let len = (knob.0 * knob.0 + knob.1 * knob.1).sqrt();
        let k = if len > reach {
            Vec2::new(knob.0 / len * reach, knob.1 / len * reach)
        } else {
            Vec2::new(knob.0, knob.1)
        };
        hud.wedge(c + k, reach * 0.4, 0.0, 1.0, [1.0, 1.0, 1.0, 0.35]);
    }
    for (button, r) in touch.buttons {
        let word = match button {
            TouchButton::Menu => "menu",
            TouchButton::Jump => "jump",
            TouchButton::Secondary => "2",
            TouchButton::Hot(_) => continue,
        };
        let held = touch.held.contains(button);
        let fill = if held {
            [1.0, 0.85, 0.3, 0.55]
        } else {
            [0.0, 0.0, 0.0, 0.35]
        };
        if !hud.frame(r.x, r.y, r.w, r.h, "hotbar_cell", s, hud::PLAIN) {
            hud.rect(r.x, r.y, r.w, r.h, [0.1, 0.1, 0.12, 0.6]);
        }
        hud.rect(
            r.x + 2.0 * s,
            r.y + 2.0 * s,
            r.w - 4.0 * s,
            r.h - 4.0 * s,
            fill,
        );
        let tw = hud.width(s, word);
        hud.print(
            r.x + (r.w - tw) * 0.5,
            r.y + (r.h - cap * s) * 0.5,
            s,
            hud::WHITE,
            word,
        );
    }
    // As much of `text` as fits in `room` pixels.
    let fit = |hud: &Hud, text: &str, room: f32| -> String {
        let mut out = String::new();
        for c in text.chars() {
            out.push(c);
            if hud.width(s, &out) > room {
                out.pop();
                break;
            }
        }
        out
    };
    // The whole HUD on the first layer, in call order: a screen's plates (the same layer,
    // drawn after) cover it, as they covered the bars before there were layers.
    hud.set_layer(ui::LAYER_PLATES);
    // Hurt: the frame's edge goes red for a moment (LOOK.md 13).
    if hurt > 0.0 {
        for (k, part) in [0.035_f32, 0.07, 0.11].into_iter().enumerate() {
            let (tx, ty) = (w * part, h * part);
            let red = [0.85, 0.05, 0.03, 0.22 * hurt / (k as f32 + 1.0)];
            hud.rect(0.0, 0.0, w, ty, red);
            hud.rect(0.0, h - ty, w, ty, red);
            hud.rect(0.0, ty, tx, h - 2.0 * ty, red);
            hud.rect(w - tx, ty, tx, h - 2.0 * ty, red);
        }
    }
    // The names over the bodies, under everything else of the HUD: small print, a shade
    // behind it, the health under it where it is known.
    {
        let print = (s * 0.5).max(1.0);
        for tag in tags {
            let Some((x, y)) = project(vp, hud.size, tag.at) else {
                continue;
            };
            let tw = hud.width(print, &tag.name);
            let (tx, ty) = ((x - tw * 0.5).round(), (y - (cap + 3.0) * print).round());
            hud.print(tx + 1.0, ty + 1.0, print, [0.0, 0.0, 0.0, 0.75], &tag.name);
            hud.print(tx, ty, print, tag.ink, &tag.name);
            if let Some(frac) = tag.health {
                let bw = (40.0 * print).max(tw * 0.8);
                hud.bar(
                    (x - bw * 0.5).round(),
                    y.round(),
                    bw,
                    3.0 * print,
                    frac,
                    tag.ink,
                );
            }
        }
    }
    // The numbers (LOOK.md 13.8): in the title face over the head they belong to, what
    // the own hand dealt the largest; each floats up as it fades, with a shade behind.
    // The own hurts in the first person have no head in the frame: they fall from
    // under the aim instead, where the eyes are.
    {
        use crate::fx::Blow;
        let face = if hud.has_face(hud::FaceId::Title) {
            hud::FaceId::Title
        } else {
            hud.words()
        };
        let (_, ascent) = hud.metrics(face);
        for pop in pops {
            let own = matches!(pop.blow, Blow::Taken | Blow::Healed);
            let (x, y) = if own && first_person {
                (w * 0.5, h * 0.5 + 100.0 * s - pop.lift * 40.0 * s)
            } else {
                let Some((x, y)) = project(vp, hud.size, pop.at) else {
                    continue;
                };
                (x, y - pop.lift * 44.0 * s)
            };
            let print = match pop.blow {
                Blow::Dealt | Blow::Mended => s * 1.4,
                Blow::Taken | Blow::Healed => s,
                Blow::Blocked => (s * 0.6).max(1.0),
            };
            let tw = hud.width_in(face, print, &pop.text);
            let (tx, ty) = ((x - tw * 0.5).round(), (y - ascent * print).round());
            let shade = [0.0, 0.0, 0.0, 0.8 * pop.ink[3]];
            hud.text_in(face, tx + 1.0, ty + 1.0, print, shade, &pop.text);
            hud.text_in(face, tx, ty, print, pop.ink, &pop.text);
        }
    }
    // The aim: a dot in the middle.
    hud.rect(w * 0.5 - 2.0, h * 0.5 - 2.0, 4.0, 4.0, hud::SHADE);
    hud.rect(w * 0.5 - 1.0, h * 0.5 - 1.0, 2.0, 2.0, hud::WHITE);
    // The combo counter (MODES.md 4.6), right of the aim: two hits or more within two
    // seconds of each other, fading over the two seconds after the last.
    if combo.0 >= 2 && combo.1 < COMBO_SECS {
        let fade = 1.0 - combo.1 / COMBO_SECS;
        let text = format!("{} hits", combo.0);
        let print = s * 1.2;
        let ink = [1.0, 0.85, 0.3, fade];
        hud.text(
            w * 0.5 + 18.0 * s + 1.0,
            h * 0.5 - 8.0 * s + 1.0,
            print,
            [0.0, 0.0, 0.0, 0.8 * fade],
            &text,
        );
        hud.text(w * 0.5 + 18.0 * s, h * 0.5 - 8.0 * s, print, ink, &text);
    }
    let Some(o) = online else { return };
    let Some(c) = &o.client else { return };

    // The gun mode (MODES.md 3.8): the crosshair opens with the cone, four lines whose
    // gap is the cone's angle on the screen; the magazine over the reserve bottom right,
    // in red below half a magazine, "reloading" while the hands are at it; the scope's
    // mask and its lines.
    if let Some((f, g)) = c.mover.gun_in_hand(&c.sheet.kit) {
        let now = c.tick;
        let cone = gm_core::sim::cone_deg(
            f,
            c.sheet.derived.max_speed,
            &c.mover,
            g,
            crouched,
            scoped,
            g.spray,
            now,
        );
        let per_deg = h / (crate::render::fov_y_deg() / zoom.max(1.0));
        let gap = (cone * per_deg).max(4.0 * s);
        let len = 6.0 * s;
        let (cx, cy) = (w * 0.5, h * 0.5);
        for (dx, dy, lw, lh) in [
            (-gap - len, -s * 0.5, len, s),
            (gap, -s * 0.5, len, s),
            (-s * 0.5, -gap - len, s, len),
            (-s * 0.5, gap, s, len),
        ] {
            hud.rect(cx + dx - 1.0, cy + dy - 1.0, lw + 2.0, lh + 2.0, hud::SHADE);
            hud.rect(cx + dx, cy + dy, lw, lh, hud::WHITE);
        }
        if zoom > 1.0 {
            let r = h * 0.42;
            let mask = [0.0, 0.0, 0.0, 0.92];
            hud.rect(0.0, 0.0, w, cy - r, mask);
            hud.rect(0.0, cy + r, w, h - cy - r, mask);
            hud.rect(0.0, cy - r, cx - r, 2.0 * r, mask);
            hud.rect(cx + r, cy - r, w - cx - r, 2.0 * r, mask);
            hud.rect(cx - r, cy - s * 0.5, 2.0 * r, s, [0.0, 0.0, 0.0, 0.6]);
            hud.rect(cx - s * 0.5, cy - r, s, 2.0 * r, [0.0, 0.0, 0.0, 0.6]);
        }
        let low = (g.magazine as u32) * 2 < f.magazine as u32 || g.magazine == 0;
        let ink = if low { hud::RED } else { hud::WHITE };
        let text = format!("{} / {}", g.magazine, g.reserve);
        let print = s * 2.0;
        let tw = hud.width(print, &text);
        // (Above the corner's "Esc menu" line.)
        let (x, y) = (w - 16.0 - tw, h - 16.0 - line - cap * print);
        hud.print(x + 1.0, y + 1.0, print, hud::SHADE, &text);
        hud.print(x, y, print, ink, &text);
        if c.mover.reloading(now) {
            let word = "reloading";
            let tw = hud.width(s, word);
            hud.label(w - 16.0 - tw, y - line, s, hud::YELLOW, word);
        }
    }
    // The kits carried (MODES.md 11.3), bottom right in every mode, above the ammo where
    // there is ammo: `F` uses one; "using a kit" while the hands are at it.
    {
        let gun = c.mover.gun_in_hand(&c.sheet.kit).is_some();
        let base = h
            - 16.0
            - line
            - if gun {
                cap * s * 2.0 + line * 2.0
            } else {
                line
            };
        let text = if c.mover.using_kit(c.tick) {
            "using a kit".to_string()
        } else {
            format!("kits {}  F", c.mover.kits)
        };
        let ink = if c.mover.using_kit(c.tick) {
            hud::YELLOW
        } else if c.mover.kits == 0 {
            hud::SHADE
        } else {
            hud::WHITE
        };
        let tw = hud.width(s, &text);
        hud.label(w - 16.0 - tw, base, s, ink, &text);
    }

    // The own body, top left (LOOK.md 3.1): a portrait in its frame, the name, the three
    // bars; without a skin, the bars alone as before, bottom left.
    let max_health = c.sheet.derived.health.max(1) as f32;
    let rows: [(&str, f32, f32, [f32; 4]); 3] = [
        ("hp", c.own_health.max(0) as f32, max_health, hud::RED),
        (
            "st",
            c.mover.stamina,
            c.sheet.derived.stamina.max(1.0),
            hud::YELLOW,
        ),
        (
            "fo",
            c.mover.focus,
            c.sheet.derived.focus.max(1.0),
            hud::BLUE,
        ),
    ];
    let mut y = if skinned {
        16.0
    } else {
        h - 16.0 - line * rows.len() as f32
    };
    if c.mover.commanding(c.tick) {
        let at = if skinned {
            h - 16.0 - 60.0 * s
        } else {
            y - line
        };
        hud.label(16.0, at, s, hud::YELLOW, "command stance");
    }
    if skinned {
        let side = 48.0 * s;
        let build = &c.sheet.build;
        let portrait = manifest.and_then(|m| {
            m.portrait(&format!(
                "{}_{}",
                gm_model::rig::frame_name(build.frame),
                build.armour.name()
            ))
            .map(str::to_string)
        });
        hud.frame(16.0, y, side, side, "portrait_frame", s, hud::PLAIN);
        if let Some(key) = &portrait {
            hud.icon(
                16.0 + 6.0 * s,
                y + 6.0 * s,
                side - 12.0 * s,
                key,
                hud::PLAIN,
            );
        }
        let x = 16.0 + side + 6.0 * s;
        let (lh, _) = hud.metrics(hud::FaceId::Text);
        // The name the zone announced the own body by, else what the command line said.
        let name = o
            .names
            .get(&c.my_id)
            .map(|n| n.0.as_str())
            .filter(|n| !n.is_empty())
            .unwrap_or(own_name);
        hud.text_in(hud::FaceId::Text, x, y, s, hud::WHITE, name);
        let mut by = y + lh * s + 2.0 * s;
        let bh = 10.0 * s;
        for (_, have, max, colour) in rows {
            hud.frame(x, by, 150.0 * s, bh + 4.0 * s, "bar_frame", s, hud::PLAIN);
            let inner_w = (150.0 * s - 6.0 * s) * (have / max).clamp(0.0, 1.0);
            if inner_w > 0.0 {
                hud.image(x + 3.0 * s, by + 2.0 * s, inner_w, bh, "bar_fill", colour);
            }
            // The numbers, with a shade under them: white on a yellow bar is not read.
            let text = format!("{:.0}/{:.0}", have, max);
            let tw = hud.width(s, &text);
            let (tx, ty) = (
                x + 150.0 * s - tw - 4.0 * s,
                by + 2.0 * s + (bh - cap * s) * 0.5,
            );
            hud.print(tx + 1.0, ty + 1.0, s, [0.0, 0.0, 0.0, 0.7], &text);
            hud.print(tx, ty, s, hud::WHITE, &text);
            by += bh + 6.0 * s;
        }
        y = by + 4.0 * s;
    } else {
        for (name, have, max, colour) in rows {
            hud.label(16.0, y, s, hud::WHITE, name);
            let x = 16.0 + hud.width(s, "hp ") + 4.0 * s;
            hud.bar(x, y + s, 180.0, cap * s - 2.0 * s, have / max, colour);
            hud.label(
                x + 188.0,
                y,
                s,
                hud::DIM,
                &format!("{:.0}/{:.0}", have, max),
            );
            y += line;
        }
        y = 16.0;
    }

    // The hotbar, bottom centre (LOOK.md 3.2): a cell per ability of the kit, its icon or
    // its glyph, its key, and its state from the predicted mover; the own statuses above it.
    if skinned {
        let cells = hotbar(o);
        let rects = hotbar_rects((w, h), s, cells.len());
        let side = 40.0 * s;
        let gap = 3.0 * s;
        let (x0, y0) = rects.first().map_or((0.0, h - 16.0 - side), |r| (r.x, r.y));
        // The cells without a picture show their ability's name (LOOK.md 3.4), whole and
        // all in one size: small print when any of them is wider than a cell.
        let name_room = side - 8.0 * s;
        let name_print = if cells.iter().all(|c| hud.width(s, &c.ability) <= name_room) {
            s
        } else {
            (s * 0.5).max(1.0)
        };
        for (i, cell) in cells.iter().enumerate() {
            let x = x0 + i as f32 * (side + gap);
            hud.frame(x, y0, side, side, "hotbar_cell", s, hud::PLAIN);
            let icon = manifest
                .and_then(|m| m.ability(&cell.ability))
                .and_then(|a| a.icon.clone());
            let inner = side - 8.0 * s;
            let tint = match cell.state {
                "unaffordable" | "silenced" => [0.45, 0.45, 0.5, 1.0],
                _ => hud::PLAIN,
            };
            let drawn = icon
                .as_deref()
                .is_some_and(|k| hud.icon(x + 4.0 * s, y0 + 4.0 * s, inner, k, tint));
            if !drawn {
                let print = name_print;
                let mut short = cell.ability.clone();
                while hud.width(print, &short) > inner && short.pop().is_some() {}
                let tw = hud.width(print, &short);
                hud.print(
                    x + (side - tw) * 0.5,
                    y0 + (side - cap * print) * 0.5,
                    print,
                    if tint == hud::PLAIN {
                        hud::WHITE
                    } else {
                        hud::DIM
                    },
                    &short,
                );
            }
            match cell.state {
                "cooling" => {
                    // The dark sweep, clockwise from twelve, over what is left; the seconds
                    // when more than one.
                    let centre = Vec2::new(x + side * 0.5, y0 + side * 0.5);
                    hud.wedge(centre, side * 0.72, cell.ready, 1.0, [0.0, 0.0, 0.0, 0.62]);
                    if cell.left_secs >= 1.0 {
                        let t = format!("{:.0}", cell.left_secs.ceil());
                        let tw = hud.width(s, &t);
                        hud.print(
                            x + (side - tw) * 0.5,
                            y0 + (side - cap * s) * 0.5,
                            s,
                            hud::WHITE,
                            &t,
                        );
                    }
                }
                "silenced" => {
                    hud.rect(
                        x + 4.0 * s,
                        y0 + side * 0.5 - s,
                        side - 8.0 * s,
                        2.0 * s,
                        hud::RED,
                    );
                }
                "active" => {
                    let rim = [1.0, 0.85, 0.3, 1.0];
                    hud.rect(x, y0, side, s, rim);
                    hud.rect(x, y0 + side - s, side, s, rim);
                    hud.rect(x, y0, s, side, rim);
                    hud.rect(x + side - s, y0, s, side, rim);
                }
                _ => {}
            }
            // A chain's stage (MODES.md 4.3), bottom right of the cell.
            if cell.stage >= 2 {
                let numeral = ["", "I", "II", "III", "IV", "V"]
                    .get(cell.stage as usize)
                    .copied()
                    .unwrap_or("V+");
                let tw = hud.width(s, numeral);
                hud.print(
                    x + side - tw - 3.0 * s,
                    y0 + side - (cap + 2.0) * s,
                    s,
                    [1.0, 0.85, 0.3, 1.0],
                    numeral,
                );
            }
            // The key, in its tab at the top left of the cell.
            let kw = hud.width(s, cell.key) + 4.0 * s;
            hud.frame(
                x - s,
                y0 - 4.0 * s,
                kw,
                (cap + 4.0) * s,
                "hotbar_key",
                s,
                hud::PLAIN,
            );
            hud.print(x + s, y0 - 2.0 * s, s, hud::WHITE, cell.key);
        }
        // The statuses (LOOK.md 3.3), above the hotbar: an icon or the status's name, a
        // ring of the time left as a wedge, the stacks.
        let now = c.tick;
        let mut sx = x0;
        let sy = y0 - 30.0 * s;
        for slot in c.mover.statuses.active() {
            let Some(status) = slot.status else { continue };
            let left = gm_core::sim::tick_delta(slot.until, now).max(0) as f32;
            let side = 24.0 * s;
            let harmful = !matches!(
                status,
                gm_core::vocab::Status::Regen
                    | gm_core::vocab::Status::Haste
                    | gm_core::vocab::Status::Fortify
                    | gm_core::vocab::Status::Stealth
            );
            hud.rect(
                sx,
                sy,
                side,
                side,
                if harmful {
                    [0.35, 0.08, 0.06, 0.85]
                } else {
                    [0.08, 0.3, 0.1, 0.85]
                },
            );
            let icon = manifest.and_then(|m| m.status_icon(status.name()).map(str::to_string));
            let drawn = icon.as_deref().is_some_and(|k| {
                hud.icon(sx + 2.0 * s, sy + 2.0 * s, side - 4.0 * s, k, hud::PLAIN)
            });
            if !drawn {
                let short = fit(hud, status.name(), side - 4.0 * s);
                hud.print(sx + 2.0 * s, sy + 2.0 * s, s, hud::WHITE, &short);
            }
            // The time left, as a sweep that empties: a status of 5 s is nearly whole at 4.
            let frac = (left / 320.0).clamp(0.0, 1.0);
            hud.wedge(
                Vec2::new(sx + side * 0.5, sy + side * 0.5),
                side * 0.6,
                frac,
                1.0,
                [0.0, 0.0, 0.0, 0.5],
            );
            if slot.stacks > 1 {
                let t = slot.stacks.to_string();
                let tw = hud.width(s, &t);
                hud.print(
                    sx + side - tw - s,
                    sy + side - cap * s - s,
                    s,
                    hud::YELLOW,
                    &t,
                );
            }
            sx += side + 3.0 * s;
        }
        let _ = time;
    }

    // The squad, top left under the own body: slot, name, role, order, and its health.
    for (i, m) in o.squad.iter().enumerate() {
        let (health, alive) = squad_view.get(i).copied().unwrap_or((None, false));
        let colour = if alive { hud::WHITE } else { hud::RED };
        let text = format!(
            "{} {}  {}  {}",
            i + 1,
            m.name,
            role_name(m.role),
            if alive { order_name(&m.order) } else { "down" }
        );
        hud.label(16.0, y, s, colour, &text);
        y += line;
        let frac = health.map_or(0.0, |v| v as f32 / m.max_health.max(1) as f32);
        hud.bar(16.0, y - 2.0 * s, 150.0, 3.0 * s, frac, hud::GREEN);
        y += 6.0 * s;
    }
    // The party, under the squad (PARTY.md 8.2, LOOK.md 3.1): each member's health when
    // the wire carries it (its body is here, in sight, and of the party in this fight), its
    // name alone when its body is here and the wire does not, `away` when it is not here;
    // with a skin, each in a small frame with a bar.
    for (name, here) in party {
        let (text, colour) = match here {
            Some(Some(0)) => (format!("{name}  down"), hud::RED),
            Some(Some(health)) => (format!("{name}  {health}"), hud::WHITE),
            Some(None) => (name.clone(), hud::WHITE),
            None => (format!("{name}  away"), hud::DIM),
        };
        if skinned {
            let fw = 150.0 * s;
            let fh = 22.0 * s;
            hud.frame(16.0, y, fw, fh, "well", s, hud::PLAIN);
            if let Some(Some(health)) = here {
                let max = 400.0f32;
                let frac = (*health as f32 / max).clamp(0.0, 1.0);
                hud.image(
                    16.0 + 3.0 * s,
                    y + fh - 7.0 * s,
                    (fw - 6.0 * s) * frac,
                    4.0 * s,
                    "bar_fill",
                    hud::GREEN,
                );
            }
            hud.print(16.0 + 4.0 * s, y + 3.0 * s, s, colour, &text);
            y += fh + 3.0 * s;
        } else {
            hud.label(16.0, y, s, colour, &text);
            y += line;
        }
    }

    // The creature being fought, top middle; messages under it.
    let mut y = 16.0;
    if let Some((name, health, max)) = target {
        let tw = hud.width(s, name);
        hud.label((w - tw) * 0.5, y, s, hud::WHITE, name);
        y += line;
        let bw = (w * 0.34).min(520.0);
        hud.bar(
            (w - bw) * 0.5,
            y,
            bw,
            5.0 * s,
            *health as f32 / (*max).max(1) as f32,
            hud::RED,
        );
        let count = format!("{health}/{max}");
        let small = (s * 0.5).max(1.0);
        hud.print(
            (w - hud.width(small, &count)) * 0.5,
            y + 0.75 * s,
            small,
            hud::WHITE,
            &count,
        );
        y += 5.0 * s + 10.0;
    }
    for (at, text, colour) in &o.messages {
        if at.elapsed().as_secs_f32() > MESSAGE_SECS {
            continue;
        }
        // A long line is cut to the screen: the log has the whole of it.
        let shown = fit(hud, text, w - 40.0);
        let tw = hud.width(s, &shown);
        hud.label((w - tw) * 0.5, y, s, *colour, &shown);
        y += line;
    }
}

impl App {
    /// Give up: say why (on the page too, in a browser) and leave the loop.
    fn fail(&mut self, event_loop: &ActiveEventLoop, why: &str) {
        log::error!("{why}");
        #[cfg(target_arch = "wasm32")]
        crate::web::tell_page("error", why);
        self.exit_requested = true;
        event_loop.exit();
    }

    /// The window has its device: configure the surface and build the renderer on it.
    fn activate(
        &mut self,
        window: Arc<Window>,
        surface: wgpu::Surface<'static>,
        gpu: Gpu,
    ) -> Result<(), Error> {
        let size = frame_size(&window);
        let caps = surface.get_capabilities(&gpu.adapter);
        let mut config = surface
            .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
            .ok_or("surface not supported by the adapter")?;
        // Frames are drawn in sRGB. Where the surface has no sRGB format of its own (a
        // WebGPU canvas is `bgra8unorm`), its sRGB twin is asked for as a view format.
        let view_format = match caps.formats.iter().copied().find(|f| f.is_srgb()) {
            Some(format) => {
                config.format = format;
                format
            }
            None => {
                config.format = caps.formats[0];
                let srgb = config.format.add_srgb_suffix();
                if srgb != config.format {
                    config.view_formats.push(srgb);
                }
                srgb
            }
        };
        // Mailbox is the default: no tearing, no blocking, and it works where Fifo stalls
        // (X11 with a compositing window manager on RADV presented one frame per second).
        // Benchmarks measure throughput, so they run without vsync unless told otherwise.
        let bench = self.opts.bench_frames.is_some();
        config.present_mode = match self.opts.present {
            Some(mode) if caps.present_modes.contains(&mode) => mode,
            Some(mode) => {
                log::warn!(
                    "present mode {mode:?} unsupported here (available: {:?}); using the default",
                    caps.present_modes
                );
                wgpu::PresentMode::AutoVsync
            }
            // A browser presents with the display, whatever is asked.
            None if cfg!(target_arch = "wasm32") => wgpu::PresentMode::AutoVsync,
            None if bench || !self.opts.vsync => wgpu::PresentMode::AutoNoVsync,
            None if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) => {
                wgpu::PresentMode::Mailbox
            }
            None => wgpu::PresentMode::AutoVsync,
        };
        log::info!("present modes available: {:?}", caps.present_modes);
        config.desired_maximum_frame_latency = 2;
        surface.configure(&gpu.device, &config);
        log::info!(
            "surface {}x{} {:?} (drawn as {:?}) {:?}",
            config.width,
            config.height,
            config.format,
            view_format,
            config.present_mode
        );
        let mesh = self.mesh.take().ok_or("world mesh already consumed")?;
        let mut renderer = Renderer::new(&gpu, view_format, &mesh, (config.width, config.height));
        // The bundle's atlas is the HUD's texture from here on (LOOK.md 2.2).
        if let Some(atlas) = self.content.take_atlas() {
            renderer.hud.set_atlas(&gpu, atlas);
        }
        // Models come from the hub, on whatever session is logged in when one is wanted.
        let source = self.hub.as_ref().map(|h| h.model_source());
        let avatars = Avatars::new(
            &gpu,
            &mut renderer.characters,
            &self.opts,
            &self.bsp,
            source,
        )?;
        self.adapter_info = Some(gpu.info.clone());
        self.active = Some(Active {
            window: window.clone(),
            surface,
            config,
            view_format,
            gpu,
            renderer,
            avatars,
            drawn_from: vec![usize::MAX],
        });
        // `--prop KEY`: the own body and the crowd hold it, from the bundle (natively it
        // is on the GPU at once; in a browser it arrives later and nobody holds it).
        if let Some(key) = self.opts.prop.clone() {
            let a = self.active.as_mut().expect("just made");
            let (gpu, characters) = (&a.gpu, &mut a.renderer.characters);
            let slot = self
                .content
                .prop(&key, |model| Some(characters.add_model(gpu, model)));
            if slot.is_none() {
                log::warn!("--prop {key}: the bundle has no such prop on the desktop");
            }
            self.offline_prop = slot;
            a.avatars.crowd_prop = slot;
        }
        if let Some(key) = self.opts.off.clone() {
            let a = self.active.as_mut().expect("just made");
            let (gpu, characters) = (&a.gpu, &mut a.renderer.characters);
            let slot = self
                .content
                .prop(&key, |model| Some(characters.add_model(gpu, model)));
            if slot.is_none() {
                log::warn!("--off {key}: the bundle has no such prop on the desktop");
            }
            self.offline_off = slot;
            a.avatars.crowd_off = slot;
        }
        // A browser gives the pointer only to a click (WEB.md 3.4): there the first click grabs.
        if cfg!(not(target_arch = "wasm32")) && self.wants_pointer() && self.focused() {
            self.set_grab(true);
        }
        self.was_up = self.screen_up();
        self.apply_settings();
        self.last_frame = Instant::now();
        self.started = Instant::now();
        // Offline there is nothing left to wait for; online the status clears at `Welcome`.
        #[cfg(target_arch = "wasm32")]
        if self.online.is_none() {
            crate::web::tell_page("status", "");
        }
        window.request_redraw();
        Ok(())
    }

    /// A screen has the pointer and the keys: one before the game, or the game menu
    /// (CLIENT.md 6).
    fn screen_up(&self) -> bool {
        self.front_up
            || self.menu.is_some()
            || self.bag.is_some()
            || self.people.is_some()
            || self.gm_page.is_some()
            || self.character_page.is_some()
            || self.title.is_some()
    }

    /// The hub and the session and character it is asked about, while one is played.
    fn owner(
        &self,
    ) -> Option<(
        &Hub,
        gm_hub_proto::protocol::SessionId,
        gm_hub_proto::protocol::CharacterId,
    )> {
        let playing = self.online.as_ref().is_some_and(|o| o.client.is_some());
        match (&self.hub, &self.account, self.character) {
            (Some(hub), Some(account), Some(character)) if playing => {
                Some((hub, account.session, character))
            }
            _ => None,
        }
    }

    /// The stall the body stands at (ITEMS.md 5): the nearest one in reach, by the rule
    /// the zone decides a purchase with.
    fn stall_in_reach(online: Option<&Online>) -> Option<&StallEntry> {
        let o = online?;
        let c = o.client.as_ref()?;
        let feet = c.mover.mv.origin + Vec3::Z * Hull::Player.mins().z;
        let far = |s: &StallEntry| (Vec3::from(s.pos) - feet).truncate().length_squared();
        o.stalls
            .iter()
            .filter(|s| gm_net::control::stall_in_reach(s.pos, feet.into()))
            .min_by(|a, b| far(a).total_cmp(&far(b)))
    }

    /// `I`: the inventory of the character being played.
    fn open_inventory(&mut self) {
        match self.owner() {
            Some((hub, session, character)) => {
                self.bag = Some(Bag::inventory(hub, session, character, Instant::now()));
                self.menu = None;
                self.release_keys();
            }
            None => self.note("there is no inventory without a hub"),
        }
    }

    /// `P`: the people here and of the party. (Without a hub there are no parties, and
    /// the zone says so to whoever asks.)
    fn open_people(&mut self) {
        if !self.online.as_ref().is_some_and(|o| o.client.is_some()) {
            return;
        }
        // (Without a hub the page still says who is here; it asks the hub nothing.)
        let nobody = (gm_hub_proto::protocol::SessionId([0; 16]), 0);
        let (session, character) = self.owner().map_or(nobody, |(_, s, c)| (s, c));
        self.people = Some(People::here(session, character, Instant::now()));
        self.menu = None;
        self.bag = None;
        self.release_keys();
    }

    /// `K`: the character's page (MATRIX.md 9.1): the thirty points and the kit, worn at
    /// the trainer.
    fn open_character(&mut self) {
        let Some(o) = &self.online else { return };
        if o.client.is_none() {
            return;
        }
        self.character_page = Some(CharacterPage::default());
        self.menu = None;
        self.bag = None;
        self.people = None;
        self.gm_page = None;
        self.release_keys();
    }

    /// What the character's page asked for: the zone is asked, and answers
    /// (`RespecResult`, shown as the page's note).
    fn character_act(&mut self, action: CharacterAction) {
        match action {
            CharacterAction::None => {}
            CharacterAction::Close => self.character_page = None,
            CharacterAction::Wear(build) => {
                if let Some(o) = &mut self.online {
                    o.respec_note = "asked".into();
                    o.net
                        .send_control(FromClient::Respec(BuildChoice::Custom(build)));
                }
            }
        }
    }

    /// Standing by the trainer (TRAINER_REACH of a body nothing hurts), where a build is
    /// worn in the world; in a team zone (the arena) anywhere, at the next respawn.
    fn at_trainer(&self) -> bool {
        let Some(o) = &self.online else { return false };
        let (Some(c), Some(pack)) = (&o.client, &o.pack) else {
            return false;
        };
        // The client is not told whether the zone is of the world: one with somebody
        // nothing hurts in it has a trainer; one without (the arena) wears a build at
        // the next respawn; the dungeon has neither, and the zone says so when asked.
        let npc = |id: &u32| {
            matches!(o.kinds.get(id), Some(BodyKind::Creature { def })
                if pack.creatures.get(*def as usize).is_some_and(|d| d.npc))
        };
        if !o.kinds.keys().any(npc) {
            return true;
        }
        let me = c.mover.mv.origin;
        c.others_at(c.render_tick(0.0))
            .iter()
            .any(|e| e.alive() && npc(&e.id) && (e.pos - me).length() <= TRAINER_REACH)
    }

    /// `G`: the game master's page (GM.md 4), for a character the zone granted it to.
    fn open_gm(&mut self) {
        let Some(o) = &self.online else { return };
        if o.client.is_none() {
            return;
        }
        if !o.gm {
            self.note("the zone did not make this character a game master");
            return;
        }
        self.gm_page = Some(GmPage::default());
        self.menu = None;
        self.bag = None;
        self.people = None;
        self.character_page = None;
        self.release_keys();
    }

    /// What the game master's page asked for: the zone is asked, and answers.
    fn gm_act(&mut self, action: GmAction) {
        match action {
            GmAction::None => {}
            GmAction::Close => self.gm_page = None,
            GmAction::Send(op) => {
                if let Some(o) = &mut self.online {
                    o.gm_note = "asked".into();
                    o.net.send_control(FromClient::Gm(op));
                }
            }
        }
    }

    /// What the page of people asked for.
    fn people_act(&mut self, action: PeopleAction) {
        match action {
            PeopleAction::None => {}
            PeopleAction::Close => self.people = None,
            PeopleAction::Zone(say) => {
                if let Some(o) = &mut self.online {
                    o.net.send_control(say);
                }
            }
            PeopleAction::Whisper(name) => {
                self.people = None;
                self.chat.open_with(format!("/w {name} "));
            }
        }
    }

    /// `E`: what the stall the body stands at has for sale.
    fn open_stall(&mut self) {
        let near = Self::stall_in_reach(self.online.as_ref());
        let Some((id, owner)) = near.map(|s| (s.id, s.owner.clone())) else {
            return;
        };
        match self.owner() {
            Some((hub, session, character)) => {
                let now = Instant::now();
                self.bag = Some(Bag::stall(hub, session, character, id, &owner, now));
                self.menu = None;
                self.release_keys();
            }
            None => self.note("there is nothing to look at without a hub"),
        }
    }

    /// A word in the window's title and the log, as the stall keys say theirs.
    fn note(&mut self, text: &str) {
        log::info!("{text}");
        if let Some(o) = &mut self.online {
            o.respec_note = text.to_string();
        }
    }

    /// The keys are the toolkit's: a screen is up, or the chat line is open.
    fn capturing(&self) -> bool {
        self.screen_up() || self.chat.open
    }

    /// Whether the game would hold the pointer for mouse look if it had the window.
    /// The character plays the RPG mode (MODES.md 5): the pointer is free, the camera
    /// turns while the secondary button is held.
    fn rpg_mode(&self) -> bool {
        self.online
            .as_ref()
            .and_then(|o| o.client.as_ref())
            .is_some_and(|c| c.sheet.kit.mode == gm_core::vocab::Mode::Rpg)
    }

    fn wants_pointer(&self) -> bool {
        !self.rpg_mode()
            && !self.screen_up()
            && self.opts.bench_frames.is_none()
            && self.opts.script.is_none()
            && self.opts.replay.is_none()
    }

    /// The window has the keyboard. The pointer is taken by itself only then: a zone that
    /// lets the character in while the person looks at another window does not confine
    /// their pointer to this one. (A click on the window takes it whatever the focus.)
    fn focused(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.window.has_focus())
    }

    /// A person is at the client: when a zone's connection ends, the characters are shown
    /// again. A client on autopilot or joined directly ends with its connection.
    fn returns_to_screens(&self) -> bool {
        self.front.as_ref().is_some_and(|f| !f.on_autopilot())
    }

    /// Keys held when the toolkit takes the keyboard are not held for the game any more.
    fn release_keys(&mut self) {
        self.input.keys.clear();
        self.input.just_pressed.clear();
        self.input.mouse.clear();
    }

    /// A key the screens act on was pressed, by a person or by a script (CLIENT.md 6).
    fn ui_key(&mut self, key: Key) {
        if self.capturing() {
            self.ui_input.keys.push(key);
            return;
        }
        // In the game: Enter opens the chat line, Escape the menu (a replay has the menu too:
        // it is where Quit is).
        match key {
            Key::Enter if self.online.as_ref().is_some_and(|o| o.client.is_some()) => {
                self.chat.open = true;
                self.release_keys();
            }
            // In the RPG mode a target is let go first (MODES.md 5.2).
            Key::Escape if self.rpg_mode() && self.rpg.target.is_some() => {
                self.rpg.clear_target();
            }
            Key::Escape if self.opts.bench_frames.is_none() => {
                self.menu = Some(GameMenu::default());
                self.release_keys();
            }
            Key::Tab if self.rpg_mode() => self.rpg_cycle(),
            Key::Inventory => self.open_inventory(),
            Key::People => self.open_people(),
            Key::Use => self.open_stall(),
            Key::Gm => self.open_gm(),
            Key::Character => self.open_character(),
            _ => {}
        }
    }

    /// Characters were typed.
    fn ui_text(&mut self, text: &str) {
        if self.capturing() {
            self.ui_input
                .text
                .extend(text.chars().filter(|c| !c.is_control()));
        }
    }

    /// The left button went down at `at` while a screen is up.
    fn ui_press(&mut self, at: (f32, f32), double: bool) {
        self.cursor = at;
        self.ui_input.pressed = true;
        self.ui_input.down = true;
        self.ui_input.double = double;
    }

    fn ui_release(&mut self) {
        self.ui_input.released = true;
        self.ui_input.down = false;
    }

    /// What a person changed is written down; a run that changed nothing leaves the
    /// file alone (CLIENT.md 8).
    fn keep_settings(&mut self) {
        if self.settings == self.settings_kept {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = &self.settings_path {
            self.settings.save(path);
        }
        #[cfg(target_arch = "wasm32")]
        self.settings.save();
        self.settings_kept = self.settings.clone();
    }

    /// What the settings say, done: called when one changes and when the window is ready.
    fn apply_settings(&mut self) {
        self.sound
            .set_volume(self.settings.volume, self.settings.mute);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(a) = &self.active {
            let wanted = self
                .settings
                .fullscreen
                .then_some(winit::window::Fullscreen::Borderless(None));
            if a.window.fullscreen().is_some() != wanted.is_some() {
                a.window.set_fullscreen(wanted);
            }
        }
    }

    /// The zone is left (the person chose to, or its connection ended): the characters are
    /// shown again, with the reason if there is one.
    fn leave_zone(&mut self, why: &str) {
        self.hang_up();
        self.sound.quiet();
        self.menu = None;
        self.bag = None;
        self.people = None;
        self.gm_page = None;
        self.character = None;
        self.chat.clear();
        self.entities.clear();
        self.bodies.clear();
        self.squad_view.clear();
        self.party_view.clear();
        self.target_view = None;
        if let Some(front) = &mut self.front {
            front.back_to_characters(why);
            self.front_up = true;
        }
    }

    /// Hang up on the zone without waiting for it: the goodbye is said on the connection's
    /// own thread while the frames go on.
    fn hang_up(&mut self) {
        // (The map that is loaded is whatever `switch_map` last loaded: a zone that was
        // left while its map was still on the way has not changed it.)
        if let Some(mut o) = self.online.take() {
            o.net.hang_up();
            self.leaving.push((o.net, Instant::now()));
        }
        // A map still being fetched for that zone arrives in a slot nobody reads.
        #[cfg(target_arch = "wasm32")]
        {
            self.pending_fetch = Default::default();
        }
    }

    /// Ctrl+V: what the clipboard holds goes where typing would (CLIENT.md 3). In a browser
    /// the clipboard is the page's, and the page's own form is where a password goes.
    #[cfg(not(target_arch = "wasm32"))]
    fn paste(&mut self) {
        /// More than any field takes.
        const MOST: usize = 1024;
        let (tx, rx) = std::sync::mpsc::channel();
        let read = std::thread::Builder::new()
            .name("gm-paste".into())
            .spawn(
                move || match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    Ok(text) => {
                        let _ = tx.send(text.chars().take(MOST).collect());
                    }
                    Err(e) => log::debug!("nothing to paste: {e}"),
                },
            );
        if read.is_ok() {
            self.pasting = Some(rx);
        }
    }

    /// What a paste brought, typed where the keyboard is now.
    #[cfg(not(target_arch = "wasm32"))]
    fn pasted(&mut self) {
        let Some(rx) = &self.pasting else { return };
        match rx.try_recv() {
            Ok(text) => {
                self.pasting = None;
                self.ui_text(&text);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pasting = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// The hub gave a ticket: connect to its zone as the character it is for.
    fn enter_zone(
        &mut self,
        ticket: gm_hub_proto::protocol::ZoneTicket,
        name: String,
        event_loop: &ActiveEventLoop,
    ) {
        log::info!("hub ticket for zone {} at {}", ticket.zone, ticket.addr);
        let reachable = if cfg!(target_arch = "wasm32") {
            ticket.web.is_some()
        } else {
            true
        };
        let entered = if reachable {
            self.opts.name = name.clone();
            let entry = Entry {
                zone: ticket_addr(&ticket),
                token: bitcode::encode(&ticket.token),
                zone_name: ticket.zone.clone(),
            };
            // What will be loaded when the zone answers: a map already on its way in.
            let loaded = self.pending_map.as_ref().map_or(self.map_hash, |m| m.1);
            online(&self.opts, &self.sim, loaded, entry).map_err(|e| e.to_string())
        } else {
            Err(format!("zone {} has no web listener", ticket.zone))
        };
        match entered {
            Ok(online) => {
                self.online = Some(online);
                self.character = Some(ticket.token.payload.character);
                // Remembered for a person; a command line that named it leaves no trace.
                if self.returns_to_screens() {
                    self.settings.character = name;
                }
            }
            Err(why) if self.returns_to_screens() => {
                if let Some(front) = &mut self.front {
                    front.back_to_characters(&why);
                }
            }
            Err(why) => self.fail(event_loop, &why),
        }
    }

    /// What the screens, the menu and the chat line asked for in this frame.
    /// What the inventory or the stall asked for: buying and wearing are said to the
    /// zone (ITEMS.md 5).
    fn bag_act(&mut self, action: BagAction) {
        let say = match action {
            BagAction::None => return,
            BagAction::Close => {
                self.bag = None;
                return;
            }
            BagAction::Buy {
                stall,
                listing,
                price,
            } => FromClient::StallBuy {
                stall,
                listing,
                price,
            },
            BagAction::Wear { item } => FromClient::Wear { item },
            BagAction::TakeOff { item } => FromClient::TakeOff { item },
        };
        if let Some(o) = &mut self.online {
            o.net.send_control(say);
        }
    }

    fn act(
        &mut self,
        front: Action,
        menu: MenuAction,
        said: Option<Said>,
        event_loop: &ActiveEventLoop,
    ) {
        if let (Some(said), Some(o)) = (said, &mut self.online) {
            // A line to the zone, to the party or to one character; and the two things
            // the chat line can ask of the party (PARTY.md 5).
            o.net.send_control(match said {
                Said::Say(text) => FromClient::Chat(text),
                Said::Party(text) => FromClient::PartySay(text),
                Said::Whisper { to, text } => FromClient::Whisper { to, text },
                Said::Invite(name) => {
                    o.social.asked = Some(Instant::now());
                    FromClient::PartyInvite { name }
                }
                Said::Leave => {
                    o.social.asked = Some(Instant::now());
                    FromClient::PartyLeave
                }
            });
        }
        match menu {
            MenuAction::None => {}
            MenuAction::Resume => self.menu = None,
            MenuAction::Inventory => self.open_inventory(),
            MenuAction::People => self.open_people(),
            MenuAction::Gm => self.open_gm(),
            MenuAction::Character => self.open_character(),
            MenuAction::Travel(zone) => {
                if let Some(o) = &mut self.online {
                    o.net.send_control(FromClient::Travel(zone.clone()));
                    o.say(format!("travel to {zone} requested"), hud::DIM);
                }
                self.menu = None;
            }
            MenuAction::Leave => self.leave_zone(""),
            MenuAction::Quit => event_loop.exit(),
            MenuAction::Changed => self.apply_settings(),
            // The tap that pressed the button is a gesture the browser still honours.
            MenuAction::PageFullscreen => {
                #[cfg(target_arch = "wasm32")]
                crate::web::tell_page("fullscreen", "toggle");
                self.menu = None;
            }
        }
        match front {
            Action::None => {}
            Action::Session(account) => {
                if let Some(hub) = &self.hub {
                    hub.set_session(account.as_ref().map(|a| a.session));
                }
                if let Some(a) = &account
                    && self.returns_to_screens()
                {
                    self.settings.email = a.email.clone();
                }
                self.account = account;
            }
            Action::Enter { ticket, name } => self.enter_zone(*ticket, name, event_loop),
            Action::CancelEntering => self.hang_up(),
            Action::Quit => event_loop.exit(),
            Action::Failed(why) => self.fail(event_loop, &why),
        }
    }

    /// What only a page has (CLIENT.md 4.1, WEB.md 3.4): its own form is the login screen,
    /// and the browser gives and takes the pointer as it sees fit.
    #[cfg(target_arch = "wasm32")]
    fn page_frame(&mut self) {
        use crate::front::Screen;
        if let Some(front) = &mut self.front {
            let at_login = self.front_up && front.screen == Screen::Login && !front.on_autopilot();
            // The form is shown while the login screen is up, with the last refusal.
            let tell = match (at_login, front.busy()) {
                (true, false) => format!("login {}", front.notice()),
                (true, true) => "login-wait ".to_string(),
                (false, _) => "screen ".to_string(),
            };
            if tell != self.told {
                if let Some((kind, text)) = tell.split_once(' ') {
                    crate::web::tell_page(kind, text);
                }
                self.told = tell;
            }
            if at_login
                && !front.busy()
                && let Some((email, password, register)) = crate::web::take_login()
            {
                front.page_login(email, password, register);
            }
        }
        self.page_keyboard();
        // The pointer: Escape gives it back to the browser and never reaches the page, so
        // losing it is what Escape is here: the menu opens, or the chat line is dropped.
        let locked = crate::web::pointer_locked();
        if self.grabbed && !locked {
            let waited = self
                .grab_asked
                .is_none_or(|at| at.elapsed().as_secs_f32() > 0.5);
            if self.was_locked {
                self.grabbed = false;
                if self.chat.open {
                    self.chat.drop_line();
                } else if !self.screen_up() {
                    self.ui_key(Key::Escape);
                }
            } else if waited {
                // It was asked for and never given: the next click asks again.
                self.grabbed = false;
            }
        }
        // Given later than it was waited for: it is the game's all the same.
        if locked && !self.grabbed && self.wants_pointer() {
            self.grabbed = true;
        }
        self.was_locked = locked;
    }

    /// The phone's keyboard is the browser's (WEB.md 3.6): the page is told which text
    /// field of the canvas has the keys and what it holds, and where the fields are, so
    /// that a finger on one brings the keyboard up; what the keyboard typed comes back
    /// through the page as the keys and text a keyboard of the window's would have sent.
    #[cfg(target_arch = "wasm32")]
    fn page_keyboard(&mut self) {
        for typed in crate::web::take_typed() {
            match typed.strip_prefix('k') {
                Some(name) => {
                    if let Some(key) = Key::parse(name) {
                        self.ui_key(key);
                    }
                }
                None => {
                    if let Some(text) = typed.strip_prefix('t') {
                        self.ui_text(text);
                    }
                }
            }
        }
        let field = if self.capturing() {
            self.ui.typing_in().map(|label| {
                let head = format!("{label}: ");
                self.ui
                    .seen
                    .iter()
                    .filter(|s| s.kind == ui::SeenKind::Field)
                    .find_map(|s| s.text.strip_prefix(&head))
                    .unwrap_or("")
                    .to_string()
            })
        } else {
            None
        };
        if field != self.told_field {
            match &field {
                Some(value) => crate::web::tell_page("field", value),
                None => crate::web::tell_page("no-field", ""),
            }
            self.told_field = field;
        }
        // The fields' places, for a finger (a mouse needs no keyboard brought up).
        let mut rects = Vec::new();
        if self.fingers.seen && self.capturing() {
            let dpr = self
                .active
                .as_ref()
                .map_or(1.0, |a| a.window.scale_factor() as f32)
                .max(0.5);
            for s in self
                .ui
                .seen
                .iter()
                .filter(|s| s.kind == ui::SeenKind::Field)
            {
                rects.extend([
                    s.rect.x / dpr,
                    s.rect.y / dpr,
                    s.rect.w / dpr,
                    s.rect.h / dpr,
                ]);
            }
        }
        if rects != self.told_fields {
            crate::web::tell_fields(&rects);
            self.told_fields = rects;
        }
    }

    /// What the script does this frame goes in where a person's events would (CLIENT.md 9).
    /// `false`: the script failed and the program is ending.
    fn run_script(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let Some(script) = &mut self.ui_script else {
            return true;
        };
        if !std::mem::take(&mut self.ui_drawn) {
            return true;
        }
        let events = match script.step(&self.ui, Instant::now()) {
            Ok(events) => events,
            Err(e) => {
                self.fail(event_loop, &e);
                return false;
            }
        };
        for event in events {
            match event {
                crate::script::Event::Press { at, double } => self.ui_press(at, double),
                crate::script::Event::Release => self.ui_release(),
                crate::script::Event::Move(at) => self.cursor = at,
                crate::script::Event::Text(text) => self.ui_text(&text),
                crate::script::Event::Key(key) => self.ui_key(key),
                crate::script::Event::Say(text) => {
                    #[cfg(not(target_arch = "wasm32"))]
                    println!("ui-script: {text}");
                    #[cfg(target_arch = "wasm32")]
                    crate::web::tell_page("say", &text);
                }
                crate::script::Event::Quit => {
                    log::info!("ui-script: ok");
                    #[cfg(not(target_arch = "wasm32"))]
                    println!("ui-script: ok");
                    #[cfg(target_arch = "wasm32")]
                    {
                        if let Some(o) = &mut self.online {
                            o.net.close();
                        }
                        if let Some(hub) = &self.hub {
                            hub.logout();
                        }
                        crate::web::tell_page("done", "ui-script: ok");
                    }
                    event_loop.exit();
                }
            }
        }
        true
    }

    fn set_grab(&mut self, grab: bool) {
        let Some(a) = &self.active else { return };
        // In a browser the page asks for the pointer itself: a refusal there is an answer
        // the page takes, not an error left in the console.
        #[cfg(target_arch = "wasm32")]
        if grab {
            self.grab_asked = Some(Instant::now());
            crate::web::ask_for_pointer();
        } else {
            crate::web::give_pointer_back();
        }
        #[cfg(not(target_arch = "wasm32"))]
        if grab {
            self.grab_asked = Some(Instant::now());
            let ok = a
                .window
                .set_cursor_grab(CursorGrabMode::Confined)
                .or_else(|_| a.window.set_cursor_grab(CursorGrabMode::Locked))
                .is_ok();
            if !ok {
                log::warn!("cursor grab unsupported here; mouse look still works while focused");
            }
        } else {
            let _ = a.window.set_cursor_grab(CursorGrabMode::None);
        }
        a.window.set_cursor_visible(!grab);
        self.grabbed = grab;
    }

    fn configure_surface(&mut self) {
        let Some(a) = &mut self.active else { return };
        let size = frame_size(&a.window);
        if size.width == 0 || size.height == 0 {
            return;
        }
        a.config.width = size.width;
        a.config.height = size.height;
        a.surface.configure(&a.gpu.device, &a.config);
        a.renderer.resize(&a.gpu, (size.width, size.height));
    }

    /// F1–F4 ask the zone for the preset builds, applied at the next respawn.
    fn respec_hotkeys(&mut self) {
        let Some(o) = self.online.as_mut() else {
            return;
        };
        let Some(pack) = &o.pack else { return };
        // T asks to travel to the zone named on the command line; a scripted run asks by
        // itself, once, after `travel_after` seconds.
        let timed = self.opts.travel_after > 0.0
            && self.started.elapsed().as_secs_f32() >= self.opts.travel_after;
        if (self.input.just_pressed.remove(&KeyCode::KeyT) || timed)
            && let Some(target) = self.opts.travel_to.clone()
        {
            if timed {
                self.opts.travel_after = 0.0;
            }
            o.net.send_control(FromClient::Travel(target.clone()));
            o.respec_note = format!("travel to {target} requested");
            log::info!("{}", o.respec_note);
        }
        // F9 reports the player nearest the crosshair (ANTICHEAT.md 5): the zone keeps the
        // last half minute and the next ten seconds for a moderator.
        if self.input.just_pressed.remove(&KeyCode::F9)
            && let Some(c) = &o.client
        {
            let eye = c.mover.eye();
            let view = view_dir(self.sim.yaw, self.sim.pitch);
            let t = c.render_tick(0.0);
            let aimed = c
                .others_at(t)
                .into_iter()
                .filter(|e| e.kind == EntityKind::Player)
                .filter(|e| matches!(o.kinds.get(&e.id), Some(BodyKind::Human) | None))
                .map(|e| (e.id, view.dot((e.pos - eye).normalize_or_zero())))
                .filter(|(_, facing)| *facing > 0.94)
                .max_by(|a, b| a.1.total_cmp(&b.1));
            match aimed {
                Some((target, _)) => {
                    o.net.send_control(FromClient::Report {
                        target,
                        reason: gm_net::control::ReportReason::Other,
                    });
                    o.respec_note = "report sent".into();
                }
                None => o.respec_note = "look at the player to report, then F9".into(),
            }
        }
        // B opens a stall on the market tile underfoot, N closes the own stall.
        if self.input.just_pressed.remove(&KeyCode::KeyB) {
            o.net.send_control(FromClient::StallOpen);
            o.respec_note = "stall requested".into();
        }
        if self.input.just_pressed.remove(&KeyCode::KeyN) {
            o.net.send_control(FromClient::StallClose);
            o.respec_note = "closing the stall".into();
        }
        let keys = [KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4];
        for (i, k) in keys.iter().enumerate() {
            if self.input.just_pressed.remove(k)
                && let Some(b) = pack.builds.get(i)
            {
                o.net
                    .send_control(FromClient::Respec(BuildChoice::Preset(b.name.clone())));
                o.respec_note = format!("respec {} requested", b.name);
            }
        }
    }

    /// Network events, prediction ticks and the entity list for this frame. Returns the
    /// camera `(position, yaw, pitch)`.
    fn online_frame(
        &mut self,
        frame_dt: f32,
        event_loop: &ActiveEventLoop,
    ) -> Option<(Vec3, f32, f32)> {
        self.respec_hotkeys();
        let returns = self.returns_to_screens();
        // The camera is the character's mode's (MODES.md 2).
        if let Some(c) = self.online.as_ref().and_then(|o| o.client.as_ref()) {
            self.viewport = if c.sheet.kit.mode.third_person() {
                Viewport::Third
            } else {
                Viewport::First
            };
        }
        let bsp = &self.bsp;
        let viewport = self.viewport;
        let scope = self.gun_scope();
        let o = self.online.as_mut()?;
        o.backlog.extend(o.net.poll());
        // The zone's map is still being fetched (a browser): what the zone sent after its
        // `Welcome` waits, except snapshots, which would be stale by then anyway.
        #[cfg(target_arch = "wasm32")]
        if let Some(wanted) = o.awaiting_map {
            // The zone hung up meanwhile: say so now, not after a map nobody needs.
            if let Some(NetEvent::Disconnected(reason)) = o
                .backlog
                .iter()
                .find(|e| matches!(e, NetEvent::Disconnected(_)))
            {
                let why = format!("disconnected: {reason}");
                if returns {
                    self.zone_ended = Some(why);
                    return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
                }
                self.fail(event_loop, &why);
                return None;
            }
            match self.pending_fetch.borrow_mut().take() {
                None => {
                    o.backlog.retain(|e| !matches!(e, NetEvent::Snapshot(_)));
                    return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
                }
                Some(Ok((map, hash))) if hash == wanted => {
                    o.awaiting_map = None;
                    o.map_hash = hash;
                    crate::web::tell_page("status", "");
                    self.pending_map = Some((map, hash));
                    // The world is replaced at the start of the next frame.
                    return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
                }
                Some(result) => {
                    let why = match result {
                        Err(e) => e,
                        Ok(_) => "this site's copy of the zone's map is another build".into(),
                    };
                    log::error!("{why}");
                    if returns {
                        self.zone_ended = Some(why);
                        return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
                    }
                    crate::web::tell_page("error", &why);
                    self.exit_requested = true;
                    event_loop.exit();
                    return None;
                }
            }
        }
        while let Some(ev) = o.backlog.pop_front() {
            match ev {
                NetEvent::Welcome {
                    entity,
                    hz,
                    map,
                    map_hash,
                } => {
                    o.rate = TickRate::new(hz as u32);
                    o.welcome = Some((entity, hz));
                    log::info!("joined as entity {entity} on {map} at {hz} Hz");
                    // The air of the map, and nothing read from the last zone's frame. A
                    // map already loaded says its own air now; another map says it as it
                    // is switched to (`switch_map`).
                    self.sound.quiet();
                    self.sound.set_rate(o.rate.dt());
                    self.sound.air(&map);
                    if map_hash == o.map_hash
                        && let Some(air) = ambience_of(&self.bsp)
                    {
                        self.sound.air_named(air);
                    }
                    #[cfg(target_arch = "wasm32")]
                    crate::web::tell_page("status", "");
                    // The name becomes a file name or a URL: a zone does not choose paths.
                    if !valid_map_name(&map) {
                        let why =
                            format!("the zone named a map this client will not load: {map:?}");
                        if returns {
                            self.zone_ended = Some(why);
                            return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
                        }
                        self.fail(event_loop, &why);
                        return None;
                    }
                    if map_hash != o.map_hash {
                        // Another map: load it (a zone change through the hub lands here).
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let path = self.opts.maps_dir.join(format!("{map}.bsp"));
                            let loaded = std::fs::read(&path)
                                .ok()
                                .filter(|bytes| fnv1a64(bytes) == map_hash)
                                .and_then(|_| Bsp::load(&path).ok());
                            match loaded {
                                Some(bsp) => {
                                    log::info!("zone runs map {map}; loading {}", path.display());
                                    self.pending_map = Some((bsp, map_hash));
                                    o.map_hash = map_hash;
                                }
                                None => {
                                    log::error!(
                                        "zone runs map {map} with hash {map_hash:016x}; ours is {:016x} and {} does not match (wrong map build)",
                                        o.map_hash,
                                        path.display()
                                    );
                                    if returns {
                                        self.zone_ended = Some(format!(
                                            "this client does not have the zone's map ({map})"
                                        ));
                                        return Some((
                                            self.sim.eye(),
                                            self.sim.yaw,
                                            self.sim.pitch,
                                        ));
                                    }
                                    self.exit_requested = true;
                                    event_loop.exit();
                                    return None;
                                }
                            }
                        }
                        #[cfg(target_arch = "wasm32")]
                        {
                            log::info!("zone runs map {map}; fetching it");
                            crate::web::tell_page("status", "loading the zone's map");
                            o.awaiting_map = Some(map_hash);
                            let (slot, assets) =
                                (self.pending_fetch.clone(), self.opts.assets.clone());
                            wasm_bindgen_futures::spawn_local(async move {
                                let fetched = fetch_map(&assets, &map, Some(map_hash)).await;
                                *slot.borrow_mut() = Some(fetched);
                            });
                            return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
                        }
                    }
                }
                NetEvent::Snapshot(bytes) => {
                    if let Some(c) = &mut o.client {
                        match c.on_snapshot(bsp, &bytes) {
                            Ok(()) => o.last_snapshot = Instant::now(),
                            Err(e) => log::debug!("snapshot dropped: {e}"),
                        }
                    }
                }
                NetEvent::Control(msg) => match msg {
                    FromZone::Content {
                        pack,
                        own,
                        team,
                        props,
                    } => {
                        let Some((entity, _)) = o.welcome else {
                            log::error!("content before welcome");
                            continue;
                        };
                        o.team = team;
                        o.props = props;
                        o.build_name = o.build_name_of(&pack, &own);
                        match &mut o.client {
                            // Content tuned under the zone (GM.md 3): the same body on the
                            // new numbers; the prediction goes on.
                            Some(c) => {
                                log::info!("content tuned: {} abilities", pack.abilities.len());
                                c.set_sheet(Sheet::new(own, &pack, team));
                                o.gm_note = "tuned".into();
                            }
                            None => {
                                log::info!(
                                    "content: {} abilities, {} presets; playing {} on team {team}",
                                    pack.abilities.len(),
                                    pack.builds.len(),
                                    o.build_name
                                );
                                o.client = Some(ClientState::new(
                                    entity,
                                    o.rate,
                                    Sheet::new(own, &pack, team),
                                ));
                            }
                        }
                        o.pack = Some(pack);
                    }
                    FromZone::Gm(news) => match news {
                        GmNews::Granted => {
                            o.gm = true;
                            o.say("game master here: G opens the page".into(), hud::DIM);
                            log::info!("game master");
                        }
                        GmNews::Tuning(t) => {
                            o.gm_note = if t.is_default() {
                                "as authored".into()
                            } else {
                                format!("tempo x{:.2}, {} set", t.tempo, t.abilities.len())
                            };
                            o.tuning = t;
                        }
                        GmNews::Refused(why) => {
                            log::info!("gm refused: {why}");
                            o.gm_note = format!("refused: {why}");
                        }
                    },
                    FromZone::BuildApplied(build) => {
                        let changed = o.client.as_ref().is_some_and(|c| c.sheet.build != build);
                        if changed && let Some(pack) = &o.pack {
                            let name = o.build_name_of(pack, &build);
                            let sheet = Sheet::new(build, pack, o.team);
                            if let Some(c) = &mut o.client {
                                c.set_sheet(sheet);
                            }
                            o.build_name = name;
                            o.respec_note = format!("now {}", o.build_name);
                            log::info!("build applied: {}", o.build_name);
                        }
                    }
                    FromZone::RespecResult(result) => {
                        o.respec_note = match result {
                            Ok(()) => "respec accepted (next respawn)".into(),
                            Err(e) => format!("respec refused: {e}"),
                        };
                        log::info!("{}", o.respec_note);
                    }
                    FromZone::TravelTicket {
                        zone,
                        addr,
                        cert_der,
                        token,
                        web,
                    } => {
                        log::info!("travel ticket for {zone} at {addr}");
                        let to = ZoneAddr {
                            addr: addr.parse().ok(),
                            cert_der,
                            web,
                        };
                        // The connection here is given up only for one that can be opened: a browser
                        // needs the zone's web listener, a native client its address.
                        let reachable = if cfg!(target_arch = "wasm32") {
                            to.web.is_some()
                        } else {
                            to.addr.is_some()
                        };
                        if reachable {
                            o.pending_travel = Some((zone, to, token));
                        } else if cfg!(target_arch = "wasm32") {
                            o.say(format!("{zone} has no web listener"), hud::ORANGE);
                        } else {
                            o.say(format!("{zone}: bad address {addr:?}"), hud::ORANGE);
                        }
                    }
                    FromZone::TravelRefused(reason) => {
                        o.respec_note = format!("travel refused: {reason}");
                        log::info!("{}", o.respec_note);
                        o.say(o.respec_note.clone(), hud::ORANGE);
                    }
                    FromZone::Roster(players) => {
                        o.names.clear();
                        o.kinds.clear();
                        o.looks.clear();
                        for p in players {
                            o.kinds.insert(p.id, p.kind);
                            o.names.insert(p.id, (p.name, p.team, p.model));
                            o.looks.insert(p.id, p.look);
                        }
                    }
                    FromZone::PlayerInfo {
                        id,
                        name,
                        team,
                        model,
                        kind,
                        look,
                    } => {
                        o.kinds.insert(id, kind);
                        o.names.insert(id, (name, team, model));
                        o.looks.insert(id, look);
                    }
                    FromZone::Look { id, look } => {
                        o.looks.insert(id, look);
                    }
                    FromZone::Squad(entries) => o.squad = entries,
                    FromZone::OrderRefused(why) => {
                        o.say(format!("order refused: {why}"), hud::ORANGE);
                    }
                    FromZone::Encounter { name, state } => {
                        let (text, colour) = match state {
                            EncounterState::Engaged => (format!("{name}: engaged"), hud::WHITE),
                            EncounterState::Reset => (format!("{name}: reset"), hud::ORANGE),
                            EncounterState::Cleared { secs } => {
                                (format!("{name}: cleared in {secs} s"), hud::GREEN)
                            }
                        };
                        o.say(text, colour);
                    }
                    FromZone::Loot {
                        encounter,
                        items,
                        coin,
                    } => {
                        let mut what = items.join(", ");
                        if coin > 0 {
                            if !what.is_empty() {
                                what.push_str(", ");
                            }
                            what.push_str(&format!("{coin} silver"));
                        }
                        o.say(format!("loot ({encounter}): {what}"), hud::YELLOW);
                    }
                    FromZone::Trial {
                        name,
                        passed,
                        detail,
                        ..
                    } => {
                        if passed {
                            o.say(format!("trial passed: {name}"), hud::GREEN);
                            o.say(detail, hud::DIM);
                        } else {
                            o.say(format!("trial not passed: {name}: {detail}"), hud::DIM);
                        }
                    }
                    FromZone::ModelRevoked(id) => {
                        for entry in o.names.values_mut() {
                            if entry.2 == Some(id) {
                                entry.2 = None;
                            }
                        }
                        for stall in &mut o.stalls {
                            if stall.model == Some(id) {
                                stall.model = None;
                            }
                        }
                        self.revoked.push(id);
                    }
                    FromZone::Stalls(stalls) => o.stalls = stalls,
                    FromZone::StallOpened(stall) => {
                        log::info!("{} opened a stall", stall.owner);
                        o.stalls.retain(|s| s.id != stall.id);
                        o.stalls.push(stall);
                    }
                    FromZone::StallClosed(id) => o.stalls.retain(|s| s.id != id),
                    FromZone::StallResult(result) => {
                        o.respec_note = match result {
                            Ok(()) => "stall: done".into(),
                            Err(e) => format!("stall refused: {e}"),
                        };
                        log::info!("{}", o.respec_note);
                    }
                    // The answers the inventory and the stall wait for (ITEMS.md 5). One
                    // that no open screen is waiting for (the screen was closed, or it is
                    // the answer to an earlier request) is said where the game says
                    // things, and moves nothing on a screen.
                    FromZone::BuyResult { listing, result } => {
                        log::info!("buy of listing {listing}: {result:?}");
                        let good = result.is_ok();
                        let elsewhere = match (&mut self.bag, &self.hub) {
                            (Some(bag), Some(hub)) => bag.bought(hub, listing, result),
                            _ => Some(match result {
                                Ok(()) => "bought: it is in the inventory".to_string(),
                                Err(e) => format!("not bought: {e}"),
                            }),
                        };
                        if let Some(text) = elsewhere {
                            o.say(text, if good { hud::DIM } else { hud::ORANGE });
                        }
                    }
                    FromZone::WearResult { item, result } => {
                        log::info!("wear of item {item}: {result:?}");
                        let good = result.is_ok();
                        let elsewhere = match (&mut self.bag, &self.hub) {
                            (Some(bag), Some(hub)) => bag.worn(hub, item, result),
                            _ => Some(match result {
                                Ok(()) => "what is worn changed".to_string(),
                                Err(e) => format!("what is worn did not change: {e}"),
                            }),
                        };
                        if let Some(text) = elsewhere {
                            o.say(text, if good { hud::DIM } else { hud::ORANGE });
                        }
                    }
                    FromZone::PlayerLeft(id) => {
                        o.names.remove(&id);
                        o.kinds.remove(&id);
                    }
                    // People together (PARTY.md 4). What the party is is the hub's
                    // word, shown as it is said.
                    FromZone::Party(names) => {
                        log::info!("party: {}", names.join(", "));
                        if !names.is_empty() {
                            // In a party: whatever invitations waited are moot.
                            o.social.asks.retain(|a| a.trade.is_some());
                        }
                        o.social.party = names;
                    }
                    FromZone::Invited { from } => {
                        // Somebody who is not heard is not heard here either: the
                        // invitation is declined, so that it holds none of the places.
                        let unheard = self
                            .settings
                            .ignored
                            .iter()
                            .any(|i| names::skeleton(i) == names::skeleton(&from));
                        if unheard {
                            // (The zone takes one such request a second, as the page's.)
                            o.social.asked = Some(Instant::now());
                            o.net
                                .send_control(FromClient::PartyAnswer { from, join: false });
                        } else if o.social.invited(from.clone(), Instant::now()) {
                            log::info!("{from} invites to a party");
                            self.sound.play(crate::sound::synth::Cue::Chime);
                            let line = format!("{from} invites you to a party: P");
                            self.chat.heard(None, line, &[]);
                        }
                    }
                    FromZone::Heard {
                        channel,
                        from,
                        text,
                    } => {
                        // (Not logged: a whisper is between two people.)
                        log::debug!("<{from} on {channel}> {text}");
                        // Heard as it is shown: not an ignored name's, not the own
                        // whisper going out (SOUND.md 3).
                        let shown = self
                            .chat
                            .heard_on(channel, from, text, &self.settings.ignored);
                        if shown && channel != gm_net::control::CHANNEL_WHISPERED {
                            self.sound.play(crate::sound::synth::Cue::Blip);
                        }
                    }
                    FromZone::TradeAsked { from } => {
                        let Some(name) = o.names.get(&from).map(|n| n.0.clone()) else {
                            continue;
                        };
                        let unheard = self
                            .settings
                            .ignored
                            .iter()
                            .any(|i| names::skeleton(i) == names::skeleton(&name));
                        if !unheard && o.social.trade_asked(from, name.clone(), Instant::now()) {
                            log::info!("{name} asks to trade");
                            self.sound.play(crate::sound::synth::Cue::Chime);
                            let line = format!("{name} asks to trade: P");
                            self.chat.heard(None, line, &[]);
                        }
                    }
                    FromZone::TradeOpened { trade, with } => {
                        // The hub opened a trade both asked for: its window comes up
                        // over whatever screen was up (said twice, it is up already).
                        log::info!("trade {trade} with {with}");
                        o.social
                            .asks
                            .retain(|a| a.trade.is_none() || a.from != with);
                        let up = self.people.as_ref().is_some_and(|p| p.trading(trade));
                        if !up
                            && let (Some(hub), Some(account), Some(character)) =
                                (&self.hub, &self.account, self.character)
                        {
                            let now = Instant::now();
                            let session = account.session;
                            self.people =
                                Some(People::trade(hub, session, character, trade, with, now));
                            self.menu = None;
                            self.bag = None;
                            self.chat.drop_line();
                            self.input.keys.clear();
                            self.input.just_pressed.clear();
                            self.input.mouse.clear();
                        }
                    }
                    FromZone::Hit {
                        target,
                        amount,
                        absorbed,
                        at,
                    } => {
                        // The number where the blow landed (LOOK.md 13.11), not over the
                        // head: a headshot reads as one.
                        let _ = target;
                        self.effects
                            .hit(Vec3::from_array(at), amount as u32, absorbed as u32);
                        let now = Instant::now();
                        self.combo = match self.combo {
                            (n, Some(last))
                                if now.duration_since(last).as_secs_f32() < COMBO_SECS =>
                            {
                                (n + 1, Some(now))
                            }
                            _ => (1, Some(now)),
                        };
                    }
                    FromZone::Healed { target, amount } => o.heals.push((target, amount)),
                    FromZone::Impact { at, normal } => {
                        self.effects
                            .impact(Vec3::from_array(at), Vec3::from_array(normal));
                    }
                    FromZone::Killed { victim, killer } => {
                        let me = o.client.as_ref().map(|c| c.my_id);
                        if Some(killer) == me && victim != killer {
                            o.kills += 1;
                        }
                        if Some(victim) == me {
                            o.deaths += 1;
                        }
                        let name = |id: u32| {
                            o.names
                                .get(&id)
                                .map(|(n, _, _)| n.clone())
                                .unwrap_or_else(|| format!("#{id}"))
                        };
                        log::info!("{} killed {}", name(killer), name(victim));
                    }
                    FromZone::ReportResult(result) => {
                        let (text, colour) = match result {
                            Ok(()) => (
                                "report taken: the fight is kept for a moderator".to_string(),
                                hud::GREEN,
                            ),
                            Err(why) => (format!("report refused: {why}"), hud::ORANGE),
                        };
                        o.say(text, colour);
                    }
                    FromZone::ChatFrom { from, text } => {
                        // From nobody: the zone itself (a refusal, a notice).
                        let who = (from != 0).then(|| {
                            o.names
                                .get(&from)
                                .map_or_else(|| format!("#{from}"), |n| n.0.clone())
                        });
                        log::info!("<{}> {text}", who.as_deref().unwrap_or("zone"));
                        let mine = o.client.as_ref().is_some_and(|c| c.my_id == from);
                        let shown = self.chat.heard(who, text, &self.settings.ignored);
                        if shown && !mine && from != 0 {
                            self.sound.play(crate::sound::synth::Cue::Blip);
                        }
                    }
                    // (Not printed: the words for every message of the zone's would be
                    // carried by every browser for this one line.)
                    _ => log::debug!("a message of the zone's this client has no use for"),
                },
                NetEvent::Disconnected(reason) => {
                    if returns {
                        log::info!("disconnected: {reason}");
                        self.zone_ended = Some(reason);
                        return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
                    }
                    log::error!("disconnected: {reason}");
                    #[cfg(target_arch = "wasm32")]
                    crate::web::tell_page("error", &format!("disconnected: {reason}"));
                    self.exit_requested = true;
                    event_loop.exit();
                    return None;
                }
            }
        }
        // A travel ticket: say goodbye here, connect there (the body stays as a ghost until
        // the other zone claims it, HUB.md 3.3).
        if let Some((zone, to, token)) = o.pending_travel.take() {
            o.net.hang_up();
            o.backlog.clear();
            match NetClient::connect(to, self.opts.name.clone(), None, self.opts.team, token) {
                Ok(net) => {
                    let old = std::mem::replace(&mut o.net, net);
                    self.leaving.push((old, Instant::now()));
                    o.welcome = None;
                    o.client = None;
                    o.pack = None;
                    o.names.clear();
                    o.kinds.clear();
                    o.squad.clear();
                    o.stalls.clear();
                    o.zone_name = zone;
                    o.respec_note = format!("travelling to {}", o.zone_name);
                    // The next zone says what the party is; who asked what stays here.
                    o.social = Social::default();
                    self.bag = None;
                    self.people = None;
                    self.gm_page = None;
                    self.entities.clear();
                    self.bodies.clear();
                }
                // The connection that was hung up says so in a moment, and that ends it.
                Err(e) => log::error!("travel failed: {e}"),
            }
            return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
        }
        let Some(c) = &mut o.client else {
            return Some((self.sim.eye(), self.sim.yaw, self.sim.pitch));
        };
        let dt = o.rate.dt();
        o.accumulator += frame_dt.min(0.25);
        let mut steps = 0;
        let bodies = c.latest_boxes();
        let scripted = self.opts.script.as_deref() == Some("fight");
        // What the own body did this frame, for the sound: its predicted actions and
        // the ground it covered by its own ticks (SOUND.md 3).
        let mut own_actions: Vec<gm_core::sim::Action> = Vec::new();
        let mut own_travel = 0.0_f32;
        // Space dodges in the action mode (MODES.md 4.6): the kit's dash, while ready.
        let dodge = (c.sheet.kit.mode == gm_core::vocab::Mode::Action)
            .then(|| dodge_slot(&c.sheet.kit, &c.mover, c.tick))
            .flatten();
        let gun = c.sheet.kit.mode == gm_core::vocab::Mode::Gun;
        let rpg = c.sheet.kit.mode == gm_core::vocab::Mode::Rpg;
        // The RPG mode's keys (MODES.md 5.3): with a target they ask for a target-action;
        // without one they press as the action mode does.
        let rpg_bodies = if rpg {
            rpg_bodies_of(c, o.team)
        } else {
            Vec::new()
        };
        let mut rpg_pressed = 0u16;
        let mut rpg_ability = 0u8;
        if rpg {
            use crate::rpg::Act;
            let present = |id: u32| rpg_bodies.iter().any(|b| b.id == id);
            self.rpg.lost(present);
            let asks = [
                (KeyCode::Digit1, Act::Primary),
                (KeyCode::Digit2, Act::Secondary),
                (KeyCode::Digit3, Act::Active(1)),
                (KeyCode::Digit4, Act::Active(2)),
                (KeyCode::Digit5, Act::Active(3)),
                (KeyCode::Digit6, Act::Active(4)),
            ];
            for (key, act) in asks {
                if !self.input.just_pressed.remove(&key) {
                    continue;
                }
                if self.rpg.target.is_some() {
                    self.rpg.ask(act);
                } else {
                    match act {
                        Act::Primary => rpg_pressed |= buttons::PRIMARY,
                        Act::Secondary => rpg_pressed |= buttons::SECONDARY,
                        Act::Active(n) => rpg_ability = n,
                    }
                }
            }
        }
        while o.accumulator >= dt && steps < MAX_STEPS_PER_FRAME {
            let (yaw, pitch) = match viewport {
                Viewport::First => (self.sim.yaw, self.sim.pitch),
                // The RPG body faces where it goes or its target; its bolts without a
                // target fly level (MODES.md 5.3).
                Viewport::Third if rpg => (self.sim.yaw, 0.0),
                Viewport::Third => {
                    let eye = c.mover.eye();
                    let camera = third_person_camera(bsp, eye, self.sim.yaw, self.sim.pitch);
                    re_aim(bsp, &bodies, camera, self.sim.yaw, self.sim.pitch, eye)
                }
            };
            self.aim = (yaw, pitch);
            let rpg_frame = rpg.then(|| {
                let mut f = self.rpg.frame(
                    bsp,
                    &c.sheet.kit,
                    &c.mover,
                    &rpg_bodies,
                    self.input.axes(),
                    yaw,
                    c.tick,
                    o.rate.hz(),
                );
                f.buttons |= std::mem::take(&mut rpg_pressed);
                if f.ability == 0 {
                    f.ability = std::mem::take(&mut rpg_ability);
                }
                f
            });
            let input = if scripted {
                let target = self
                    .script_target
                    .map(|(_, at, velocity, _)| (at, velocity));
                let input = fight_input(c.mover.eye(), target, &mut self.sim, c.tick);
                self.aim = (input.yaw, input.pitch);
                input
            } else {
                self.input.sim_input(yaw, pitch, dodge, gun, rpg_frame)
            };
            let before = c.mover.mv.origin;
            let datagram = c.local_tick(bsp, input);
            own_actions.extend(c.actions.iter().copied());
            if c.mover.mv.on_ground {
                own_travel += (c.mover.mv.origin - before).truncate().length();
            }
            o.net.send_input(datagram.encode());
            o.prev_origin = o.curr_origin;
            o.curr_origin = c.mover.mv.origin;
            o.accumulator -= dt;
            steps += 1;
        }
        if steps == MAX_STEPS_PER_FRAME {
            o.accumulator = 0.0;
        }
        if !c.synced() {
            // Until the first snapshot lands, show the map from the local spawn.
            o.prev_origin = c.mover.mv.origin;
            o.curr_origin = c.mover.mv.origin;
        }

        // Other entities at the interpolation time (PROTOCOL.md 7.3).
        let extra = o.last_snapshot.elapsed().as_secs_f32() / dt;
        let t = c.render_tick(extra);
        self.entities.clear();
        self.bodies.clear();
        self.tags.clear();
        self.effects.begin(frame_dt);
        let feet_under = Vec3::Z * Hull::Player.mins().z;
        let my_team = o.team;
        // The crouch lowers the eye (MODES.md 3.4): the predicted mover's drop, eased over
        // about a tenth of a second so the view does not jump.
        let drop = if c.mover.crouched {
            gm_core::sim::CROUCH_DROP
        } else {
            0.0
        };
        self.eye_drop += (drop - self.eye_drop) * (frame_dt * 12.0).min(1.0);
        // The own hull origin as drawn, and the eye over it less the drop: the body
        // itself is lowered by its squat (the animator's), not by the eye.
        let centre = {
            let alpha = (o.accumulator / o.rate.dt()).clamp(0.0, 1.0);
            o.prev_origin.lerp(o.curr_origin, alpha)
        };
        let eye = centre + Vec3::Z * (c.mover.mv.hull.eye_height() - self.eye_drop);
        // The recoil's punch (MODES.md 3.3): the firearm's kick for this shot lands on the
        // view at once and decays over 150 ms; the frames sent carry the mouse's aim,
        // the zone kicks the bolt by the same pair.
        for a in &own_actions {
            if let gm_core::sim::Action::Fire { kick, .. } = a
                && (kick.0 != 0.0 || kick.1 != 0.0)
            {
                self.view_punch = *kick;
            }
        }
        let decay = (-frame_dt / PUNCH_DECAY_SECS).exp();
        self.view_punch = (self.view_punch.0 * decay, self.view_punch.1 * decay);
        self.zoom = if self.input.scoped {
            scope.max(1) as f32
        } else {
            1.0
        };
        let camera = match viewport {
            Viewport::First => eye,
            Viewport::Third if rpg => {
                crate::rpg::orbit_camera(bsp, centre, self.sim.yaw, self.sim.pitch, self.rpg.dist)
            }
            Viewport::Third => third_person_camera(bsp, eye, self.sim.yaw, self.sim.pitch),
        };
        let others = c.others_at(t);
        if scripted {
            let nearest = others
                .iter()
                .filter(|e| e.kind == EntityKind::Player && e.alive())
                .filter(|e| match e.spawn {
                    SpawnInfo::Player { team, .. } => my_team == 0 || team != my_team,
                    _ => false,
                })
                .min_by(|a, b| {
                    (a.pos - centre)
                        .length_squared()
                        .total_cmp(&(b.pos - centre).length_squared())
                })
                .map(|e| (e.id, e.pos));
            // Its velocity from where it stood a frame ago, if it is the same body.
            let now = Instant::now();
            self.script_target = nearest.map(|(id, at)| {
                let velocity = match self.script_target {
                    Some((was, before, _, then)) if was == id => {
                        let dt = (now - then).as_secs_f32();
                        if dt > 1.0e-3 {
                            ((at - before) / dt).clamp_length_max(600.0)
                        } else {
                            Vec3::ZERO
                        }
                    }
                    _ => Vec3::ZERO,
                };
                (id, at, velocity, now)
            });
        }
        // What the HUD shows of the squad and of the creature being fought: the one the
        // squad is ordered onto, or else the nearest one that is hurt.
        self.squad_view = o
            .squad
            .iter()
            .map(|m| {
                let e = others.iter().find(|e| e.id == m.id);
                (e.and_then(|e| e.health), e.is_some_and(|e| e.alive()))
            })
            .collect();
        // And of the party: the other members, each with the health the wire carries for
        // its body when that is here (the bodies that are people: a hired copy of
        // somebody bears that name too).
        let me = Some(c.my_id);
        self.party_view =
            o.social
                .party
                .iter()
                .filter_map(|member| {
                    let body = o.names.iter().find(|(id, n)| {
                        n.0 == *member && o.kinds.get(id) == Some(&BodyKind::Human)
                    });
                    match body {
                        Some((id, _)) if Some(*id) == me => None,
                        Some((id, _)) => {
                            let health = others.iter().find(|e| e.id == *id).and_then(|e| e.health);
                            Some((member.clone(), Some(health)))
                        }
                        None => Some((member.clone(), None)),
                    }
                })
                .collect();
        self.target_view = None;
        // The RPG mode's target frame (MODES.md 5.2): its name and the health the zone
        // sends for a targeted body; a stranger's whole is read as the band's top.
        if rpg && let Some(target) = self.rpg.target {
            if let Some(e) = others.iter().find(|e| e.id == target) {
                let name = o
                    .names
                    .get(&target)
                    .map_or_else(|| "?".to_string(), |n| n.0.clone());
                let max = match (o.kinds.get(&target), &o.pack) {
                    (Some(BodyKind::Creature { def }), Some(pack)) => {
                        pack.creatures.get(*def as usize).map_or(1500, |d| d.health)
                    }
                    _ => 1500,
                };
                self.target_view = Some((name, e.health.unwrap_or(0), max));
            }
        } else if let Some(pack) = &o.pack {
            let mut best: Option<(f32, String, u16, u16)> = None;
            for e in others.iter().filter(|e| e.alive()) {
                let (Some(BodyKind::Creature { def }), Some(health)) =
                    (o.kinds.get(&e.id), e.health)
                else {
                    continue;
                };
                let Some(d) = pack.creatures.get(*def as usize) else {
                    continue;
                };
                let ordered = o
                    .squad
                    .iter()
                    .any(|m| matches!(m.order, Order::Attack(id) if id == e.id));
                if !ordered && health >= d.health {
                    continue;
                }
                let rank = (e.pos - centre).length() - if ordered { 1.0e6 } else { 0.0 };
                if best.as_ref().is_none_or(|b| rank < b.0) {
                    best = Some((rank, d.name.clone(), health, d.health));
                }
            }
            self.target_view = best.map(|(_, name, health, max)| (name, health, max));
        }
        for e in others.iter().copied() {
            match e.kind {
                EntityKind::Player => {
                    let SpawnInfo::Player {
                        frame,
                        team,
                        aspects,
                        armour,
                    } = e.spawn
                    else {
                        continue;
                    };
                    // Health the zone sends (the own party's and creatures') under the name.
                    let mate = o.squad.iter().position(|m| m.id == e.id);
                    let max_health = match (mate, o.kinds.get(&e.id), &o.pack) {
                        (Some(i), _, _) => Some(o.squad[i].max_health),
                        (None, Some(BodyKind::Creature { def }), Some(pack)) => {
                            pack.creatures.get(*def as usize).map(|d| d.health)
                        }
                        _ => None,
                    };
                    // Whose side it is on: the own team's, the own squad's and the own
                    // party's are friends; a creature and another team's are foes.
                    let name = o.names.get(&e.id).map(|n| n.0.as_str()).unwrap_or("");
                    let creature = matches!(o.kinds.get(&e.id), Some(BodyKind::Creature { .. }));
                    let friend = (my_team != 0 && team == my_team)
                        || mate.is_some()
                        || (!name.is_empty() && self.party_view.iter().any(|(n, _)| n == name));
                    let foe = !friend && (creature || (my_team != 0 && team != 0));
                    if e.alive() {
                        // What it does, drawn where it lands (LOOK.md 13): the wedge of the
                        // swing it winds up, the slash when it comes, a spark when it is hit.
                        let swing = o
                            .pack
                            .as_ref()
                            .zip((e.acting as usize).checked_sub(1))
                            .and_then(|(p, i)| p.abilities.get(i))
                            .and_then(|d| swing_of(&d.ability, dt));
                        self.effects.fighter(
                            &crate::fx::Fighter {
                                key: e.id,
                                feet: e.pos + feet_under,
                                chest: crate::fx::chest_of(
                                    e.flags & gm_net::snapshot::flags::CROUCHED != 0,
                                ),
                                yaw: e.yaw,
                                anim: e.anim,
                                swing,
                                side: if friend {
                                    crate::fx::Side::Friend
                                } else {
                                    crate::fx::Side::Foe
                                },
                                health: e.health,
                            },
                            &mut self.fx,
                        );
                        // What the own hand did to it, as the zone said: a number over
                        // its head (LOOK.md 13.8).
                        let over_head = e.pos + feet_under + Vec3::Z * 72.0;
                        o.heals.retain(|&(target, amount)| {
                            if target != e.id {
                                return true;
                            }
                            self.effects.healed(over_head, amount as u32);
                            false
                        });
                        // Its name over its head, when the head is in sight and near.
                        let head = e.pos + Vec3::Z * 40.0;
                        if !name.is_empty()
                            && (head - camera).length() < 900.0
                            && bsp.trace(Hull::Point, camera, head).fraction >= 1.0
                        {
                            self.tags.push(Tag {
                                at: head,
                                name: name.to_string(),
                                ink: if friend {
                                    [0.55, 0.80, 1.0, 1.0]
                                } else if creature {
                                    [1.0, 0.62, 0.30, 1.0]
                                } else if foe {
                                    [1.0, 0.42, 0.36, 1.0]
                                } else {
                                    hud::WHITE
                                },
                                health: e
                                    .health
                                    .zip(max_health)
                                    .map(|(h, max)| h as f32 / max.max(1) as f32),
                            });
                        }
                    }
                    let yaw = facing(
                        self.facings.get(&e.id).copied(),
                        e.yaw,
                        e.vel,
                        e.anim,
                        e.flags & gm_net::snapshot::flags::RPG != 0,
                        frame_dt,
                    );
                    self.facings.insert(e.id, yaw);
                    self.bodies.push(Body {
                        key: e.id,
                        origin: e.pos,
                        yaw,
                        pitch: e.pitch,
                        anim: e.anim,
                        crouched: e.flags & gm_net::snapshot::flags::CROUCHED != 0,
                        frame,
                        armour,
                        aspects,
                        status: e.status,
                        model: o.names.get(&e.id).and_then(|n| n.2),
                        distance: (e.pos - camera).length(),
                        lit: self.effects.flash(e.id),
                        prop: held_prop(
                            &mut self.content,
                            self.active.as_mut(),
                            &o.props,
                            o.looks.get(&e.id).copied().unwrap_or_default(),
                        ),
                        off: off_prop(
                            &mut self.content,
                            self.active.as_mut(),
                            &o.props,
                            o.looks.get(&e.id).copied().unwrap_or_default(),
                        ),
                    });
                }
                EntityKind::Projectile => {
                    // A bright head on a streak along its way, round a small solid core.
                    self.entities.push(EntityDraw {
                        mins: e.pos - Vec3::splat(1.2),
                        maxs: e.pos + Vec3::splat(1.2),
                        color: [1.0, 0.95, 0.6, 1.0],
                    });
                    // The own bolts are known by their ability (its size, its damage's
                    // colour); another's is a bolt.
                    let known = match e.spawn {
                        SpawnInfo::Projectile { owner, def, .. } if owner == c.my_id => {
                            c.sheet.kit.abilities.get(def as usize).and_then(|a| {
                                a.steps.iter().find_map(|s| match &s.verb {
                                    gm_core::vocab::Verb::Projectile(p) => {
                                        Some((p.radius, damage_ink(&p.damage)))
                                    }
                                    _ => None,
                                })
                            })
                        }
                        _ => None,
                    };
                    let (radius, ink) = known.unwrap_or((3.0, crate::fx::BOLT));
                    self.effects
                        .projectile(e.id, e.pos, e.vel, radius, ink, camera, &mut self.fx);
                }
                EntityKind::Area => {
                    // What hurts is orange, what helps is green (PROTOCOL.md 5).
                    let (r, harmful) = match e.spawn {
                        SpawnInfo::Area {
                            radius, harmful, ..
                        } => (radius as f32, harmful),
                        _ => (32.0, true),
                    };
                    // A disc of its own size on the floor, and a burst when it appears.
                    self.effects.area(e.id, e.pos, r, harmful, &mut self.fx);
                }
            }
        }
        // Heard (SOUND.md 3): every new sample of every entity up to the render tick,
        // the own body as it predicted itself and as the zone said, from the own body's
        // place with the camera's facing.
        {
            let world = |from: Vec3, to: Vec3| {
                use gm_core::trace::CollisionWorld;
                bsp.trace(Hull::Point, from, to).fraction < 1.0
            };
            self.sound.feed(c.tracks(), t, &world);
            let anims = std::mem::take(&mut c.own_anims);
            if c.synced() {
                let me = crate::sound::cues::OwnNow {
                    id: c.my_id,
                    pos: centre,
                    on_ground: c.mover.mv.on_ground,
                    travel: own_travel,
                    health: c.own_health,
                    alive: c.own_alive,
                };
                self.sound.own(
                    self.started.elapsed().as_secs_f32(),
                    me,
                    &anims,
                    &own_actions,
                );
            }
            let yaw = self.sim.yaw;
            let listener = crate::sound::Listener {
                pos: eye,
                yaw: yaw.to_radians(),
            };
            self.sound.end_frame(frame_dt, listener);
        }
        c.prune(t);
        // The market: every stall with its keeper, who never moves and costs no snapshot.
        for stall in &o.stalls {
            stall_boxes(stall, &mut self.entities);
            // While somebody stands behind the counter (the owner, usually), that body is
            // the keeper; the stand-in is drawn when the tile is empty.
            let at = Vec3::from(stall.pos);
            let attended = self
                .bodies
                .iter()
                .any(|b| (b.origin - at).truncate().length() < 40.0)
                || (o.curr_origin - at).truncate().length() < 40.0;
            if !attended {
                self.bodies.push(stall_keeper(stall, camera));
            }
        }
        // The own body's stance (LOOK.md 13): what the zone says of it, except that its
        // own script is shown from the prediction, a round trip sooner; and so is the
        // place its swing lands, which is what a hand aims with.
        let own_script = c.mover.script.and_then(|s| {
            let ability = c.sheet.kit.abilities.get(s.ability as usize)?;
            let elapsed = gm_core::sim::tick_delta(c.tick, s.started).max(0) as u32;
            Some((ability, elapsed))
        });
        let own_anim = {
            use gm_core::sim::anim;
            let said = c.own_anim;
            if !c.synced() {
                anim::IDLE
            } else if !c.own_alive
                || !(anim::acts(said) || matches!(said, anim::IDLE | anim::RUN | anim::AIR))
            {
                // Dead, staggered, commanding, dashing, guarding: the zone's word.
                said
            } else if let Some((ability, elapsed)) = own_script {
                gm_core::sim::script_anim(ability, elapsed)
            } else {
                // The feet are predicted here, so their stance is too: a run that waited
                // for the zone's word slid for a round trip before the legs moved (and
                // the script over here and not yet there is the same case).
                if !c.mover.mv.on_ground {
                    anim::AIR
                } else if c.mover.ground_speed() > 10.0 {
                    anim::RUN
                } else {
                    anim::IDLE
                }
            }
        };
        if c.synced() {
            self.effects.fighter(
                &crate::fx::Fighter {
                    key: OWN,
                    feet: centre + feet_under,
                    // Under the eye in the first person, so the slash crosses the view.
                    chest: crate::fx::chest_of(c.mover.crouched),
                    yaw: c.mover.yaw,
                    anim: own_anim,
                    swing: own_script.and_then(|(ability, _)| swing_of(ability, dt)),
                    side: crate::fx::Side::Own,
                    health: Some(c.own_health.clamp(0, u16::MAX as i32) as u16),
                },
                &mut self.fx,
            );
        }
        // A bolt the own body let go this frame flies from the hand at once (the zone's
        // bolt shows a round trip later, well on its way).
        for action in &own_actions {
            if let gm_core::sim::Action::Fire {
                ability,
                step,
                target,
                ..
            } = action
                && let Some(ab) = c.sheet.kit.abilities.get(*ability as usize)
                && let Some(gm_core::vocab::Verb::Projectile(p)) =
                    ab.steps.get(*step as usize).map(|s| &s.verb)
            {
                // From where the zone spawns it (`resolve_origin`): the weapon's offset in
                // the body's frame, else the eyes. Not the camera: in the third person
                // the eye of the body is well in front of it.
                let from = match p.spawn {
                    gm_core::vocab::Origin::Weapon { offset } => {
                        let (fwd, right) = gm_core::movement::yaw_vectors(c.mover.yaw);
                        eye + fwd * offset[0] + right * offset[1] + Vec3::Z * offset[2]
                    }
                    _ => eye,
                };
                // Where the zone sends it (MODES.md 5.3): led to its target when that is
                // within the ability's range and in sight, as the zone leads it; else
                // along the body's look. (Until 2026-10-08 the tracer always flew the
                // look's way, which in the RPG mode is the camera's: the director saw
                // the shard leave toward the camera's horizon while the zone's bolt
                // went for the dummy.)
                let led = rpg_bodies
                    .iter()
                    .find(|b| *target != 0 && b.id == *target)
                    .filter(|b| {
                        let centre = b.centre();
                        ab.range > 0.0
                            && (centre - c.mover.mv.origin).truncate().length() <= ab.range
                            && gm_core::sim::sees(bsp, c.mover.eye(), centre)
                    })
                    .map(|b| {
                        let vel = others
                            .iter()
                            .find(|e| e.id == b.id)
                            .map_or(Vec3::ZERO, |e| e.vel);
                        let point = gm_core::sim::aim::lead(
                            from,
                            b.centre(),
                            vel,
                            p.speed,
                            p.gravity_scale,
                        );
                        (point - from).normalize_or_zero()
                    })
                    .filter(|d| d.length_squared() > 0.5);
                let dir = led.unwrap_or_else(|| gm_core::sim::view_dir(c.mover.yaw, c.mover.pitch));
                self.effects
                    .launch(from, dir * p.speed, p.radius, damage_ink(&p.damage));
            }
        }
        // A blow on a body the frame did not find (gone, or never in sight) says nothing.
        o.heals.clear();
        self.effects.draw(camera, &mut self.fx);
        // The RPG mode's marks (MODES.md 5.5): where the body is going, and a ring under
        // its target.
        if rpg {
            if let Some(goal) = self.rpg.walk {
                let feet = goal + Vec3::Z * Hull::Player.mins().z;
                self.fx.wall(
                    feet,
                    10.0,
                    6.0,
                    [1.0, 0.85, 0.3, 0.6],
                    [1.0, 0.85, 0.3, 0.0],
                );
            }
            if let Some(t) = self.rpg.target
                && let Some(e) = others.iter().find(|e| e.id == t)
            {
                let feet = e.pos + Vec3::Z * Hull::Player.mins().z;
                self.fx
                    .wall(feet, 20.0, 4.0, [1.0, 0.3, 0.2, 0.7], [1.0, 0.3, 0.2, 0.0]);
            }
        }
        self.effects.end();
        self.pops = self.effects.numbers();
        // (The own body's facing is kept across a frame in the first person, where it
        // is not drawn: a switch of the viewport finds it where it was.)
        let drawn: HashSet<u32> = self.bodies.iter().map(|b| b.key).chain([OWN]).collect();
        self.facings.retain(|k, _| drawn.contains(k));
        match viewport {
            Viewport::First => {
                // The view model (LOOK.md 6.4): the held prop in the frame's corner, with
                // the stride's bob and a kick on a launch.
                // A launch kicks; a swing is carried across the view by the own stance.
                let fired = own_actions
                    .iter()
                    .any(|a| matches!(a, gm_core::sim::Action::Fire { .. }));
                let want = match own_anim {
                    gm_core::sim::anim::WINDUP => -1.0,
                    gm_core::sim::anim::SWING => 1.0,
                    gm_core::sim::anim::RECOVER => 0.55,
                    _ => 0.0,
                };
                let rate = if want == 0.0 { 9.0 } else { 34.0 };
                self.view_swing += (want - self.view_swing) * (frame_dt * rate).min(1.0);
                if fired {
                    self.view_kick = 1.0;
                }
                self.view_kick = (self.view_kick - frame_dt * 6.0).max(0.0);
                self.view_stride += own_travel / 64.0;
                // The gun mode's hand is known here before the zone says so.
                let key = match gun_hand_prop(&self.content, o.pack.as_ref(), c) {
                    Some(key) => Some(key),
                    None => o
                        .props
                        .get(o.looks.get(&c.my_id).copied().unwrap_or_default().held as usize)
                        .cloned(),
                };
                let fit = key
                    .as_deref()
                    .map_or(Mat4::IDENTITY, |k| view_fit_of(&self.content, k));
                let prop = key.and_then(|key| {
                    let active = self.active.as_mut()?;
                    let (gpu, characters) = (&active.gpu, &mut active.renderer.characters);
                    self.content
                        .prop(&key, |model| Some(characters.add_model(gpu, model)))
                });
                let reload = reload_progress(c);
                // (Not while the scope is up, MODES.md 3.2.)
                self.view_model = prop.filter(|_| !self.input.scoped).map(|slot| ViewModel {
                    slot,
                    fit,
                    eye,
                    yaw: self.sim.yaw + self.view_punch.0,
                    pitch: self.sim.pitch - self.view_punch.1,
                    stride: if c.mover.mv.on_ground {
                        self.view_stride
                    } else {
                        0.0
                    },
                    kick: self.view_kick,
                    swing: self.view_swing,
                    reload,
                    light: crate::avatars::light_at(bsp, eye),
                });
                // The recoil's punch on the view (MODES.md 3.3).
                Some((
                    eye,
                    self.sim.yaw + self.view_punch.0,
                    (self.sim.pitch - self.view_punch.1).clamp(-89.0, 89.0),
                ))
            }
            _ => {
                // The own body, posed by the server's animation state. It faces where
                // the mover does while a turn holds it (toward its target for a
                // target-action, MODES.md 5.3; the magnet's turn, 4.2) and the camera's
                // way otherwise; an RPG body is free of the camera between actions
                // (MODES.md 5.1): it stands the way it last went or was turned, where
                // until 2026-10-08 it swung round with the orbit (the director).
                let build = &c.sheet.build;
                let locked = c.mover.lock_yaw.is_some();
                let look = if locked { c.mover.yaw } else { self.sim.yaw };
                let rpg = c.sheet.kit.mode == gm_core::vocab::Mode::Rpg;
                let yaw = facing(
                    self.facings.get(&OWN).copied(),
                    look,
                    c.mover.mv.velocity,
                    own_anim,
                    rpg && !locked,
                    frame_dt,
                );
                self.facings.insert(OWN, yaw);
                self.bodies.push(Body {
                    key: OWN,
                    origin: centre,
                    yaw,
                    pitch: self.sim.pitch,
                    anim: own_anim,
                    crouched: c.mover.crouched,
                    frame: gm_model::rig::frame_index(build.frame),
                    armour: build.armour as u8,
                    aspects: build.aspects.0,
                    status: c.mover.statuses.mask(),
                    model: o.names.get(&c.my_id).and_then(|n| n.2),
                    distance: 0.0,
                    lit: self.effects.flash(OWN),
                    prop: held_prop(
                        &mut self.content,
                        self.active.as_mut(),
                        &o.props,
                        o.looks.get(&c.my_id).copied().unwrap_or_default(),
                    ),
                    off: off_prop(
                        &mut self.content,
                        self.active.as_mut(),
                        &o.props,
                        o.looks.get(&c.my_id).copied().unwrap_or_default(),
                    ),
                });
                Some((camera, self.sim.yaw, self.sim.pitch))
            }
        }
    }

    /// The other bodies as the RPG mode reads them (MODES.md 5): where they stand now,
    /// their frame, and whether they are enemies (another team; in the wild, everyone).
    fn rpg_bodies(&self) -> Vec<crate::rpg::Body> {
        let Some(o) = &self.online else {
            return Vec::new();
        };
        let Some(c) = &o.client else {
            return Vec::new();
        };
        rpg_bodies_of(c, o.team)
    }

    /// A left click in the RPG mode (MODES.md 5.2, 5.5): a target, the primary at the
    /// target, or a walk.
    fn rpg_click(&mut self) {
        let Some((vp, size)) = self.last_vp else {
            return;
        };
        let Some((from, dir)) = crate::rpg::Rpg::ray(vp, size, self.cursor) else {
            return;
        };
        let bodies = self.rpg_bodies();
        self.rpg.click(&self.bsp, from, dir, &bodies);
    }

    /// The body under a pixel in the RPG mode, by the last frame's view.
    fn rpg_under(&self, cursor: (f32, f32)) -> Option<u32> {
        let (vp, size) = self.last_vp?;
        let (from, dir) = crate::rpg::Rpg::ray(vp, size, cursor)?;
        crate::rpg::Rpg::pick(&self.bsp, from, dir, &self.rpg_bodies())
    }

    /// The right button let go in the RPG mode: a tap on the target (no drag of the
    /// orbit, quick) is the secondary at it (MODES.md 5.5); a drag was the camera's.
    fn rpg_right_release(&mut self) {
        let Some((at, cursor, yaw, pitch)) = self.rpg_right.take() else {
            return;
        };
        let quick = at.elapsed().as_secs_f32() < DOUBLE_CLICK_SECS;
        let turned = (self.sim.yaw - yaw).abs() > 1.0 || (self.sim.pitch - pitch).abs() > 1.0;
        if quick && !turned && self.rpg.hover(self.rpg_under(cursor)) == crate::rpg::Hover::Attack {
            self.rpg.ask(crate::rpg::Act::Secondary);
        }
    }

    /// What a finger landing at `at` is for (MODES.md 5.6): the pointer while a screen
    /// is up; a control it lands on; the world in the RPG mode; else the stick on the
    /// left of the frame and the look on the right.
    fn touch_zone(&self, at: (f32, f32)) -> Zone {
        if self.screen_up() {
            return Zone::Ui;
        }
        if let Some((b, _)) = self.touch_buttons.iter().find(|(_, r)| r.contains(at)) {
            return Zone::Button(*b);
        }
        if self.rpg_mode() {
            return Zone::World;
        }
        let w = self.active.as_ref().map_or(0.0, |a| a.config.width as f32);
        if at.0 < w * touch::STICK_SHARE {
            Zone::Stick
        } else {
            Zone::Look
        }
    }

    /// The scale the HUD and the screens are drawn at this frame (CLIENT.md 3).
    fn ui_scale(&self) -> f32 {
        let size = self.active.as_ref().map_or((1280.0, 720.0), |a| {
            (a.config.width as f32, a.config.height as f32)
        });
        ui::scale_for(size, PANEL_UNITS, self.ui_scale_choice())
    }

    /// The scale asked for: the setting; by the window when it is 0, except on a touch
    /// screen, whose pixels are small under a finger: there by the device's pixel ratio
    /// (CLIENT.md 3, MODES.md 5.6), still never larger than the panels can fit.
    fn ui_scale_choice(&self) -> u8 {
        if self.settings.ui_scale != 0 || !self.fingers.seen {
            return self.settings.ui_scale;
        }
        let dpr = self
            .active
            .as_ref()
            .map_or(1.0, |a| a.window.scale_factor() as f32);
        ui::touch_scale(dpr)
    }

    /// A mouse button pressed in the game with the pointer held: the secondary toggles
    /// the scope of a scoped firearm (MODES.md 3.2).
    fn press_in_game(&mut self, button: MouseButton) {
        if self.input.mouse.insert(button) && button == MouseButton::Right && self.gun_scope() > 1 {
            self.input.scoped = !self.input.scoped;
        }
    }

    /// A key held by a finger (a control, a cell of the hotbar).
    fn hold_key(&mut self, code: KeyCode, down: bool) {
        if down {
            if self.input.keys.insert(code) {
                self.input.just_pressed.insert(code);
            }
        } else {
            self.input.keys.remove(&code);
        }
    }

    /// A control pressed or let go by a finger (MODES.md 5.6).
    fn touch_button(&mut self, button: TouchButton, down: bool) {
        match button {
            TouchButton::Menu => {
                if down {
                    self.ui_key(Key::Escape);
                }
            }
            TouchButton::Jump => self.hold_key(KeyCode::Space, down),
            TouchButton::Secondary => {
                if self.rpg_mode() {
                    if down {
                        self.rpg.ask(crate::rpg::Act::Secondary);
                    }
                } else if down {
                    self.press_in_game(MouseButton::Right);
                } else {
                    self.input.mouse.remove(&MouseButton::Right);
                }
            }
            TouchButton::Hot(i) => {
                let key = self
                    .online
                    .as_ref()
                    .and_then(|o| hotbar(o).get(i as usize).map(|c| c.key));
                match key {
                    Some("Shift") => self.hold_key(KeyCode::ShiftLeft, down),
                    Some("C") => self.hold_key(KeyCode::KeyC, down),
                    Some("LMB") if down => self.press_in_game(MouseButton::Left),
                    Some("LMB") => {
                        self.input.mouse.remove(&MouseButton::Left);
                    }
                    Some("RMB") if down => self.press_in_game(MouseButton::Right),
                    Some("RMB") => {
                        self.input.mouse.remove(&MouseButton::Right);
                    }
                    Some(k) => {
                        let digits = [
                            KeyCode::Digit1,
                            KeyCode::Digit2,
                            KeyCode::Digit3,
                            KeyCode::Digit4,
                            KeyCode::Digit5,
                            KeyCode::Digit6,
                            KeyCode::Digit7,
                            KeyCode::Digit8,
                        ];
                        if let Some(n) = k.parse::<usize>().ok().filter(|n| (1..=8).contains(n)) {
                            self.hold_key(digits[n - 1], down);
                        }
                    }
                    None => {}
                }
            }
        }
    }

    /// What the fingers did since the last frame (MODES.md 5.6), as the mouse and the
    /// keys would have done it: before the look, which turns by a finger's drag.
    fn fingers_frame(&mut self, up: bool) {
        if let Some(at) = self.tap_fire
            && at.elapsed().as_secs_f32() > touch::TAP_HOLD_SECS
        {
            self.tap_fire = None;
            self.input.mouse.remove(&MouseButton::Left);
        }
        if !self.fingers.seen {
            return;
        }
        let dpr = self
            .active
            .as_ref()
            .map_or(1.0, |a| a.window.scale_factor() as f32)
            .max(0.5);
        let reach = touch::STICK_DOTS * self.ui_scale();
        self.input.stick = self.fingers.stick(reach);
        for event in self.fingers.drain() {
            match event {
                TouchEvent::Press(Zone::Ui, at) => {
                    let now = Instant::now();
                    let double = self.last_press.is_some_and(|(when, where_)| {
                        now.duration_since(when).as_secs_f32() < DOUBLE_CLICK_SECS
                            && (where_.0 - at.0).abs() < DOUBLE_CLICK_PIXELS * dpr
                            && (where_.1 - at.1).abs() < DOUBLE_CLICK_PIXELS * dpr
                    });
                    self.last_press = (!double).then_some((now, at));
                    self.ui_press(at, double);
                }
                TouchEvent::Move(Zone::Ui, at) => self.cursor = at,
                TouchEvent::Release(Zone::Ui, at) => {
                    self.cursor = at;
                    if self.ui_input.down {
                        self.ui_release();
                    }
                }
                TouchEvent::Press(Zone::Button(b), _) => self.touch_button(b, true),
                TouchEvent::Release(Zone::Button(b), _) => self.touch_button(b, false),
                TouchEvent::Press(..) | TouchEvent::Move(..) | TouchEvent::Release(..) => {}
                TouchEvent::Tap(Zone::World, at) if !up => {
                    self.cursor = at;
                    self.rpg_click();
                }
                TouchEvent::Tap(Zone::Look, _) if !up => {
                    self.input.mouse.insert(MouseButton::Left);
                    self.tap_fire = Some(Instant::now());
                }
                TouchEvent::Tap(..) => {}
                TouchEvent::LongPress(Zone::World, at) if !up => {
                    if self.rpg.hover(self.rpg_under(at)) == crate::rpg::Hover::Attack {
                        self.rpg.ask(crate::rpg::Act::Secondary);
                    }
                }
                TouchEvent::LongPress(..) => {}
                TouchEvent::Drag(dx, dy) => {
                    let gain = touch::LOOK_GAIN / dpr;
                    self.input.mouse_dx += dx * gain;
                    self.input.mouse_dy += dy * gain;
                }
                TouchEvent::Pinch(ratio) => {
                    if self.rpg_mode() && ratio > 0.0 {
                        self.rpg.dist = (self.rpg.dist / ratio)
                            .clamp(crate::rpg::DIST_MIN, crate::rpg::DIST_MAX);
                    }
                }
            }
        }
    }

    /// The cursor the pointer shows (MODES.md 5.5): in the RPG mode with the pointer
    /// free, a crosshair over the target (a click attacks), a hand over another body (a
    /// click targets), the arrow elsewhere; the arrow everywhere else.
    fn show_cursor(&mut self) {
        let want = if self.rpg_mode() && !self.grabbed && !self.screen_up() {
            match self.rpg.hover(self.rpg_under(self.cursor)) {
                crate::rpg::Hover::Attack => CursorIcon::Crosshair,
                crate::rpg::Hover::Body => CursorIcon::Pointer,
                crate::rpg::Hover::Ground => CursorIcon::Default,
            }
        } else {
            CursorIcon::Default
        };
        if want != self.cursor_icon {
            self.cursor_icon = want;
            if let Some(a) = self.active.as_ref() {
                a.window.set_cursor(want);
            }
        }
    }

    /// Tab in the RPG mode (MODES.md 5.2): the nearest enemy in sight not yet cycled.
    fn rpg_cycle(&mut self) {
        let Some(c) = self.online.as_ref().and_then(|o| o.client.as_ref()) else {
            return;
        };
        let eye = c.mover.eye();
        let me = c.mover.mv.origin;
        let candidates: Vec<(u32, f32)> = self
            .rpg_bodies()
            .iter()
            .filter(|b| b.enemy)
            .filter(|b| gm_core::sim::sees(&self.bsp, eye, b.capsule().center()))
            .map(|b| (b.id, (b.origin - me).length()))
            .collect();
        self.rpg.cycle(&candidates);
    }

    /// The scope of the firearm in hand (MODES.md 3.2): its zoom, 0 without one.
    fn gun_scope(&self) -> u8 {
        let Some(c) = self.online.as_ref().and_then(|o| o.client.as_ref()) else {
            return 0;
        };
        if self.input.held > 1 {
            return 0;
        }
        c.mover
            .in_hand(&c.sheet.kit)
            .and_then(|i| c.sheet.kit.abilities[i as usize].firearm.as_ref())
            .map_or(0, |f| f.scope)
    }

    /// A frame of a replay (ANTICHEAT.md 3.4): its keys, its time, its scene, and the camera
    /// in the followed body's eyes, behind it, or above it.
    #[cfg(not(target_arch = "wasm32"))]
    fn playback_frame(&mut self, frame_dt: f32) -> (Vec3, f32, f32) {
        let Some(p) = &mut self.playback else {
            return (self.sim.eye(), self.sim.yaw, self.sim.pitch);
        };
        let pressed = |k: KeyCode, input: &mut Input| input.just_pressed.remove(&k);
        if pressed(KeyCode::BracketLeft, &mut self.input) {
            p.cycle(-1);
        }
        if pressed(KeyCode::BracketRight, &mut self.input) {
            p.cycle(1);
        }
        if pressed(KeyCode::Space, &mut self.input) {
            p.paused = !p.paused;
        }
        if p.paused {
            if pressed(KeyCode::Comma, &mut self.input) {
                p.step(-1);
            }
            if pressed(KeyCode::Period, &mut self.input) {
                p.step(1);
            }
        }
        if pressed(KeyCode::ArrowLeft, &mut self.input) {
            p.seek(p.time - 5.0);
        }
        if pressed(KeyCode::ArrowRight, &mut self.input) {
            p.seek(p.time + 5.0);
        }
        for (key, speed) in [
            (KeyCode::Digit1, 0.25),
            (KeyCode::Digit2, 0.5),
            (KeyCode::Digit3, 1.0),
            (KeyCode::Digit4, 2.0),
        ] {
            if pressed(key, &mut self.input) {
                p.speed = speed;
            }
        }
        self.input.just_pressed.clear();
        p.advance(frame_dt);
        self.entities.clear();
        self.bodies.clear();
        let outside = self.viewport == Viewport::Third;
        let (eye, yaw, pitch) = p.scene(outside, &mut self.bodies, &mut self.entities);
        if self.viewport == Viewport::Third {
            (third_person_camera(&self.bsp, eye, yaw, pitch), yaw, pitch)
        } else {
            (eye, yaw, pitch)
        }
    }

    /// Replace the world (BSP, mesh, renderer) with another map.
    fn switch_map(&mut self, bsp: Bsp, hash: u64) {
        self.rpg.forget_map();
        self.map_hash = hash;
        // A map that says what its air is (SOUND.md 3) is believed over its name.
        if let Some(air) = ambience_of(&bsp) {
            self.sound.air_named(air);
        }
        let mesh = world::build(&bsp, &self.palette);
        self.faces_total = mesh
            .face_ranges
            .iter()
            .filter(|r| r.index_count > 0)
            .count();
        if let Some(a) = &mut self.active {
            a.renderer.set_world(&a.gpu, &mesh);
            a.drawn_from = vec![usize::MAX];
        } else {
            self.mesh = Some(mesh);
        }
        self.sim = Sim::new(&bsp);
        self.bsp = bsp;
        log::info!(
            "switched map ({} faces, {} leaves)",
            self.bsp.faces.len(),
            self.bsp.leaves.len()
        );
    }

    /// One line of what the client measured since the last one (`--report`, WEB.md 8).
    fn report_line(&mut self) -> String {
        let r = self.stats.report_since(self.report_frame);
        self.report_frame = self.stats.frames();
        let mut line = format!(
            "fps={:.1} frame_ms_avg={:.2} frame_ms_p99={:.2} frame_ms_max={:.2}",
            r.fps_avg, r.ms_avg, r.ms_p99, r.ms_max
        );
        if let Some(a) = &self.active {
            let m = a.avatars.stats();
            line.push_str(&format!(
                " backend={:?} draw_calls={} characters={} with_model={} models={} fetched={} fetched_bytes={} cache_hits={} failed={} refused={}",
                a.gpu.info.backend,
                a.renderer.draw_calls,
                a.renderer.characters.drawn,
                a.avatars.with_model,
                m.ready,
                m.fetched,
                m.fetched_bytes,
                m.disk_hits,
                m.failed,
                m.refused
            ));
            #[cfg(target_arch = "wasm32")]
            if let Some(cache) = &a.avatars.cache {
                line.push_str(&format!(
                    " cache_bytes={} cache_cap_bytes={}",
                    cache.loader.store.total(),
                    cache.loader.store.cap()
                ));
            }
        }
        if let Some(o) = &self.online {
            if let Some(c) = &o.client {
                let s = c.stats;
                line.push_str(&format!(
                    " entity={} snapshots={} gaps={} max_gap={} corrections={} unexplained={} inputs={} health={}",
                    c.my_id,
                    s.snapshots,
                    s.gaps,
                    s.max_gap,
                    s.corrections,
                    s.corrections_unexplained,
                    s.inputs_sent,
                    c.own_health
                ));
            }
            line.push_str(&format!(
                " kills={} deaths={} players={} zone={}",
                o.kills,
                o.deaths,
                o.names.len(),
                if o.zone_name.is_empty() {
                    "-"
                } else {
                    &o.zone_name
                }
            ));
            // The hotbar as drawn (LOOK.md 3.2): key, ability, state, and how ready.
            let cells: Vec<String> = hotbar(o)
                .iter()
                .map(|c| format!("{}:{}:{}:{:.2}", c.key, c.ability, c.state, c.ready))
                .collect();
            if !cells.is_empty() {
                line.push_str(&format!(" hotbar={}", cells.join(",")));
            }
            let held: Vec<String> = o
                .looks
                .iter()
                .filter_map(|(id, l)| o.props.get(l.held as usize).map(|k| format!("{id}:{k}")))
                .collect();
            line.push_str(&format!(
                " held={} props_loaded={}",
                held.len(),
                self.content
                    .props
                    .values()
                    .filter(|p| matches!(p, crate::content::PropState::Loaded(_)))
                    .count()
            ));
            #[cfg(target_arch = "wasm32")]
            {
                let (rx, tx) = o.net.bytes();
                line.push_str(&format!(" rx_bytes={rx} tx_bytes={tx}"));
            }
        }
        // The UI's scale and the density of the atlas it is drawn with (LOOK.md 2.2):
        // the same number when the bundle has that atlas. Offline too (the phone gate).
        if let Some(a) = &self.active {
            line.push_str(&format!(
                " ui_scale={} atlas_density={} size={}x{} dpr={:.3}",
                ui::scale_for(a.renderer.hud.size, PANEL_UNITS, self.ui_scale_choice()),
                a.renderer.hud.density(),
                a.config.width,
                a.config.height,
                a.window.scale_factor()
            ));
        }
        // Where the body is and looks (offline, the local walk's), and whether a finger
        // has touched the screen: what the phone gate reads (WEB.md 3.5).
        let pos = self
            .online
            .as_ref()
            .and_then(|o| o.client.as_ref())
            .map_or(self.sim.curr.origin, |c| c.mover.mv.origin);
        let axes = self.input.axes();
        line.push_str(&format!(
            " yaw={:.0} pos={:.0},{:.0},{:.0} touch={} axes={:.2},{:.2}",
            self.sim.yaw, pos.x, pos.y, pos.z, self.fingers.seen as u8, axes.0, axes.1
        ));
        #[cfg(target_arch = "wasm32")]
        line.push_str(&format!(
            " wasm_memory_bytes={} first_frame_ms={:.0}",
            crate::web::wasm_memory_bytes(),
            self.first_frame_ms
        ));
        // What was heard, in the same line (the gates read it as `GM-DONE ... sound: ...`).
        line.push(' ');
        line.push_str(&self.sound.report());
        line
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        #[cfg(target_arch = "wasm32")]
        if self.active.is_none() {
            let made = self.pending_gpu.borrow_mut().take();
            match (made, self.window.clone()) {
                (Some(Ok((surface, gpu))), Some(window)) => {
                    if let Err(e) = self.activate(window, surface, gpu) {
                        self.fail(event_loop, &format!("renderer setup failed: {e}"));
                        return;
                    }
                }
                (Some(Err(e)), _) => {
                    self.fail(event_loop, &format!("renderer setup failed: {e}"));
                    return;
                }
                _ => {}
            }
        }
        if let Some((bsp, hash)) = self.pending_map.take() {
            self.switch_map(bsp, hash);
        }
        let now = Instant::now();
        let frame_dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        #[cfg(not(target_arch = "wasm32"))]
        self.pasted();
        if !self.run_script(event_loop) {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        self.page_frame();
        self.leaving
            .retain(|(net, since)| !net.gone() && since.elapsed().as_secs_f32() < 1.0);
        // A session lives while it is used, and a zone is played without a word to the hub:
        // now and then the client says one (HUB.md 3.1).
        if self.online.is_some()
            && self.hub_touched.elapsed().as_secs() >= SESSION_TOUCH_SECS
            && let (Some(hub), Some(account)) = (&self.hub, &self.account)
        {
            self.hub_touched = Instant::now();
            let session = account.session;
            // Nobody waits for the answer.
            let _ = hub.call(PlayerRequest::ListZones { session });
        }
        let returns = self.returns_to_screens();
        // The pointer follows the screens: free while one is up, the game's again after.
        // Not for a finger (WEB.md 3.5): a phone's browser gives the pointer lock to the
        // tap that put the screen down and takes it back at the next touch, and losing
        // it is Escape, so every tap in the game opened the menu.
        let up = self.screen_up();
        if up != self.was_up {
            self.was_up = up;
            if up {
                self.release_keys();
                self.set_grab(false);
            } else if self.wants_pointer() && self.focused() && !self.fingers.seen {
                self.set_grab(true);
            }
        }
        if self.capturing() {
            // Whatever was held is not held for the game while the toolkit has the keys.
            self.release_keys();
        }
        self.fingers_frame(up);

        // Mouse look is applied per frame for responsiveness; movement uses it at tick time.
        let bench = self.opts.bench_frames.is_some();
        if bench && self.opts.crowd > 0 {
            let t = self.started.elapsed().as_secs_f32();
            self.sim.yaw = self.bench_yaw0 + BENCH_CROWD_SWING_DEG * (t * 0.7).sin();
        } else if bench {
            self.sim.yaw += BENCH_YAW_DEG_PER_S * frame_dt;
        } else if (self.grabbed || self.fingers.turning()) && !up {
            let turn = self.settings.sensitivity / self.zoom.max(1.0);
            let tilt = if self.settings.invert { -turn } else { turn };
            self.sim.yaw -= self.input.mouse_dx * turn;
            let (low, high) = if self.rpg_mode() {
                (5.0, 80.0)
            } else {
                (-89.0, 89.0)
            };
            self.sim.pitch = (self.sim.pitch + self.input.mouse_dy * tilt).clamp(low, high);
        }
        // Taunted (MATRIX.md 8): the view is turned to the taunter with the body and held
        // there; the mouse moves nothing until the taunt is out.
        if let Some(c) = self.online.as_ref().and_then(|o| o.client.as_ref())
            && c.mover.statuses.has(gm_core::vocab::Status::Taunt)
            && c.mover.lock_yaw.is_some()
        {
            self.sim.yaw = c.mover.yaw;
        }
        self.sim.yaw = self.sim.yaw.rem_euclid(360.0);
        self.input.mouse_dx = 0.0;
        self.input.mouse_dy = 0.0;

        #[cfg(not(target_arch = "wasm32"))]
        let watching = self.playback.is_some();
        #[cfg(target_arch = "wasm32")]
        let watching = false;
        let (camera, cam_yaw, cam_pitch) = if watching {
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.playback_frame(frame_dt)
            }
            #[cfg(target_arch = "wasm32")]
            unreachable!()
        } else if self.online.is_some() {
            let camera = match self.online_frame(frame_dt, event_loop) {
                Some(cam) => cam,
                None => return,
            };
            // The zone has the character: the screens step aside.
            if self.front_up && self.online.as_ref().is_some_and(|o| o.client.is_some()) {
                self.front_up = false;
                self.chat.clear();
            }
            // Its connection ended and a person is here: back to the characters.
            if let Some(why) = self.zone_ended.take() {
                self.leave_zone(&why);
            }
            camera
        } else if self.front_up || self.title.is_some() {
            // Behind a screen with no zone: the map, turning slowly (CLIENT.md 2).
            self.entities.clear();
            self.bodies.clear();
            let yaw = self.bench_yaw0 + BACKDROP_DEG_PER_S * self.started.elapsed().as_secs_f32();
            (self.sim.eye(), yaw.rem_euclid(360.0), 0.0)
        } else {
            let walk = self.opts.script.as_deref() == Some("walk");
            let input = if bench {
                MoveInput {
                    yaw: self.sim.yaw,
                    ..Default::default()
                }
            } else if walk {
                // A second standing, five seconds forward, standing again (SOUND.md 7):
                // what the sound gate renders to a file.
                let t = self.started.elapsed().as_secs_f32();
                MoveInput {
                    yaw: self.sim.yaw,
                    forward: if (1.0..6.0).contains(&t) { 1.0 } else { 0.0 },
                    ..Default::default()
                }
            } else {
                self.input.move_input(self.sim.yaw)
            };
            self.input.just_pressed.clear();
            self.sim.advance(&self.bsp, &input, frame_dt);
            // Heard: the own body's steps and landings, offline too (its animation is
            // the local mover's; nobody else is here).
            {
                let v = self.sim.curr.velocity;
                let anim = if !self.sim.curr.on_ground {
                    gm_core::sim::anim::AIR
                } else if v.truncate().length() > 20.0 {
                    gm_core::sim::anim::RUN
                } else {
                    gm_core::sim::anim::IDLE
                };
                let here = self.sim.origin();
                let travel = match (self.sim.curr.on_ground, self.sound_from) {
                    (true, Some(from)) => (here - from).truncate().length(),
                    _ => 0.0,
                };
                self.sound_from = Some(here);
                let me = crate::sound::cues::OwnNow {
                    id: 0,
                    pos: self.sim.origin(),
                    on_ground: self.sim.curr.on_ground,
                    travel,
                    health: 0,
                    alive: true,
                };
                let now = self.started.elapsed().as_secs_f32();
                // (No zone here: the word's tick is the frame clock at 64 Hz.)
                self.sound.own(now, me, &[((now * 64.0) as u32, anim)], &[]);
                let listener = crate::sound::Listener {
                    pos: self.sim.eye(),
                    yaw: self.sim.yaw.to_radians(),
                };
                self.sound.end_frame(frame_dt, listener);
            }
            self.entities.clear();
            self.bodies.clear();
            match self.viewport {
                Viewport::First => {
                    // The fitting room of the view model (`--prop KEY`, LOOK.md 6.4):
                    // the prop in the view with its stride's bob; R held works a reload
                    // over and over, to see it.
                    let eye = self.sim.eye();
                    self.view_stride +=
                        self.sim.curr.velocity.truncate().length() * frame_dt / 64.0;
                    let t = self.started.elapsed().as_secs_f32();
                    let reload = if self.input.down(KeyCode::KeyR) {
                        (t / 2.4).fract()
                    } else {
                        0.0
                    };
                    self.view_model = self.offline_prop.map(|slot| ViewModel {
                        slot,
                        fit: self
                            .opts
                            .prop
                            .as_deref()
                            .map_or(Mat4::IDENTITY, |k| view_fit_of(&self.content, k)),
                        eye,
                        yaw: self.sim.yaw,
                        pitch: self.sim.pitch,
                        stride: if self.sim.curr.on_ground {
                            self.view_stride
                        } else {
                            0.0
                        },
                        kick: 0.0,
                        swing: 0.0,
                        reload,
                        light: crate::avatars::light_at(&self.bsp, eye),
                    });
                    (eye, self.sim.yaw, self.sim.pitch)
                }
                _ => {
                    let v = self.sim.curr.velocity;
                    self.bodies.push(Body {
                        key: OWN,
                        origin: self.sim.origin(),
                        yaw: self.sim.yaw,
                        pitch: self.sim.pitch,
                        anim: if !self.sim.curr.on_ground {
                            gm_core::sim::anim::AIR
                        } else if v.truncate().length() > 20.0 {
                            gm_core::sim::anim::RUN
                        } else {
                            gm_core::sim::anim::IDLE
                        },
                        crouched: self.sim.curr.on_ground
                            && (self.input.down(KeyCode::ControlLeft)
                                || self.input.down(KeyCode::ControlRight)
                                || self.input.down(KeyCode::KeyC)),
                        frame: 1,
                        armour: 0,
                        aspects: 0,
                        status: 0,
                        model: None,
                        distance: 0.0,
                        lit: 0.0,
                        // Offline (`--prop FILE`, CONTENT.md 9): the prop to look at.
                        prop: self.offline_prop,
                        off: self.offline_off,
                    });
                    (
                        third_person_camera(
                            &self.bsp,
                            self.sim.eye(),
                            self.sim.yaw,
                            self.sim.pitch,
                        ),
                        self.sim.yaw,
                        self.sim.pitch,
                    )
                }
            }
        };

        // Whether the character's page may wear its draft here, read before the renderer
        // is borrowed for the frame.
        let at_trainer = self.at_trainer();
        // The UI's scale this frame, and the controls a finger may land on (MODES.md
        // 5.6), which the HUD draws where they are hit: read before the renderer is
        // borrowed too.
        let chosen = self.ui_scale_choice();
        self.touch_buttons = if self.fingers.seen && !watching && !bench && !self.screen_up() {
            let size = self.active.as_ref().map_or((1.0, 1.0), |a| {
                (a.config.width.max(1) as f32, a.config.height.max(1) as f32)
            });
            touch_controls(size, self.ui_scale(), self.online.as_ref(), self.rpg_mode())
        } else {
            Vec::new()
        };
        let Some(a) = &mut self.active else { return };
        // The world is drawn from the camera's leaf.
        let leaves = vec![self.bsp.leaf_for_point(camera)];
        if a.drawn_from != leaves {
            if leaves.is_empty() || leaves.contains(&0) {
                a.renderer.set_visible_faces(&a.gpu, None);
            } else {
                a.renderer
                    .set_visible_faces(&a.gpu, Some(&visible_from(&self.bsp, &leaves)));
            }
            a.drawn_from = leaves;
        }

        let frame = match a.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                log::debug!("surface suboptimal; reconfiguring");
                a.gpu.queue.present(f);
                self.configure_surface();
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.acquire_timeouts += 1;
                if self.acquire_timeouts == 3 {
                    log::warn!(
                        "the display is not releasing frames (Fifo on a compositor, or a sleeping monitor); try --present mailbox"
                    );
                }
                return;
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                log::debug!("surface occluded");
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                log::debug!("surface outdated or lost; reconfiguring");
                self.configure_surface();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("surface validation error");
                event_loop.exit();
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(a.view_format),
            ..Default::default()
        });
        let aspect = a.config.width as f32 / a.config.height.max(1) as f32;
        for id in self.revoked.drain(..) {
            a.avatars.revoke(&id, &mut a.renderer.characters);
        }
        // The own avatar is the last thing the disk cache lets go of (MODELS.md 8).
        if let Some(o) = &self.online
            && let Some(c) = &o.client
            && let Some(id) = o.names.get(&c.my_id).and_then(|n| n.2)
        {
            a.avatars.pin(&id);
        }
        // Props that arrived from the site since last frame go on the GPU, one a frame
        // (CONTENT.md 6; natively a prop is read when first asked for).
        #[cfg(target_arch = "wasm32")]
        {
            let (gpu, characters) = (&a.gpu, &mut a.renderer.characters);
            self.content
                .poll(|model| Some(characters.add_model(gpu, model)));
        }
        a.avatars.begin_frame();
        if let Some(v) = self.view_model.take() {
            a.avatars.view_model(
                v.slot, v.fit, v.eye, v.yaw, v.pitch, v.stride, v.kick, v.swing, v.reload, v.light,
            );
        }
        for body in &self.bodies {
            a.avatars.push(
                body,
                frame_dt,
                &self.bsp,
                &a.renderer.characters,
                &mut self.entities,
            );
            // The ring of its aspects at its feet (MODELS.md 9, LOOK.md 13).
            if body.anim != gm_core::sim::anim::DEAD && body.aspects != 0 {
                crate::fx::aspect_ring(
                    &mut self.fx,
                    body.origin + Vec3::Z * Hull::Player.mins().z,
                    body.yaw,
                    body.aspects,
                );
            }
        }
        // The frame's effects go to the renderer, which draws them after the bodies.
        a.renderer.fx.verts.append(&mut self.fx.verts);
        a.avatars.push_crowd(
            self.started.elapsed().as_secs_f32(),
            frame_dt,
            camera,
            &self.bsp,
            &a.renderer.characters,
            &mut self.entities,
        );
        let vp = crate::render::view_proj_zoomed(camera, cam_yaw, cam_pitch, aspect, self.zoom);
        self.last_vp = Some((
            vp,
            (a.config.width.max(1) as f32, a.config.height.max(1) as f32),
        ));
        // One scale for the HUD and the screens: what the window gives, or what was chosen;
        // and the atlas made for that scale (LOOK.md 2.2), from the frame it is here.
        let scale = ui::scale_for(
            (a.config.width.max(1) as f32, a.config.height.max(1) as f32),
            PANEL_UNITS,
            chosen,
        );
        self.content.want_atlas(scale as u8);
        if let Some(atlas) = self.content.take_atlas() {
            a.renderer.hud.set_atlas(&a.gpu, atlas);
        }
        a.renderer.hud.begin((a.config.width, a.config.height));
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(p) = &self.playback {
            p.hud(&mut a.renderer.hud);
        }
        if !watching && !bench && !self.front_up && self.title.is_none() {
            build_hud(
                &mut a.renderer.hud,
                self.online.as_ref(),
                vp,
                HudView {
                    tags: &self.tags,
                    pops: &self.pops,
                    first_person: self.viewport == Viewport::First,
                    hurt: self.effects.own_hurt,
                    squad: &self.squad_view,
                    party: &self.party_view,
                    target: self.target_view.as_ref(),
                    scale,
                    manifest: self.content.manifest.as_ref(),
                    time: self.started.elapsed().as_secs_f32(),
                    own_name: &self.opts.character,
                    combo: (
                        self.combo.0,
                        self.combo.1.map_or(f32::MAX, |t| t.elapsed().as_secs_f32()),
                    ),
                    crouched: self.input.down(KeyCode::ControlLeft)
                        || self.input.down(KeyCode::ControlRight)
                        || self.input.down(KeyCode::KeyC),
                    zoom: self.zoom,
                    scoped: self.input.scoped,
                    touch: TouchHud {
                        buttons: &self.touch_buttons,
                        held: self
                            .touch_buttons
                            .iter()
                            .map(|(b, _)| *b)
                            .filter(|b| self.fingers.holds(*b))
                            .collect(),
                        stick: self.fingers.stick_drawn(),
                    },
                },
            );
        }
        // The screens, the menu and the chat, over the HUD (CLIENT.md 4 to 6).
        let mut front_actions = [Action::None, Action::None];
        let (mut menu_action, mut said) = (MenuAction::None, None);
        let mut bag_action = BagAction::None;
        let mut people_action = PeopleAction::None;
        let mut gm_action = GmAction::None;
        let mut character_action = CharacterAction::None;
        if !bench {
            let playing = self.online.as_ref().is_some_and(|o| o.client.is_some());
            let screen = match (&self.front, &self.menu, &self.bag) {
                _ if self.title.is_some() => "title",
                (Some(front), _, _) if self.front_up => front.screen.name(),
                (_, Some(menu), _) => menu.page.name(),
                (_, _, Some(bag)) => bag.page.name(),
                _ if self.people.is_some() => self.people.as_ref().map_or("", |p| p.page.name()),
                _ if self.gm_page.is_some() => self.gm_page.as_ref().map_or("", |g| g.name()),
                _ if self.character_page.is_some() => {
                    self.character_page.as_ref().map_or("", |p| p.name())
                }
                _ if self.chat.open => "chat",
                _ => "game",
            };
            // Whose stall the body stands at, for the corner of the screen.
            let near = Self::stall_in_reach(self.online.as_ref())
                .filter(|_| playing && self.hub.is_some())
                .map(|s| s.owner.clone());
            let me = self.opts.name.clone();
            let keeps_stall = self
                .online
                .as_ref()
                .is_some_and(|o| o.stalls.iter().any(|s| s.owner == me));
            self.ui_input.last_cursor = self.ui_input.cursor;
            self.ui_input.cursor = self.cursor;
            self.ui_input.time = self.started.elapsed().as_secs_f32();
            self.ui.paperdolls.clear();
            let mut ui = Ui::begin_at(
                &mut a.renderer.hud,
                &mut self.ui,
                &self.ui_input,
                screen,
                PANEL_UNITS,
                chosen,
            );
            if let Some(title) = &self.title {
                match crate::menu::title(&mut ui, title) {
                    Some(true) => self.title = None,
                    Some(false) => front_actions[1] = Action::Quit,
                    None => {}
                }
            } else if self.front_up {
                if let Some(front) = &mut self.front {
                    front_actions = front.frame(&mut ui);
                }
            } else {
                if playing {
                    said = self.chat.frame(&mut ui, &mut self.settings.ignored);
                } else if self.chat.open {
                    // Between two zones nothing is drawn of it: nor does it keep the keys.
                    self.chat.drop_line();
                }
                // The game goes away under an open inventory as under the menu; a body
                // that stopped playing (a travel, the zone gone) has neither.
                if !playing {
                    self.bag = None;
                    self.people = None;
                    self.gm_page = None;
                }
                match (&mut self.bag, &self.hub) {
                    (Some(bag), Some(hub)) if self.menu.is_none() => {
                        let looks = crate::bag::ItemLooks {
                            manifest: self.content.manifest.as_ref(),
                        };
                        bag_action =
                            bag.frame_with(&mut ui, hub, keeps_stall, Instant::now(), looks);
                    }
                    _ => {}
                }
                // The people here and of the party, a trade, the tavern (PARTY.md 8).
                if let (Some(people), Some(o), None, None) =
                    (&mut self.people, &mut self.online, &self.menu, &self.bag)
                    && let Some(c) = &o.client
                {
                    // Who is here: the bodies that are people, and whether each stands
                    // near enough to trade with (the rule the zone decides with).
                    let mine: [f32; 3] = c.mover.mv.origin.into();
                    let others = c.others_at(c.render_tick(0.0));
                    let mut here: Vec<Here> = o
                        .names
                        .iter()
                        .filter(|(id, _)| {
                            **id != c.my_id && o.kinds.get(id) == Some(&BodyKind::Human)
                        })
                        .map(|(id, (name, _, _))| Here {
                            body: *id,
                            name: name.clone(),
                            near: others.iter().find(|e| e.id == *id).is_some_and(|e| {
                                gm_net::control::trade_in_reach(mine, e.pos.into())
                            }),
                        })
                        .collect();
                    here.sort_by(|a, b| a.name.cmp(&b.name));
                    let hub = self
                        .hub
                        .as_ref()
                        .filter(|_| self.account.is_some() && self.character.is_some())
                        .map(|hub| hub as &dyn HubApi);
                    let unix = web_time::SystemTime::now()
                        .duration_since(web_time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    let word = self.chat.zone_said(std::time::Duration::from_secs(8));
                    people_action = people.frame_with(
                        &mut ui,
                        hub,
                        &me,
                        &mut o.social,
                        &here,
                        word,
                        Instant::now(),
                        unix,
                        crate::bag::ItemLooks {
                            manifest: self.content.manifest.as_ref(),
                        },
                    );
                }
                // The game master's page (GM.md 4).
                if let (Some(page), Some(o), None, None, None) = (
                    &mut self.gm_page,
                    &self.online,
                    &self.menu,
                    &self.bag,
                    &self.people,
                ) && let (Some(c), Some(pack)) = (&o.client, &o.pack)
                {
                    let view = GmView {
                        pack,
                        own: &c.sheet.build,
                        tuning: &o.tuning,
                        rate: o.rate,
                        note: &o.gm_note,
                    };
                    gm_action = page.frame(&mut ui, &view);
                }
                // The character's page (MATRIX.md 9.1).
                if let (Some(page), Some(o), None, None, None, None) = (
                    &mut self.character_page,
                    &self.online,
                    &self.menu,
                    &self.bag,
                    &self.people,
                    &self.gm_page,
                ) && let (Some(c), Some(pack)) = (&o.client, &o.pack)
                {
                    let view = CharacterView {
                        pack,
                        own: &c.sheet.build,
                        rate: o.rate,
                        at_trainer,
                        note: &o.respec_note,
                    };
                    character_action = page.frame(&mut ui, &view);
                }
                match &mut self.menu {
                    Some(_) if self.bag.is_some() => {}
                    Some(menu) => {
                        let hub = self
                            .hub
                            .as_ref()
                            .zip(self.account.as_ref())
                            .map(|(hub, account)| (hub as &dyn HubApi, account.session));
                        // A page's tab is closed and made fullscreen by the browser.
                        let offers = Offers {
                            inventory: playing && hub.is_some() && self.character.is_some(),
                            people: playing,
                            gm: playing && self.online.as_ref().is_some_and(|o| o.gm),
                            travel: playing && hub.is_some(),
                            leave: returns && self.online.is_some(),
                            fullscreen: cfg!(not(target_arch = "wasm32")),
                            #[cfg(target_arch = "wasm32")]
                            page_fullscreen: Some(crate::web::fullscreen_on()),
                            #[cfg(not(target_arch = "wasm32"))]
                            page_fullscreen: None,
                            quit: cfg!(not(target_arch = "wasm32")),
                        };
                        let here = self.online.as_ref().map_or("", |o| o.zone_name.as_str());
                        menu_action = menu.frame(&mut ui, hub, offers, here, &mut self.settings);
                    }
                    None if self.bag.is_some()
                        || self.people.is_some()
                        || self.gm_page.is_some()
                        || self.character_page.is_some() => {}
                    None if playing && !self.chat.open => {
                        // Where this is, and the two keys nothing else tells of.
                        let here = self.online.as_ref().map_or("", |o| o.zone_name.as_str());
                        let hint = if here.is_empty() {
                            "Esc menu  Enter chat".to_string()
                        } else {
                            format!("{here}  Esc menu  Enter chat")
                        };
                        let (w, h) = ui.size();
                        ui.small(w - 16.0, h - 14.0 * ui.scale, ui::FAINT, &hint);
                        // And the stall the body stands at, whose it is and the key.
                        if let Some(owner) = &near {
                            let look = format!("{owner}'s stall  E look");
                            ui.small(w - 16.0, h - 24.0 * ui.scale, ui::TEXT, &look);
                        }
                    }
                    None => {}
                }
            }
            ui.end();
            if std::mem::take(&mut self.ui.presses) > 0 {
                self.sound.play(crate::sound::synth::Cue::Click);
            }
            self.ui_drawn = true;
            // What happened is used up; a button still held is still held.
            self.ui_input = UiInput {
                down: self.ui_input.down,
                // The pointer stays where it is: the paperdoll turns by how far it moved
                // since last frame, not by where it is.
                cursor: self.ui_input.cursor,
                ..Default::default()
            };
        }
        // The paperdoll (LOOK.md 5): the own body as the equip panel shows it, idle,
        // turned by the drag across it, with what it holds, drawn into the panel's
        // rectangle by a camera of its own.
        let doll_draws: Vec<CharacterDraw> = match (self.ui.paperdolls.first(), &self.online) {
            // The selector's body (CLIENT.md 4.2): the character or the archetype shown,
            // in the open, with the props of its weapon and its guard.
            (Some(doll), _) if self.front_up && doll.rect.w > 1.0 && doll.rect.h > 1.0 => {
                match self.front.as_ref().and_then(|f| f.shown()) {
                    Some((pack, build, model)) => {
                        let prop =
                            ability_prop(&mut self.content, Some(a), pack, Some(build.primary));
                        let off = ability_prop(&mut self.content, Some(a), pack, build.guard);
                        let body = Body {
                            key: DOLL,
                            origin: Vec3::new(0.0, 0.0, 24.0),
                            yaw: 180.0 + doll.turn * 360.0,
                            pitch: 0.0,
                            anim: gm_core::sim::anim::IDLE,
                            crouched: false,
                            frame: gm_model::rig::frame_index(build.frame),
                            armour: build.armour as u8,
                            aspects: build.aspects.0,
                            status: 0,
                            model,
                            distance: 0.0,
                            lit: 0.0,
                            prop,
                            off,
                        };
                        a.avatars
                            .doll(&body, frame_dt, &self.bsp, &a.renderer.characters)
                    }
                    None => Vec::new(),
                }
            }
            (Some(doll), Some(o)) if doll.rect.w > 1.0 && doll.rect.h > 1.0 => match &o.client {
                Some(c) => {
                    let build = &c.sheet.build;
                    let body = Body {
                        key: DOLL,
                        origin: Vec3::new(0.0, 0.0, 24.0),
                        yaw: 180.0 + doll.turn * 360.0,
                        pitch: 0.0,
                        anim: gm_core::sim::anim::IDLE,
                        crouched: false,
                        frame: gm_model::rig::frame_index(build.frame),
                        armour: build.armour as u8,
                        aspects: build.aspects.0,
                        status: 0,
                        model: o.names.get(&c.my_id).and_then(|n| n.2),
                        distance: 0.0,
                        lit: 0.0,
                        prop: held_prop(
                            &mut self.content,
                            Some(a),
                            &o.props,
                            o.looks.get(&c.my_id).copied().unwrap_or_default(),
                        ),
                        off: off_prop(
                            &mut self.content,
                            Some(a),
                            &o.props,
                            o.looks.get(&c.my_id).copied().unwrap_or_default(),
                        ),
                    };
                    a.avatars
                        .doll(&body, frame_dt, &self.bsp, &a.renderer.characters)
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        let doll = self.ui.paperdolls.first().map(|d| {
            let aspect = d.rect.w / d.rect.h.max(1.0);
            let vp = if d.open {
                // The whole body, head to feet, from a little further back.
                view_proj(
                    Vec3::new(DOLL_OPEN_DISTANCE, 0.0, 34.0),
                    180.0,
                    -1.0,
                    aspect,
                )
            } else {
                // In front of the body, at its chest, looking back at it.
                view_proj(Vec3::new(82.0, 0.0, 31.0), 180.0, -3.0, aspect)
            };
            (d.rect, vp, doll_draws.as_slice())
        });
        a.renderer
            .render_with_doll(&a.gpu, &view, vp, &self.entities, &a.avatars.draws, doll);
        a.avatars.end_frame(&a.gpu, &mut a.renderer.characters);
        a.window.pre_present_notify();
        a.gpu.queue.present(frame);
        self.acquire_timeouts = 0;
        // A bench counts frames once every model its crowd wears is on the GPU.
        if bench
            && a.avatars.cache.as_ref().is_some_and(|c| c.pending() > 0)
            && self.started.elapsed().as_secs_f32() < 60.0
        {
            return;
        }
        self.stats.frame();
        #[cfg(target_arch = "wasm32")]
        if self.first_frame_ms == 0.0 {
            self.first_frame_ms = web_sys::window()
                .and_then(|w| w.performance())
                .map_or(0.0, |p| p.now());
        }
        #[cfg(not(target_arch = "wasm32"))]
        if !bench && self.opts.max_fps > 0 {
            // Cheap CPU-side cap so an uncapped present mode does not spin the GPU at 100%.
            let budget = std::time::Duration::from_secs_f64(1.0 / self.opts.max_fps as f64);
            let spent = now.elapsed();
            if spent < budget {
                std::thread::sleep(budget - spent);
            }
        }
        if bench && self.stats.frames() > 0 && self.stats.frames().is_multiple_of(60) {
            log::debug!("bench frame {}", self.stats.frames());
        }

        if self.last_title.elapsed().as_secs_f32() >= 1.0 {
            let r = self.stats.report_since(self.title_frame);
            let vp = match self
                .online
                .as_ref()
                .and_then(|o| o.client.as_ref())
                .map(|c| c.sheet.kit.mode)
            {
                Some(mode) => mode.name(),
                None => match self.viewport {
                    Viewport::First => "1st",
                    Viewport::Third => "3rd",
                },
            };
            let title = match &self.online {
                Some(o) => {
                    let (hp, max_hp, st, fo, corr, delay) =
                        o.client.as_ref().map_or((0, 0, 0.0, 0.0, 0, 0), |c| {
                            (
                                c.own_health,
                                c.sheet.derived.health,
                                c.mover.stamina,
                                c.mover.focus,
                                c.stats.corrections,
                                c.delay_ticks,
                            )
                        });
                    let models = a.avatars.stats();
                    format!(
                        "gamengine [{}{} team {} {vp}]  hp {hp}/{max_hp}  st {st:.0}  fo {fo:.0}  k {} d {}  {} players  {} stalls  {} models  delay {delay}  corr {corr}  {:.0} fps  {}",
                        if o.zone_name.is_empty() {
                            String::new()
                        } else {
                            format!("{} ", o.zone_name)
                        },
                        o.build_name,
                        o.team,
                        o.kills,
                        o.deaths,
                        o.names.len(),
                        o.stalls.len(),
                        models.ready,
                        r.fps_avg,
                        o.respec_note
                    )
                }
                None => format!(
                    "gamengine [{vp}]  {:.0} fps  {:.2} ms  {} / {} faces",
                    r.fps_avg, r.ms_avg, a.renderer.faces_drawn, self.faces_total
                ),
            };
            a.window.set_title(&title);
            self.title_frame = self.stats.frames();
            self.last_title = Instant::now();
            self.keep_settings();
        }
        if let Some(n) = self.opts.bench_frames
            && self.stats.frames() >= n as usize
        {
            event_loop.exit();
        }
        if self.opts.report && self.last_report.elapsed().as_secs_f32() >= 1.0 {
            self.last_report = Instant::now();
            let line = self.report_line();
            #[cfg(not(target_arch = "wasm32"))]
            println!("stats: {line}");
            #[cfg(target_arch = "wasm32")]
            crate::web::tell_page("stats", &line);
        }
        if self.opts.seconds > 0.0 && self.started.elapsed().as_secs_f32() >= self.opts.seconds {
            #[cfg(target_arch = "wasm32")]
            {
                // The whole run in one line, for whoever scripted it.
                self.report_frame = 0;
                let line = self.report_line();
                if let Some(o) = &mut self.online {
                    o.net.close();
                }
                if let Some(hub) = &self.hub {
                    hub.logout();
                }
                crate::web::tell_page("done", &line);
            }
            event_loop.exit();
        }
        let [answered, clicked] = front_actions;
        self.bag_act(bag_action);
        self.people_act(people_action);
        self.gm_act(gm_action);
        self.character_act(character_action);
        self.act(answered, menu_action, said, event_loop);
        self.act(clicked, MenuAction::None, None, event_loop);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() || self.window.is_some() {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let attrs = Window::default_attributes()
            .with_title("gamengine")
            .with_inner_size(winit::dpi::PhysicalSize::new(
                self.opts.width,
                self.opts.height,
            ))
            // The smallest frame the screens are made for (CLIENT.md 3).
            .with_min_inner_size(winit::dpi::PhysicalSize::new(640, 360));
        // The page owns the canvas and its size (WEB.md 5).
        #[cfg(target_arch = "wasm32")]
        let attrs = {
            use wasm_bindgen::JsCast;
            use winit::platform::web::WindowAttributesExtWebSys;
            let canvas = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.get_element_by_id("gm-canvas"))
                .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok());
            Window::default_attributes()
                .with_title("gamengine")
                .with_canvas(canvas)
                .with_prevent_default(true)
                .with_focusable(true)
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.fail(event_loop, &format!("window creation failed: {e}"));
                return;
            }
        };
        self.window = Some(window.clone());
        #[cfg(not(target_arch = "wasm32"))]
        {
            let instance = wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
            );
            let result = instance
                .create_surface(window.clone())
                .map_err(Error::from)
                .and_then(|surface| {
                    let gpu = Gpu::new(&instance, Some(&surface), self.opts.software)?;
                    self.activate(window.clone(), surface, gpu)
                });
            if let Err(e) = result {
                self.fail(event_loop, &format!("renderer setup failed: {e}"));
            }
        }
        // A browser hands out its GPU device asynchronously: the frame picks it up.
        #[cfg(target_arch = "wasm32")]
        {
            let slot = self.pending_gpu.clone();
            let software = self.opts.software;
            wasm_bindgen_futures::spawn_local(async move {
                let made = async {
                    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
                    // Each build draws through one API (WEB.md 3.2).
                    desc.backends = if cfg!(feature = "webgl") {
                        wgpu::Backends::GL
                    } else {
                        wgpu::Backends::BROWSER_WEBGPU
                    };
                    let instance = wgpu::Instance::new(desc);
                    let surface = instance
                        .create_surface(window.clone())
                        .map_err(|e| e.to_string())?;
                    let gpu = Gpu::request(&instance, Some(&surface), software)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok((surface, gpu))
                }
                .await;
                *slot.borrow_mut() = Some(made);
                window.request_redraw();
            });
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.configure_surface(),
            WindowEvent::Focused(false) => {
                self.input.keys.clear();
                self.input.mouse.clear();
                self.fingers.clear();
                self.set_grab(false);
            }
            WindowEvent::ModifiersChanged(held) => {
                let held = held.state();
                self.shift = held.shift_key();
                self.command = (held.control_key() || held.super_key()) && !held.alt_key();
            }
            WindowEvent::Focused(true) => {
                if cfg!(not(target_arch = "wasm32")) && self.wants_pointer() {
                    self.set_grab(true);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
            }
            WindowEvent::Touch(t) => {
                let at = (t.location.x as f32, t.location.y as f32);
                let phase = match t.phase {
                    TouchPhase::Started => touch::Phase::Started,
                    TouchPhase::Moved => touch::Phase::Moved,
                    TouchPhase::Ended => touch::Phase::Ended,
                    TouchPhase::Cancelled => touch::Phase::Cancelled,
                };
                let zone = self.touch_zone(at);
                let slop = touch::SLOP_DOTS * self.ui_scale();
                self.fingers
                    .touch(t.id, phase, at, Instant::now(), slop, |_| zone);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let turn = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                if self.screen_up() {
                    self.ui_input.wheel += turn;
                } else if self.rpg_mode() {
                    // The orbit camera's distance (MODES.md 5.5).
                    self.rpg.dist = (self.rpg.dist - turn * 20.0)
                        .clamp(crate::rpg::DIST_MIN, crate::rpg::DIST_MAX);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match state {
                ElementState::Pressed => {
                    if self.screen_up() {
                        // The pointer is the screen's (CLIENT.md 3): a click is a widget's.
                        if button == MouseButton::Left {
                            let now = Instant::now();
                            let double = self.last_press.is_some_and(|(at, where_)| {
                                now.duration_since(at).as_secs_f32() < DOUBLE_CLICK_SECS
                                    && (where_.0 - self.cursor.0).abs() < DOUBLE_CLICK_PIXELS
                                    && (where_.1 - self.cursor.1).abs() < DOUBLE_CLICK_PIXELS
                            });
                            self.last_press = (!double).then_some((now, self.cursor));
                            self.ui_press(self.cursor, double);
                        }
                    } else if self.rpg_mode() {
                        // The RPG mode (MODES.md 5.5): the left button picks a body or a
                        // place on the ground, the right one held turns the camera.
                        match button {
                            MouseButton::Left => self.rpg_click(),
                            MouseButton::Right => {
                                self.rpg_right = Some((
                                    Instant::now(),
                                    self.cursor,
                                    self.sim.yaw,
                                    self.sim.pitch,
                                ));
                                self.input.mouse.insert(button);
                                self.set_grab(true);
                            }
                            _ => {}
                        }
                    } else if !self.grabbed && self.opts.bench_frames.is_none() {
                        self.set_grab(true);
                    } else {
                        self.press_in_game(button);
                    }
                }
                ElementState::Released => {
                    self.input.mouse.remove(&button);
                    if button == MouseButton::Left && self.ui_input.down {
                        self.ui_release();
                    }
                    if button == MouseButton::Right && self.rpg_mode() && self.grabbed {
                        self.set_grab(false);
                        self.rpg_right_release();
                    }
                }
            },
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let code = match event.physical_key {
                    PhysicalKey::Code(code) => Some(code),
                    PhysicalKey::Unidentified(_) => None,
                };
                // The keys the screens act on (CLIENT.md 6).
                let key = match code {
                    Some(KeyCode::Enter | KeyCode::NumpadEnter) => Some(Key::Enter),
                    Some(KeyCode::Escape) => Some(Key::Escape),
                    Some(KeyCode::Tab) if self.shift => Some(Key::BackTab),
                    Some(KeyCode::Tab) => Some(Key::Tab),
                    Some(KeyCode::Backspace) => Some(Key::Backspace),
                    Some(KeyCode::Delete) => Some(Key::Delete),
                    Some(KeyCode::ArrowLeft) => Some(Key::Left),
                    Some(KeyCode::ArrowRight) => Some(Key::Right),
                    Some(KeyCode::ArrowUp) => Some(Key::Up),
                    Some(KeyCode::ArrowDown) => Some(Key::Down),
                    Some(KeyCode::Home) => Some(Key::Home),
                    Some(KeyCode::End) => Some(Key::End),
                    Some(KeyCode::PageUp) => Some(Key::PageUp),
                    Some(KeyCode::PageDown) => Some(Key::PageDown),
                    _ => None,
                };
                if self.capturing() {
                    // The toolkit has the keyboard: the game gets nothing.
                    if !pressed {
                        return;
                    }
                    // Ctrl+V and Shift+Insert paste (the key that types `v`, whatever
                    // the layout; the V key where the layout has no such letter).
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let v = match event.logical_key.as_ref() {
                            winit::keyboard::Key::Character(c) if c.is_ascii() => {
                                c.eq_ignore_ascii_case("v")
                            }
                            _ => code == Some(KeyCode::KeyV),
                        };
                        if (self.command && v) || (self.shift && code == Some(KeyCode::Insert)) {
                            self.paste();
                            return;
                        }
                    }
                    match (key, &event.text) {
                        // A key held down does not press Enter or Escape again: the
                        // line it opened would close, the login it sent be sent again.
                        (Some(Key::Enter | Key::Escape), _) if event.repeat => {}
                        (Some(key), _) => self.ui_key(key),
                        // With Ctrl held a key is a shortcut, not a letter (and a key
                        // that has no place on this keyboard's map still has its text:
                        // a program that types for a person sends such).
                        (None, Some(text)) if !self.command => self.ui_text(text),
                        _ => {}
                    }
                    return;
                }
                let Some(code) = code else { return };
                match event.state {
                    ElementState::Pressed => {
                        if self.input.keys.insert(code) {
                            self.input.just_pressed.insert(code);
                        }
                        match (code, key) {
                            // Enter opens the chat line, Escape the menu.
                            (_, Some(key @ (Key::Enter | Key::Escape))) if !event.repeat => {
                                self.ui_key(key)
                            }
                            _ => {}
                        }
                        match code {
                            // The inventory, and the stall the body stands at.
                            KeyCode::KeyI if !event.repeat => self.ui_key(Key::Inventory),
                            KeyCode::KeyE if !event.repeat => self.ui_key(Key::Use),
                            KeyCode::KeyP if !event.repeat => self.ui_key(Key::People),
                            KeyCode::KeyG if !event.repeat => self.ui_key(Key::Gm),
                            KeyCode::KeyK if !event.repeat => self.ui_key(Key::Character),
                            KeyCode::Tab if !event.repeat => self.ui_key(Key::Tab),
                            // Outside a zone only (the fitting room, a replay): in one
                            // the character's mode is the camera (MODES.md 2).
                            KeyCode::KeyV if !event.repeat && self.online.is_none() => {
                                self.viewport = match self.viewport {
                                    Viewport::First => Viewport::Third,
                                    Viewport::Third => Viewport::First,
                                };
                                log::info!("viewport: {:?}", self.viewport);
                            }
                            _ => {}
                        }
                    }
                    ElementState::Released => {
                        self.input.keys.remove(&code);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.show_cursor();
                self.frame(event_loop);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.grabbed
        {
            self.input.mouse_dx += delta.0 as f32;
            self.input.mouse_dy += delta.1 as f32;
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window
            && !self.exit_requested
        {
            w.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::collide::BoxWorld;

    #[test]
    fn a_running_body_faces_its_travel_and_an_acting_one_its_aim() {
        use gm_core::sim::anim;
        let side = Vec3::new(0.0, 200.0, 0.0);
        // Looking along +x and stepping to the left (+y): drawn running that way, turned
        // to it over a few frames rather than at once.
        assert_eq!(facing(None, 0.0, side, anim::RUN, false, 0.016), 90.0);
        let turned = facing(Some(0.0), 0.0, side, anim::RUN, false, 0.05);
        assert!(turned > 30.0 && turned < 40.0, "{turned}");
        let there = facing(Some(turned), 0.0, side, anim::RUN, false, 0.25);
        assert_eq!(there, 90.0);
        // Winding up a blow: back to the aim, where the blow lands. In the air, the travel.
        assert_eq!(
            facing(Some(90.0), 0.0, side, anim::WINDUP, false, 0.25),
            0.0
        );
        assert_eq!(facing(Some(90.0), 0.0, side, anim::AIR, false, 0.25), 90.0);
        // Standing still, or guarding: the aim. Backing off: a backpedal, the aim kept.
        assert_eq!(
            facing(None, 30.0, Vec3::ZERO, anim::RUN, false, 0.016),
            30.0
        );
        assert_eq!(facing(None, 30.0, side, anim::GUARD, false, 0.016), 30.0);
        let back = Vec3::new(-200.0, 50.0, 0.0);
        assert_eq!(facing(None, 0.0, back, anim::RUN, false, 0.016), 0.0);
        // The turn takes the short way round.
        let near = facing(
            Some(350.0),
            0.0,
            Vec3::new(200.0, 60.0, 0.0),
            anim::RUN,
            false,
            0.01,
        );
        assert!(!(10.0..=350.0).contains(&near), "{near}");
    }

    #[test]
    fn an_rpg_body_stands_as_it_was_left_and_turns_only_for_an_action() {
        use gm_core::sim::anim;
        let still = Vec3::ZERO;
        // Standing, the camera orbits (the look turns): the body stays where it was drawn.
        assert_eq!(facing(Some(90.0), 0.0, still, anim::IDLE, true, 0.25), 90.0);
        assert_eq!(
            facing(Some(90.0), 180.0, still, anim::GUARD, true, 0.25),
            90.0
        );
        // Never drawn yet: the look, once.
        assert_eq!(facing(None, 30.0, still, anim::IDLE, true, 0.016), 30.0);
        // Walking toward the camera (S): a run that way, not a backpedal.
        let back = Vec3::new(-200.0, 0.0, 0.0);
        assert_eq!(facing(Some(180.0), 0.0, back, anim::RUN, true, 0.25), 180.0);
        assert_eq!(facing(Some(180.0), 0.0, back, anim::RUN, false, 0.25), 0.0);
        // An action without a target fires the camera's way: the body turns to it.
        assert_eq!(facing(Some(90.0), 0.0, still, anim::CAST, true, 0.25), 0.0);
        // A target-action's turn: the caller passes the mover's yaw and no freedom.
        assert_eq!(
            facing(Some(90.0), 45.0, still, anim::IDLE, false, 0.25),
            45.0
        );
    }

    #[test]
    fn third_person_camera_stays_out_of_walls() {
        let mut world = BoxWorld::floor();
        // A wall right behind the player (yaw 0 looks +x, the camera goes -x).
        world.push(
            Vec3::new(-60.0, -256.0, 0.0),
            Vec3::new(-40.0, 256.0, 200.0),
        );
        let eye = Vec3::new(0.0, 0.0, 46.0);
        let cam = third_person_camera(&world, eye, 0.0, 0.0);
        assert!(cam.x > -40.0, "camera inside the wall: {cam:?}");
        assert!(cam.x < -20.0, "camera not pulled back at all: {cam:?}");
        let open = third_person_camera(&BoxWorld::floor(), eye, 0.0, 0.0);
        assert!((open.x + CAMERA_BACK).abs() < 1e-3);
    }

    #[test]
    fn re_aim_targets_the_point_under_the_crosshair() {
        let world = BoxWorld::floor();
        let eye = Vec3::new(0.0, 0.0, 46.0);
        let cam = third_person_camera(&world, eye, 0.0, 0.0);
        // A body 300 u ahead on the camera ray (the camera sits CAMERA_RIGHT to the right,
        // which is -y at yaw 0): the ray hits it and the eye aims at the hit point, a few
        // degrees to the right of straight ahead.
        let body = Aabb::around(Vec3::new(300.0, -CAMERA_RIGHT, 40.0), Hull::Player);
        let (yaw, pitch) = re_aim(&world, &[body], cam, 0.0, 0.0, eye);
        assert!(yaw > 350.0 && yaw < 359.0, "yaw {yaw}");
        assert!(pitch.abs() < 5.0, "pitch {pitch}");
        // With nothing to hit, the eye ray converges on the far point: nearly parallel.
        let (yaw, _) = re_aim(&world, &[], cam, 0.0, 0.0, eye);
        assert!(!(1.0..=359.0).contains(&yaw), "yaw {yaw}");
    }
}

//! What a character owns and what a stall sells (ITEMS.md 6): the inventory, the account's
//! storage, the price of a thing put up for sale, and the stall the body stands at.
//! Everything on these screens is the hub's word: what an item is, where it is worn and
//! what it does arrive with it in words, and the screens work nothing out. Buying and
//! wearing are said to the zone, which knows where the body stands and whether it is in a
//! fight.
//!
//! Three rules hold on every page (PLAN.md 6):
//! - A list is asked for when its page opens and after something the person did, and at no
//!   other time: what is on the screen does not move under the pointer.
//! - What is picked is picked by what it is (an item's id, a listing's id). When that is
//!   gone, nothing is picked, and no button acts until the person picks again: nothing
//!   takes a vanished thing's place by standing in its row.
//! - Nothing is said that the hub did not say: a list that has not been answered is not
//!   an empty list.

use gm_hub_proto::player::{PlayerEcon, PlayerEconReply, PlayerRequest, PlayerResponse};
use gm_hub_proto::protocol::{
    CharacterId, HubError, ItemSummary, ListingSummary, PLACE_ARMOUR, PLACE_NONE, PLACE_WEAPON,
    SessionId,
};
use web_time::{Duration, Instant};

use crate::font::ADVANCE;
use crate::front::PANEL_UNITS;
use crate::hub::{Answer, HubApi, Pending, RpcError};
use crate::ui::{self, Canvas, Column, Field, Key, NONE, Rect, SlotMark, SlotThing, Ui};

/// Where an item's picture comes from (LOOK.md 4): the bundle's manifest, by the item's
/// template (a whole thing) or its first material (a part); none without a bundle.
#[derive(Clone, Copy, Default)]
pub struct ItemLooks<'a> {
    pub manifest: Option<&'a gm_model::manifest::Manifest>,
}

impl ItemLooks<'_> {
    pub fn icon(&self, item: &ItemSummary) -> Option<String> {
        let m = self.manifest?;
        if item.place == PLACE_NONE {
            let (_, material) = item.components.first()?;
            m.material(material)?.icon.clone()
        } else {
            m.template(&item.template)?.icon.clone()
        }
    }

    /// The thing a grid shows for an item, said as its row was (`sword  slash +2.0%  worn`).
    pub fn thing(&self, item: &ItemSummary) -> SlotThing {
        let worn = if item.worn { "  worn" } else { "" };
        SlotThing {
            id: item.id,
            name: name(item),
            said: format!("{}  {}{worn}", name(item), headline(item)),
            icon: self.icon(item),
            worn: item.worn,
            mark: item.worn.then_some(SlotMark::Worn),
            ..Default::default()
        }
    }
}

/// The lines of an item's tooltip (LOOK.md 2.4): what it is, what it does, what is made
/// of; and what is worn in its place, when `worn` (the inventory) is known.
pub(crate) fn tooltip_lines(
    item: &ItemSummary,
    worn: Option<&[ItemSummary]>,
) -> Vec<(String, [f32; 4])> {
    let mut lines = vec![(name(item), ui::FOCUS), (item.what.clone(), ui::TEXT)];
    for d in &item.does {
        lines.push((d.clone(), ui::TEXT));
    }
    if let (true, Some(worn)) = (item.place != PLACE_NONE && !item.worn, worn) {
        let now = worn
            .iter()
            .find(|w| w.worn && w.place == item.place && w.id != item.id);
        lines.push(match now {
            Some(w) => (format!("you wear: {}", w.does.join("  ")), ui::FAINT),
            None => ("you wear nothing in its place".to_string(), ui::FAINT),
        });
    }
    lines.push((made_of(item), ui::FAINT));
    lines
}

/// A colour for each unit of coin (ECONOMY.md 2: silver and gold, nothing smaller).
pub const GOLD: [f32; 4] = [0.96, 0.80, 0.26, 1.0];
pub const SILVER: [f32; 4] = [0.80, 0.83, 0.88, 1.0];

/// Silver in a gold.
pub const SILVER_PER_GOLD: i64 = 100;

/// The most a price may be, in silver (the hub's own limit: a hundred million gold).
const MAX_PRICE: i64 = 10_000_000_000;
/// The slots a grid draws (ECONOMY.md 3: what a holder has room for).
const INVENTORY_SLOTS: usize = 24;
const STORAGE_SLOTS: usize = 60;
const STALL_SLOTS: usize = 12;
/// The zone takes one request of a kind from a player in a second; and one it never
/// answers is given up after `PATIENCE`.
const ZONE_GAP: Duration = Duration::from_millis(1100);
const PATIENCE: Duration = Duration::from_secs(12);
/// What a row has room for of a price: the units that do not fit are in the line under
/// the list, and the row says that there are more.
const PRICE_CHARS: usize = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Inventory,
    Storage,
    Price,
    Stall,
}

impl Page {
    /// The name a UI script waits for (CLIENT.md 9).
    pub fn name(self) -> &'static str {
        match self {
            Page::Inventory => "inventory",
            Page::Storage => "storage",
            Page::Price => "price",
            Page::Stall => "stall",
        }
    }
}

/// What the app must say to the zone after a frame of these screens.
#[derive(Clone, Debug, PartialEq)]
pub enum BagAction {
    None,
    Close,
    /// Buy this listing of the stall the body stands at, at the price shown (ITEMS.md 5).
    Buy {
        stall: i64,
        listing: i64,
        price: i64,
    },
    /// Put this item of the inventory on, or take it off (ITEMS.md 2).
    Wear {
        item: i64,
    },
    TakeOff {
        item: i64,
    },
}

/// The three lists the hub is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum List {
    Inventory = 0,
    Storage = 1,
    Stall = 2,
}

/// What the hub was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    Read(List),
    /// The item bar (LOOK.md 3.2): the four cells' templates.
    Bar,
    /// Something that changes what is where; what to say when it is done.
    Change(&'static str),
}

struct Stall {
    id: i64,
    owner: String,
    mine: bool,
    /// `None` until the hub has said.
    listings: Option<Vec<ListingSummary>>,
    picked: usize,
}

/// A request the zone has yet to answer: what it was about, and when the last of its kind
/// was made.
#[derive(Default)]
struct Asked {
    waiting: Option<i64>,
    at: Option<Instant>,
}

impl Asked {
    /// The zone would take another now.
    fn free(&self, now: Instant) -> bool {
        self.waiting.is_none()
            && self
                .at
                .is_none_or(|at| now.saturating_duration_since(at) > ZONE_GAP)
    }

    fn begin(&mut self, about: i64, now: Instant) {
        self.waiting = Some(about);
        self.at = Some(now);
    }

    /// The zone never answered: `true` once, when the waiting is given up.
    fn gave_up(&mut self, now: Instant) -> bool {
        let late = self.waiting.is_some()
            && self
                .at
                .is_some_and(|at| now.saturating_duration_since(at) > PATIENCE);
        if late {
            self.waiting = None;
        }
        late
    }

    /// An answer about `about` came: `true` when it is the one waited for.
    fn answered(&mut self, about: i64) -> bool {
        let mine = self.waiting == Some(about);
        if mine {
            self.waiting = None;
        }
        mine
    }
}

pub struct Bag {
    pub page: Page,
    session: SessionId,
    character: CharacterId,
    /// `None` until the hub has said.
    coin: Option<i64>,
    items: Option<Vec<ItemSummary>>,
    picked: usize,
    stored: Option<Vec<ItemSummary>>,
    stored_picked: usize,
    stall: Option<Stall>,
    /// The item a price is being named for.
    selling: Option<i64>,
    /// The price being named: gold, silver.
    price: [String; 2],
    /// What the hub has been asked and has not answered, each with its number.
    asks: Vec<(u32, Ask, Pending<Answer>)>,
    serial: u32,
    /// The number of the newest answer each list was filled from: an older answer that
    /// arrives later is not taken.
    filled: [u32; 3],
    /// A list could not be asked for again after something changed: what it shows may be
    /// old, and nothing acts on it.
    old: [bool; 3],
    notice: String,
    /// The notice is a refusal.
    bad: bool,
    buying: Asked,
    wearing: Asked,
    now: Instant,
    /// The item bar (LOOK.md 3.2, ITEMS.md 4): the template on each cell; `None` until
    /// the hub has said. Shown as a row of slots under the grid; a stack dragged onto one
    /// sets it, a cell dragged into the grid empties it, two cells swap.
    bar: Option<Vec<Option<String>>>,
    bar_filled: u32,
}

/// Silver as a person reads it (ECONOMY.md 2): its units, the largest first, each in its
/// own colour, and never a long row of digits.
pub fn coin_parts(silver: i64) -> Vec<(String, [f32; 4])> {
    let silver = silver.max(0);
    let (gold, rest) = (silver / SILVER_PER_GOLD, silver % SILVER_PER_GOLD);
    let mut parts = Vec::new();
    if gold > 0 {
        // Thousands of gold in threes: a digit more or less is seen.
        let digits = gold.to_string();
        let mut grouped = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(c);
        }
        parts.push((format!("{grouped} g"), GOLD));
    }
    if rest > 0 || parts.is_empty() {
        parts.push((format!("{rest} s"), SILVER));
    }
    parts
}

#[cfg(test)]
fn coin_words(silver: i64) -> String {
    let parts: Vec<String> = coin_parts(silver).into_iter().map(|(t, _)| t).collect();
    parts.join(" ")
}

/// A price for a row with room for `room` characters: whole when it fits; otherwise its
/// largest units and a `+` that says there is more (the line under the list has all of it,
/// and that is the price a purchase is made at). Never a number cut in the middle.
pub fn coin_row(silver: i64, room: usize) -> String {
    let mut parts: Vec<String> = coin_parts(silver).into_iter().map(|(t, _)| t).collect();
    let whole = parts.join(" ");
    if whole.chars().count() <= room {
        return whole;
    }
    while parts.len() > 1 {
        parts.pop();
        let short = format!("{}+", parts.join(" "));
        if short.chars().count() <= room {
            return short;
        }
    }
    format!("{}+", parts[0])
}

/// What two fields of gold and silver say, in silver (an empty field is none of that
/// unit); `None` when it is no amount the hub would take.
pub(crate) fn silver_of(fields: &[String; 2]) -> Option<i64> {
    let part = |text: &str| -> Option<i64> {
        if text.is_empty() {
            Some(0)
        } else {
            text.parse().ok()
        }
    };
    let total = part(&fields[0])?
        .checked_mul(SILVER_PER_GOLD)?
        .checked_add(part(&fields[1])?)?;
    (0..=MAX_PRICE).contains(&total).then_some(total)
}

/// The two fields a price is named in (gold, silver: digits only), across `r`.
pub(crate) fn coin_fields<C: Canvas>(
    ui: &mut Ui<'_, C>,
    r: Rect,
    gap: f32,
    fields: &mut [String; 2],
) {
    let w = (r.w - gap) / 2.0;
    for (i, (label, digits)) in [("gold", 10), ("silver", 2)].into_iter().enumerate() {
        let at = Rect::new(r.x + i as f32 * (w + gap), r.y, w, r.h);
        ui.text_field(at, label, &mut fields[i], Field::number(digits));
    }
}

/// A material without its layer, as a word: `shard/boss_scale` is `boss scale`.
fn material(id: &str) -> String {
    id.rsplit('/').next().unwrap_or(id).replace('_', " ")
}

/// What a row calls the item: its template, or for a part what it is made of.
pub(crate) fn name(item: &ItemSummary) -> String {
    match (item.place, item.components.first()) {
        // A stack says how many (MODES.md 11.1).
        _ if item.cap > 0 => format!("{} ×{}", item.template, item.quantity),
        (PLACE_NONE, Some((_, m))) => material(m),
        _ => item.template.clone(),
    }
}

/// The row's word for it: the strongest thing it does, or what it is.
pub(crate) fn headline(item: &ItemSummary) -> &str {
    item.does.first().unwrap_or(&item.what)
}

fn made_of(item: &ItemSummary) -> String {
    let parts: Vec<String> = item.components.iter().map(|(_, m)| material(m)).collect();
    format!("of {}", parts.join(", "))
}

/// The same thing stays picked when a list is filled again; when it is not in the list
/// any more, nothing is.
pub(crate) fn keep<T>(list: &[T], picked: &mut usize, was: Option<i64>, id: impl Fn(&T) -> i64) {
    if let Some(was) = was {
        *picked = list.iter().position(|x| id(x) == was).unwrap_or(NONE);
    }
}

impl Bag {
    fn new(page: Page, session: SessionId, character: CharacterId, now: Instant) -> Bag {
        Bag {
            page,
            session,
            character,
            coin: None,
            items: None,
            picked: 0,
            stored: None,
            stored_picked: 0,
            stall: None,
            selling: None,
            price: Default::default(),
            asks: Vec::new(),
            serial: 0,
            filled: [0; 3],
            old: [false; 3],
            notice: String::new(),
            bad: false,
            buying: Asked::default(),
            wearing: Asked::default(),
            now,
            bar: None,
            bar_filled: 0,
        }
    }

    /// The item bar as this screen last heard it from the hub (LOOK.md 3.2), for the HUD.
    pub fn bar(&self) -> Option<&[Option<String>]> {
        self.bar.as_deref()
    }

    /// The inventory of the character being played.
    pub fn inventory(
        hub: &dyn HubApi,
        session: SessionId,
        character: CharacterId,
        now: Instant,
    ) -> Bag {
        let mut bag = Bag::new(Page::Inventory, session, character, now);
        bag.read(hub, List::Inventory);
        bag.ask(hub, Ask::Bar, PlayerEcon::Bar);
        bag
    }

    /// The stall the body stands at.
    pub fn stall(
        hub: &dyn HubApi,
        session: SessionId,
        character: CharacterId,
        stall: i64,
        owner: &str,
        now: Instant,
    ) -> Bag {
        let mut bag = Bag::new(Page::Stall, session, character, now);
        bag.stall = Some(Stall {
            id: stall,
            owner: owner.to_string(),
            mine: false,
            listings: None,
            picked: 0,
        });
        bag.read(hub, List::Stall);
        bag.read(hub, List::Inventory);
        bag
    }

    fn ask(&mut self, hub: &dyn HubApi, what: Ask, op: PlayerEcon) {
        let req = PlayerRequest::Econ {
            session: self.session,
            character: self.character,
            op,
        };
        self.serial += 1;
        self.asks.push((self.serial, what, hub.call(req)));
    }

    /// Ask for a list again.
    fn read(&mut self, hub: &dyn HubApi, list: List) {
        let op = match (list, &self.stall) {
            (List::Inventory, _) => PlayerEcon::Inventory,
            (List::Storage, _) => PlayerEcon::Storage,
            (List::Stall, Some(stall)) => PlayerEcon::StallView { stall: stall.id },
            (List::Stall, None) => return,
        };
        self.ask(hub, Ask::Read(list), op);
    }

    /// Everything this bag shows, asked again: after something the person did.
    fn refresh(&mut self, hub: &dyn HubApi) {
        self.read(hub, List::Stall);
        self.read(hub, List::Inventory);
        if self.page == Page::Inventory {
            self.ask(hub, Ask::Bar, PlayerEcon::Bar);
        }
        if self.stored.is_some() || self.page == Page::Storage {
            self.read(hub, List::Storage);
        }
    }

    fn say(&mut self, text: impl Into<String>, bad: bool) {
        self.notice = text.into();
        self.bad = bad;
    }

    /// The zone answered a buy of `listing`. `Some`: this screen was not waiting for it
    /// (it is the answer to an earlier request), and here is what to say of it where the
    /// game says such things; the lists are left as they are.
    pub fn bought(
        &mut self,
        hub: &dyn HubApi,
        listing: i64,
        result: Result<(), String>,
    ) -> Option<String> {
        if !self.buying.answered(listing) {
            return Some(match result {
                Ok(()) => "an earlier purchase went through".to_string(),
                Err(why) => format!("an earlier purchase did not: {why}"),
            });
        }
        match result {
            Ok(()) => self.say("bought: it is in the inventory", false),
            Err(why) => self.say(why, true),
        }
        self.refresh(hub);
        None
    }

    /// The zone answered a wear or a take-off of `item`; as `bought`.
    pub fn worn(
        &mut self,
        hub: &dyn HubApi,
        item: i64,
        result: Result<(), String>,
    ) -> Option<String> {
        if !self.wearing.answered(item) {
            return Some(match result {
                Ok(()) => "what is worn changed".to_string(),
                Err(why) => format!("what is worn did not change: {why}"),
            });
        }
        match result {
            Ok(()) => self.say("done", false),
            Err(why) => self.say(why, true),
        }
        self.refresh(hub);
        None
    }

    /// Take the answers that have come.
    fn answers(&mut self, hub: &dyn HubApi) {
        let mut i = 0;
        while i < self.asks.len() {
            let Some(answer) = self.asks[i].2.take() else {
                i += 1;
                continue;
            };
            let (serial, what, _) = self.asks.remove(i);
            match (what, answer) {
                (Ask::Read(list), Ok(PlayerResponse::Econ(reply))) => {
                    // An answer older than the one a list was filled from says what was,
                    // not what is.
                    if serial < self.filled[list as usize] {
                        continue;
                    }
                    if self.fill(list, reply) {
                        self.filled[list as usize] = serial;
                        self.old[list as usize] = false;
                    } else {
                        self.say("the hub answered something else", true);
                        self.old[list as usize] = true;
                    }
                }
                (Ask::Read(List::Stall), Err(RpcError::Refused(HubError::NotFound))) => {
                    if let Some(stall) = &mut self.stall {
                        stall.listings = Some(Vec::new());
                        stall.picked = NONE;
                    }
                    self.filled[List::Stall as usize] = serial;
                    self.say("this stall has closed", true);
                }
                (Ask::Read(list), Err(e)) => {
                    // What the screen shows of this list may be old now. The word of
                    // what was done stays; a session that ended is said, since nothing
                    // will work until it is mended.
                    self.old[list as usize] = true;
                    if matches!(e, RpcError::Refused(HubError::Unauthorized)) {
                        self.say("the session has ended: leave and log in again", true);
                    } else if self.notice.is_empty() {
                        self.say(Self::words(&e), true);
                    }
                }
                (Ask::Read(list), Ok(_)) => self.old[list as usize] = true,
                (Ask::Bar, Ok(PlayerResponse::Econ(PlayerEconReply::Bar(cells)))) => {
                    if serial >= self.bar_filled {
                        self.bar_filled = serial;
                        self.bar = Some(cells);
                    }
                }
                (Ask::Bar, Ok(_)) => self.say("the hub answered something else", true),
                (Ask::Bar, Err(e)) => {
                    if self.notice.is_empty() {
                        self.say(Self::words(&e), true);
                    }
                }
                (
                    Ask::Change(done),
                    Ok(PlayerResponse::Econ(PlayerEconReply::Done | PlayerEconReply::Id(_))),
                ) => {
                    self.say(done, false);
                    if self.page == Page::Price {
                        self.page = Page::Inventory;
                        self.selling = None;
                    }
                    self.refresh(hub);
                }
                (
                    Ask::Change(_),
                    Err(RpcError::Refused(HubError::NotFound | HubError::Unauthorized)),
                ) => {
                    // What was acted on is not where this screen had it (it was moved, or
                    // sold, from somewhere else): the lists say what is.
                    let why = match self.page {
                        Page::Price => "you have no open stall here to sell at",
                        _ => "it is not there any more",
                    };
                    self.say(why, true);
                    self.refresh(hub);
                }
                (Ask::Change(_), Ok(_)) => self.say("the hub answered something else", true),
                (Ask::Change(_), Err(e)) => self.say(Self::words(&e), true),
            }
        }
        // The item a price was being named for is gone: there is nothing to name one for.
        if self.page == Page::Price && self.selling_item().is_none() && self.items.is_some() {
            self.page = Page::Inventory;
            self.selling = None;
            self.say("it is not there any more", true);
        }
    }

    /// Put an answer into its list; `false` when it is not an answer to that question.
    fn fill(&mut self, list: List, reply: PlayerEconReply) -> bool {
        match (list, reply) {
            (List::Inventory, PlayerEconReply::Holder { coin, items }) => {
                let was = self
                    .items
                    .as_ref()
                    .and_then(|l| l.get(self.picked))
                    .map(|i| i.id);
                keep(&items, &mut self.picked, was, |i| i.id);
                self.coin = Some(coin);
                self.items = Some(items);
            }
            (List::Storage, PlayerEconReply::Holder { items, .. }) => {
                let was = self
                    .stored
                    .as_ref()
                    .and_then(|l| l.get(self.stored_picked))
                    .map(|i| i.id);
                keep(&items, &mut self.stored_picked, was, |i| i.id);
                self.stored = Some(items);
            }
            (
                List::Stall,
                PlayerEconReply::Listings {
                    owner,
                    mine,
                    listings,
                },
            ) => {
                let Some(stall) = &mut self.stall else {
                    return true;
                };
                let was = stall
                    .listings
                    .as_ref()
                    .and_then(|l| l.get(stall.picked))
                    .map(|l| l.id);
                keep(&listings, &mut stall.picked, was, |l| l.id);
                stall.owner = owner;
                stall.mine = mine;
                stall.listings = Some(listings);
            }
            _ => return false,
        }
        true
    }

    /// A refusal in a person's words.
    fn words(e: &RpcError) -> String {
        match e {
            RpcError::Refused(HubError::Invalid(why)) => why.clone(),
            RpcError::Refused(HubError::Full) => "there is no room for it".to_string(),
            RpcError::Refused(HubError::Busy) => "the hub is busy: try again".to_string(),
            RpcError::Refused(HubError::Unauthorized) => {
                "the session has ended: leave and log in again".to_string()
            }
            other => other.to_string(),
        }
    }

    fn selling_item(&self) -> Option<&ItemSummary> {
        let id = self.selling?;
        self.items.as_ref()?.iter().find(|i| i.id == id)
    }

    /// One frame. `ui` was begun for `self.page.name()`. `keeps_stall`: the character has
    /// an open stall in this zone, so there is somewhere to sell. `now` is the frame's
    /// time.
    #[allow(dead_code)]
    pub fn frame<C: Canvas>(
        &mut self,
        ui: &mut Ui<'_, C>,
        hub: &dyn HubApi,
        keeps_stall: bool,
        now: Instant,
    ) -> BagAction {
        self.frame_with(ui, hub, keeps_stall, now, ItemLooks::default())
    }

    /// The same, with the bundle's pictures.
    pub fn frame_with<C: Canvas>(
        &mut self,
        ui: &mut Ui<'_, C>,
        hub: &dyn HubApi,
        keeps_stall: bool,
        now: Instant,
        looks: ItemLooks<'_>,
    ) -> BagAction {
        self.now = now;
        // The zone takes one request of a kind in a second and may drop what comes
        // sooner; a screen does not wait for ever for an answer that is not coming.
        if self.buying.gave_up(now) {
            self.say("the keeper did not answer: try again", true);
        }
        if self.wearing.gave_up(now) {
            self.say("the zone did not answer: try again", true);
        }
        let action = match self.page {
            Page::Inventory => self.inventory_page(ui, hub, keeps_stall, looks),
            Page::Storage => self.storage_page(ui, hub, looks),
            Page::Price => self.price_page(ui, hub),
            Page::Stall => self.stall_page(ui, hub, looks),
        };
        // Answers are taken when the frame has been drawn: a page changes between two
        // frames, never inside one.
        self.answers(hub);
        action
    }

    /// Nothing is being waited for and every list shown is the hub's latest word: a
    /// button may act on what is on the screen.
    fn ready(&self) -> bool {
        self.asks.is_empty()
            && self.buying.waiting.is_none()
            && self.wearing.waiting.is_none()
            && !self.old.contains(&true)
    }

    /// The two lines under the lists: what is being waited for or the last thing said,
    /// and that a list may be old.
    fn notice_lines<C: Canvas>(&self, ui: &mut Ui<'_, C>, r: Rect) {
        let (text, color) = if self.buying.waiting.is_some() {
            ("asking the keeper", ui::FAINT)
        } else if self.wearing.waiting.is_some() {
            ("asking the zone", ui::FAINT)
        } else if !self.asks.is_empty() {
            ("asking the hub", ui::FAINT)
        } else if self.bad {
            (self.notice.as_str(), ui::WARN)
        } else {
            (self.notice.as_str(), ui::TEXT)
        };
        let mut said = text.to_string();
        if self.old.contains(&true) && self.asks.is_empty() {
            if !said.is_empty() {
                said.push_str(". ");
            }
            said.push_str("This may be old: close it and look again");
        }
        if !said.is_empty() {
            ui.paragraph(r, color, &said);
        }
    }

    /// `label`, then an amount of coin in its colours (nothing while it is not known).
    fn purse<C: Canvas>(ui: &mut Ui<'_, C>, r: Rect, label: &str, coin: Option<i64>) {
        ui.label(r.x, r.y, 0.0, ui::FAINT, label);
        if let Some(coin) = coin {
            let x = r.x + ui.text_width(label) + 2.0 * ADVANCE * ui.scale;
            ui.coins(x, r.y, &coin_parts(coin));
        }
    }

    /// What is picked, in full (five lines): what it is, what it does, what is worn in
    /// its place now, and what it is made of. `worn` is the inventory, when the hub has
    /// said it.
    fn detail<C: Canvas>(
        ui: &mut Ui<'_, C>,
        r: Rect,
        item: Option<&ItemSummary>,
        worn: Option<&[ItemSummary]>,
        nothing: &str,
    ) {
        let line = ui.line();
        let Some(item) = item else {
            ui.label(r.x, r.y, r.w, ui::FAINT, nothing);
            return;
        };
        ui.label(r.x, r.y, r.w, ui::TEXT, &item.what);
        ui.label(r.x, r.y + line, r.w, ui::TEXT, &item.does.join("  "));
        // Beside what is worn in its place (PLAN.md 6: a thing is judged against what
        // one has), once the hub has said what that is.
        let mut y = r.y + 2.0 * line;
        if let (true, Some(worn)) = (item.place != PLACE_NONE && !item.worn, worn) {
            let now = worn
                .iter()
                .find(|w| w.worn && w.place == item.place && w.id != item.id);
            let said = match now {
                Some(w) => format!("you wear: {}", w.does.join("  ")),
                None => "you wear nothing in its place".to_string(),
            };
            ui.label(r.x, y, r.w, ui::FAINT, &said);
            y += line;
        }
        let rest = Rect::new(r.x, y, r.w, r.y + r.h - y);
        ui.paragraph(rest, ui::FAINT, &made_of(item));
    }

    /// What to say where a list's picked thing would be shown, when nothing is.
    fn nothing(known: bool, empty: bool, none: &'static str) -> &'static str {
        match (known, empty) {
            (false, _) => "",
            (true, true) => none,
            (true, false) => "pick one to see it",
        }
    }

    /// The inventory (LOOK.md 4): the 24 slots as a grid, the equip panel beside it (a
    /// paperdoll with the weapon and armour slots), the picked thing in full under them,
    /// and the buttons. A drag onto a slot wears; a drag of a worn thing into the grid
    /// takes it off; the buttons do the same (ITEMS.md 6 and LOOK.md 1.3).
    fn inventory_page<C: Canvas>(
        &mut self,
        ui: &mut Ui<'_, C>,
        hub: &dyn HubApi,
        keeps_stall: bool,
        looks: ItemLooks<'_>,
    ) -> BagAction {
        let s = ui.scale;
        let gap = 5.0 * s;
        let line = ui.line();
        let h = ui.button_height();
        let side = ui.slot_side();
        let cols = 6usize;
        let grid_h = 4.0 * side + 3.0 * 2.0 * s;
        let bar_h = side + line;
        let inner =
            line + gap + grid_h + gap + bar_h + gap + 3.0 * line + gap + 2.0 * line + gap + h;
        let panel = Rect::centred(ui.size(), PANEL_UNITS * s, ui.panel_height(inner, true));
        let inner = ui.panel(panel, "inventory");
        let mut col = Column::new(inner, gap);
        Self::purse(ui, col.take(line), "coin", self.coin);
        let items: Vec<ItemSummary> = self.items.clone().unwrap_or_default();
        let things: Vec<SlotThing> = items.iter().map(|i| looks.thing(i)).collect();
        let area = col.take(grid_h);
        let grid_w = cols as f32 * side + (cols - 1) as f32 * 2.0 * s;
        let grid = Rect::new(area.x, area.y, grid_w, area.h);
        let mut sel = items.get(self.picked).map(|i| i.id);
        let worn_list = self.items.clone();
        let tip = |t: &SlotThing| {
            items
                .iter()
                .find(|i| i.id == t.id)
                .map(|i| tooltip_lines(i, worn_list.as_deref()))
                .unwrap_or_default()
        };
        ui.grid(
            grid,
            "items",
            cols,
            INVENTORY_SLOTS,
            &things,
            &mut sel,
            0.0,
            &tip,
        );
        // (An empty or unknown list leaves the pick as it was: the first thing is picked
        // when the list arrives, as a list's first row was.)
        if !items.is_empty() {
            self.picked = sel
                .and_then(|id| items.iter().position(|i| i.id == id))
                .unwrap_or(NONE);
        }
        // The equip panel: the paperdoll between the two places (LOOK.md 4).
        let ex = grid.x + grid_w + gap;
        let ew = (area.x + area.w - ex).max(0.0);
        let slot_w = side;
        let doll = Rect::new(ex, area.y, (ew - slot_w - gap).max(0.0), area.h);
        if doll.w >= 20.0 * s {
            ui.paperdoll(doll);
        }
        let weapon = items.iter().find(|i| i.worn && i.place == PLACE_WEAPON);
        let armour = items.iter().find(|i| i.worn && i.place == PLACE_ARMOUR);
        let sx = area.x + area.w - slot_w;
        let weapon_slot = Rect::new(sx, area.y, slot_w, side);
        let armour_slot = Rect::new(sx, area.y + side + gap, slot_w, side);
        let wt = weapon.map(|i| looks.thing(i));
        let at = armour.map(|i| looks.thing(i));
        let dropped_on_weapon = ui.drop_slot(weapon_slot, "weapon", wt.as_ref(), &tip);
        let dropped_on_armour = ui.drop_slot(armour_slot, "armour", at.as_ref(), &tip);
        let dropped_in_grid = ui.dropped("items");
        // The item bar (LOOK.md 3.2, ITEMS.md 4): its four cells as slots under the grid,
        // each with the stack set on it (dim with none carried) and its key beneath; a
        // stack dragged from the grid onto one sets it, a cell dragged into the grid
        // empties it, a cell dragged onto another swaps them.
        let bar_area = col.take(bar_h);
        let mut cells: Vec<Option<String>> = self.bar.clone().unwrap_or_default();
        cells.resize(gm_core::sim::BAR_CELLS, None);
        ui.small(
            bar_area.x + 14.0 * s,
            bar_area.y + 2.0 * s,
            ui::FAINT,
            "bar",
        );
        let mut dropped_on_bar: Vec<Option<(String, i64)>> = Vec::new();
        let bar_things: Vec<Option<SlotThing>> = cells
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let template = t.as_deref()?;
                let carried = items
                    .iter()
                    .find(|it| it.template == template && it.cap > 0);
                let mut thing = match carried {
                    Some(it) => looks.thing(it),
                    None => SlotThing {
                        id: -(i as i64) - 1,
                        name: template.to_string(),
                        said: template.to_string(),
                        off: true,
                        ..Default::default()
                    },
                };
                thing.count = Some(carried.map_or(0, |it| it.quantity));
                thing.mark = None;
                Some(thing)
            })
            .collect();
        for (i, key) in crate::app::BAR_KEYS.iter().enumerate() {
            let r = Rect::new(
                bar_area.x + 22.0 * s + i as f32 * (side + gap),
                bar_area.y,
                side,
                side,
            );
            let name = format!("bar {key}");
            dropped_on_bar.push(ui.drop_slot(r, &name, bar_things[i].as_ref(), &tip));
            ui.small(r.x + r.w, r.y + r.h + 2.0 * s, ui::FAINT, key);
        }
        let mut arranged = cells.clone();
        let mut changed = false;
        for (i, dropped) in dropped_on_bar.iter().enumerate() {
            let Some((from, id)) = dropped else { continue };
            if from == "items" {
                if let Some(it) = items.iter().find(|it| it.id == *id && it.cap > 0) {
                    // A kind sits on one cell: dragged onto another it moves there.
                    for cell in arranged.iter_mut() {
                        if cell.as_deref() == Some(it.template.as_str()) {
                            *cell = None;
                        }
                    }
                    arranged[i] = Some(it.template.clone());
                    changed = true;
                }
            } else if let Some(k) = crate::app::BAR_KEYS
                .iter()
                .position(|key| *from == format!("bar {key}"))
                && k != i
            {
                arranged.swap(i, k);
                changed = true;
            }
        }
        if let Some((from, _)) = &dropped_in_grid
            && let Some(k) = crate::app::BAR_KEYS
                .iter()
                .position(|key| *from == format!("bar {key}"))
        {
            arranged[k] = None;
            changed = true;
        }
        if changed && self.ready() {
            self.bar = Some(arranged.clone());
            self.ask(
                hub,
                Ask::Change("the bar is set"),
                PlayerEcon::SetBar { cells: arranged },
            );
            self.notice.clear();
        }
        let picked = items.get(self.picked).cloned();
        let nothing = Self::nothing(self.items.is_some(), items.is_empty(), "nothing is carried");
        Self::detail(
            ui,
            col.take(3.0 * line),
            picked.as_ref(),
            self.items.as_deref(),
            nothing,
        );
        self.notice_lines(ui, col.take(2.0 * line));
        let ready = self.ready();
        let worn = picked.as_ref().is_some_and(|i| i.worn);
        // What is worn comes off whatever it is; what is not must have a place to go and
        // fit this build's hands (ITEMS.md 2: the hub said so, and its words are in the
        // item's lines above).
        let wearable = picked
            .as_ref()
            .is_some_and(|i| i.worn || (i.place != PLACE_NONE && i.fits));
        let wear = if worn { "Take off" } else { "Wear" };
        let row = ui.buttons(col.take(h), &[wear, "Sell", "Store", "Storage", "Close"]);
        let may_change = ready && self.wearing.free(self.now);
        let change = ui.button_if(row[0], wear, may_change && wearable);
        // What is worn is not for sale, and there must be a stall to sell at.
        let loose = picked.is_some() && !worn;
        if ui.button_if(row[1], "Sell", ready && loose && keeps_stall)
            && let Some(item) = &picked
        {
            self.page = Page::Price;
            self.selling = Some(item.id);
            self.price = Default::default();
            self.notice.clear();
            ui.focus("field", "gold");
        }
        if ui.button_if(row[2], "Store", ready && loose)
            && let Some(item) = &picked
        {
            let op = PlayerEcon::StorageDeposit { item: item.id };
            self.ask(hub, Ask::Change("it is in the storage"), op);
            self.notice.clear();
        }
        if ui.button(row[3], "Storage") {
            self.page = Page::Storage;
            self.notice.clear();
            self.read(hub, List::Storage);
        }
        if ui.button(row[4], "Close") || ui.key(Key::Escape) {
            return BagAction::Close;
        }
        // Drops do what the buttons do, through the same gate: an unworn thing onto its
        // place wears it; a worn thing dragged into the grid comes off.
        // A thing dropped on a slot must be for that slot (a cuirass on the weapon slot is
        // nothing; the zone would refuse it, and the screen does not ask).
        let for_slot = |dropped: Option<(String, i64)>, place: u8| {
            dropped
                .filter(|(from, _)| from == "items")
                .map(|(_, id)| id)
                .and_then(|id| items.iter().find(|i| i.id == id))
                .filter(|i| !i.worn && i.place == place && i.fits)
                .cloned()
        };
        let dropped_to_wear =
            for_slot(dropped_on_weapon, PLACE_WEAPON).or(for_slot(dropped_on_armour, PLACE_ARMOUR));
        let dropped_to_take_off = dropped_in_grid
            .filter(|(from, _)| from == "weapon" || from == "armour")
            .map(|(_, id)| id)
            .and_then(|id| items.iter().find(|i| i.id == id))
            .filter(|i| i.worn)
            .cloned();
        if may_change && let Some(item) = dropped_to_wear {
            self.wearing.begin(item.id, self.now);
            self.notice.clear();
            return BagAction::Wear { item: item.id };
        }
        if may_change && let Some(item) = dropped_to_take_off {
            self.wearing.begin(item.id, self.now);
            self.notice.clear();
            return BagAction::TakeOff { item: item.id };
        }
        match (change, picked) {
            (true, Some(item)) => {
                self.wearing.begin(item.id, self.now);
                self.notice.clear();
                if worn {
                    BagAction::TakeOff { item: item.id }
                } else {
                    BagAction::Wear { item: item.id }
                }
            }
            _ => BagAction::None,
        }
    }

    /// The account's storage: where what a closed stall could not hand back is, and
    /// where a full inventory is emptied into.
    fn storage_page<C: Canvas>(
        &mut self,
        ui: &mut Ui<'_, C>,
        hub: &dyn HubApi,
        looks: ItemLooks<'_>,
    ) -> BagAction {
        let s = ui.scale;
        let gap = 5.0 * s;
        let line = ui.line();
        let h = ui.button_height();
        let side = ui.slot_side();
        let cols = 8usize;
        let grid_h = 4.0 * side + 3.0 * 2.0 * s;
        let inner = grid_h + gap + 3.0 * line + gap + 2.0 * line + gap + h;
        let panel = Rect::centred(ui.size(), PANEL_UNITS * s, ui.panel_height(inner, true));
        let inner = ui.panel(panel, "storage");
        let mut col = Column::new(inner, gap);
        let stored: Vec<ItemSummary> = self.stored.clone().unwrap_or_default();
        let things: Vec<SlotThing> = stored.iter().map(|i| looks.thing(i)).collect();
        let area = col.take(grid_h);
        let mut sel = stored.get(self.stored_picked).map(|i| i.id);
        let worn_list = self.items.clone();
        let tip = |t: &SlotThing| {
            stored
                .iter()
                .find(|i| i.id == t.id)
                .map(|i| tooltip_lines(i, worn_list.as_deref()))
                .unwrap_or_default()
        };
        ui.grid(
            area,
            "stored",
            cols,
            STORAGE_SLOTS,
            &things,
            &mut sel,
            0.0,
            &tip,
        );
        if !stored.is_empty() {
            self.stored_picked = sel
                .and_then(|id| stored.iter().position(|i| i.id == id))
                .unwrap_or(NONE);
        }
        let picked = stored.get(self.stored_picked).cloned();
        let nothing = Self::nothing(
            self.stored.is_some(),
            stored.is_empty(),
            "nothing is stored",
        );
        Self::detail(
            ui,
            col.take(3.0 * line),
            picked.as_ref(),
            self.items.as_deref(),
            nothing,
        );
        self.notice_lines(ui, col.take(2.0 * line));
        let may = self.ready() && picked.is_some();
        let row = ui.buttons(col.take(h), &["Take", "Back"]);
        let take = ui.button_if(row[0], "Take", may);
        if ui.button(row[1], "Back") || ui.key(Key::Escape) {
            self.page = Page::Inventory;
            self.notice.clear();
            return BagAction::None;
        }
        if let (true, Some(item)) = (take, picked) {
            let op = PlayerEcon::StorageWithdraw { item: item.id };
            self.ask(hub, Ask::Change("it is in the inventory"), op);
            self.notice.clear();
        }
        BagAction::None
    }

    /// The price named so far, in silver; `None` while it is not one.
    fn price(&self) -> Option<i64> {
        silver_of(&self.price).filter(|total| *total > 0)
    }

    fn price_page<C: Canvas>(&mut self, ui: &mut Ui<'_, C>, hub: &dyn HubApi) -> BagAction {
        let item = self.selling_item().cloned();
        let s = ui.scale;
        let gap = 6.0 * s;
        let line = ui.line();
        let h = ui.button_height();
        // (Three lines for what is said: this panel is the narrow one.)
        let inner = line + gap + ui.field_height() + gap + line + gap + 3.0 * line + gap + h;
        let panel = Rect::centred(ui.size(), 250.0 * s, ui.panel_height(inner, true));
        let inner = ui.panel(panel, "price");
        let mut col = Column::new(inner, gap);
        let what = col.take(line);
        if let Some(item) = &item {
            let said = format!("{}  {}", name(item), headline(item));
            ui.label(what.x, what.y, what.w, ui::TEXT, &said);
        }
        // Gold and silver apart, and the whole said back in words before it is sent: a
        // price is never a row of digits to miscount.
        let fields = col.take(ui.field_height());
        ui.focus_default("field", "gold");
        coin_fields(ui, fields, gap, &mut self.price);
        let price = self.price();
        let shown = col.take(line);
        match price {
            Some(silver) => Self::purse(ui, shown, "for", Some(silver)),
            None => ui.label(shown.x, shown.y, shown.w, ui::FAINT, "name a price"),
        }
        self.notice_lines(ui, col.take(3.0 * line));
        let may = self.ready() && price.is_some() && item.is_some();
        let row = ui.buttons(col.take(h), &["List it", "Back"]);
        let mut list = ui.button_if(row[0], "List it", may);
        if ui.button(row[1], "Back") || ui.key(Key::Escape) {
            self.page = Page::Inventory;
            self.selling = None;
            self.notice.clear();
            return BagAction::None;
        }
        list |= may && ui.key(Key::Enter);
        if let (true, Some(price), Some(item)) = (list, price, item) {
            let op = PlayerEcon::StallList {
                item: item.id,
                price,
            };
            self.ask(hub, Ask::Change("it is in your stall"), op);
            self.notice.clear();
        }
        BagAction::None
    }

    fn stall_page<C: Canvas>(
        &mut self,
        ui: &mut Ui<'_, C>,
        hub: &dyn HubApi,
        looks: ItemLooks<'_>,
    ) -> BagAction {
        let ready = self.ready();
        let free = self.buying.free(self.now);
        let worn_list = self.items.clone();
        let Some(stall) = &mut self.stall else {
            return BagAction::Close;
        };
        let s = ui.scale;
        let gap = 5.0 * s;
        let line = ui.line();
        let h = ui.button_height();
        let side = ui.slot_side();
        let cols = 6usize;
        let under = line;
        let grid_h = 2.0 * (side + under) + 2.0 * s;
        let inner =
            line + gap + grid_h + gap + 3.0 * line + gap + line + gap + 2.0 * line + gap + h;
        let panel = Rect::centred(ui.size(), PANEL_UNITS * s, ui.panel_height(inner, true));
        let title = if stall.mine {
            "your stall".to_string()
        } else {
            format!("{}'s stall", stall.owner)
        };
        let inner = ui.panel(panel, &title);
        let mut col = Column::new(inner, gap);
        Self::purse(ui, col.take(line), "you have", self.coin);
        let known = stall.listings.is_some();
        let listings = stall.listings.clone().unwrap_or_default();
        let things: Vec<SlotThing> = listings
            .iter()
            .map(|l| SlotThing {
                // Picked by the listing's id, not the item's (ITEMS.md 6).
                id: l.id,
                said: format!(
                    "{}  {}  {}",
                    name(&l.item),
                    headline(&l.item),
                    coin_row(l.price, PRICE_CHARS)
                ),
                under: coin_row(l.price, 9),
                under_colour: ui::FOCUS,
                fixed: true,
                ..looks.thing(&l.item)
            })
            .collect();
        let area = col.take(grid_h);
        let mut sel = listings.get(stall.picked).map(|l| l.id);
        let tip = |t: &SlotThing| {
            listings
                .iter()
                .find(|l| l.id == t.id)
                .map(|l| {
                    let mut lines = tooltip_lines(&l.item, worn_list.as_deref());
                    lines.push((format!("price: {}", coin_row(l.price, 40)), ui::FOCUS));
                    lines
                })
                .unwrap_or_default()
        };
        ui.grid(
            area,
            "for sale",
            cols,
            STALL_SLOTS,
            &things,
            &mut sel,
            under,
            &tip,
        );
        if !listings.is_empty() {
            stall.picked = sel
                .and_then(|id| listings.iter().position(|l| l.id == id))
                .unwrap_or(NONE);
        }
        let picked = listings.get(stall.picked).cloned();
        let nothing = Self::nothing(known, listings.is_empty(), "nothing is for sale here");
        Self::detail(
            ui,
            col.take(3.0 * line),
            picked.as_ref().map(|l| &l.item),
            self.items.as_deref(),
            nothing,
        );
        let priced = col.take(line);
        if let Some(l) = &picked {
            Self::purse(ui, priced, "price", Some(l.price));
        }
        let (id, mine) = (stall.id, stall.mine);
        self.notice_lines(ui, col.take(2.0 * line));
        // The keeper takes a thing back; anybody else buys it.
        let act = if mine { "Take back" } else { "Buy" };
        let may = picked.is_some() && ready && (mine || free);
        let row = ui.buttons(col.take(h), &[act, "Close"]);
        let pressed = ui.button_if(row[0], act, may);
        if ui.button(row[1], "Close") || ui.key(Key::Escape) {
            return BagAction::Close;
        }
        match (pressed, picked) {
            (true, Some(l)) if mine => {
                let op = PlayerEcon::StallUnlist { listing: l.id };
                self.ask(hub, Ask::Change("it is back in the inventory"), op);
                self.notice.clear();
                BagAction::None
            }
            (true, Some(l)) => {
                self.buying.begin(l.id, self.now);
                self.notice.clear();
                BagAction::Buy {
                    stall: id,
                    listing: l.id,
                    price: l.price,
                }
            }
            _ => BagAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gm_hub_proto::protocol::PLACE_WEAPON;

    use super::*;
    use crate::ui::tests::{Recorder, SIZES, tidy};
    use crate::ui::{UiInput, UiState};

    const S: SessionId = SessionId([7; 16]);
    const ME: CharacterId = 41;

    /// The hub as these screens know it: a purse, an inventory, a storage and one stall,
    /// and everything that was asked of it.
    #[derive(Default)]
    struct Shop {
        coin: i64,
        items: Vec<ItemSummary>,
        stored: Vec<ItemSummary>,
        listings: Vec<ListingSummary>,
        mine: bool,
        closed: bool,
        /// Reads are refused with this from now on.
        deaf: Option<HubError>,
        /// Changes are refused with this from now on.
        refuse: Option<HubError>,
        /// Answers are not given at once but held, in the order asked.
        hold: bool,
        held: Vec<(crate::hub::Filler<Answer>, Answer)>,
        asked: Vec<PlayerEcon>,
        /// The item bar as the hub holds it (LOOK.md 3.2).
        bar: Vec<Option<String>>,
    }

    struct Hub(RefCell<Shop>);

    fn item(id: i64, template: &str, does: &[&str], parts: &[&str]) -> ItemSummary {
        let whole = parts.len() > 1;
        ItemSummary {
            id,
            template: if whole { template } else { "component" }.to_string(),
            components: parts
                .iter()
                .map(|m| (m.split('/').next().unwrap().to_string(), m.to_string()))
                .collect(),
            place: if whole { PLACE_WEAPON } else { PLACE_NONE },
            edge: [0; 9],
            worn: false,
            quantity: 1,
            cap: 0,
            fits: true,
            what: if whole {
                "a weapon, 250 of 250".to_string()
            } else {
                "a catalyst, for crafting".to_string()
            },
            does: does.iter().map(|d| d.to_string()).collect(),
        }
    }

    const BEST: [&str; 6] = [
        "shard/boss_scale",
        "core/dragonbone",
        "catalyst/thunderstone",
        "frame/whalebone",
        "gem/opal",
        "gem/opal",
    ];

    impl HubApi for Hub {
        fn call(&self, req: PlayerRequest) -> Pending<Answer> {
            let PlayerRequest::Econ {
                session: S,
                character: ME,
                op,
            } = req
            else {
                panic!("not what these screens ask: {req:?}");
            };
            let mut shop = self.0.borrow_mut();
            shop.asked.push(op.clone());
            let holder = |coin, items: &Vec<ItemSummary>| PlayerEconReply::Holder {
                coin,
                items: items.clone(),
            };
            let read = matches!(
                op,
                PlayerEcon::Inventory
                    | PlayerEcon::Bar
                    | PlayerEcon::Storage
                    | PlayerEcon::StallView { .. }
            );
            let refused = if read {
                shop.deaf.clone()
            } else {
                shop.refuse.clone()
            };
            let reply = match (refused, op) {
                (Some(e), _) => Err(e),
                (None, PlayerEcon::Inventory) => Ok(holder(shop.coin, &shop.items)),
                (None, PlayerEcon::Bar) => Ok(PlayerEconReply::Bar(shop.bar.clone())),
                (None, PlayerEcon::SetBar { cells }) => {
                    shop.bar = cells;
                    Ok(PlayerEconReply::Done)
                }
                (None, PlayerEcon::Storage) => Ok(holder(0, &shop.stored)),
                (None, PlayerEcon::StorageDeposit { item }) => {
                    match shop.items.iter().position(|i| i.id == item) {
                        Some(at) => {
                            let moved = shop.items.remove(at);
                            shop.stored.push(moved);
                            Ok(PlayerEconReply::Done)
                        }
                        None => Err(HubError::Unauthorized),
                    }
                }
                (None, PlayerEcon::StorageWithdraw { item }) => {
                    match shop.stored.iter().position(|i| i.id == item) {
                        Some(at) => {
                            let moved = shop.stored.remove(at);
                            shop.items.push(moved);
                            Ok(PlayerEconReply::Done)
                        }
                        None => Err(HubError::Unauthorized),
                    }
                }
                (None, PlayerEcon::StallView { .. }) if shop.closed => Err(HubError::NotFound),
                (None, PlayerEcon::StallView { .. }) => Ok(PlayerEconReply::Listings {
                    owner: "Smith".into(),
                    mine: shop.mine,
                    listings: shop.listings.clone(),
                }),
                (None, PlayerEcon::StallList { item, price }) => {
                    match shop.items.iter().position(|i| i.id == item) {
                        Some(at) => {
                            let moved = shop.items.remove(at);
                            shop.listings.push(ListingSummary {
                                id: 900 + item,
                                item: moved,
                                price,
                            });
                            Ok(PlayerEconReply::Id(900 + item))
                        }
                        None => Err(HubError::Unauthorized),
                    }
                }
                (None, PlayerEcon::StallUnlist { listing }) => {
                    match shop.listings.iter().position(|l| l.id == listing) {
                        Some(at) => {
                            let back = shop.listings.remove(at).item;
                            shop.items.push(back);
                            Ok(PlayerEconReply::Done)
                        }
                        None => Err(HubError::NotFound),
                    }
                }
                (None, other) => panic!("not what these screens ask: {other:?}"),
            };
            let answer = reply.map(PlayerResponse::Econ).map_err(RpcError::Refused);
            if shop.hold {
                let (pending, filler) = Pending::new();
                shop.held.push((filler, answer));
                pending
            } else {
                Pending::ready(answer)
            }
        }
    }

    fn shop() -> Hub {
        let mut worn = item(2, "sword", &["slash +2.0%"], &["core/iron", "frame/oak"]);
        worn.what = "a weapon, 40 of 250".into();
        Hub(RefCell::new(Shop {
            coin: 215,
            items: vec![
                item(1, "sword", &["slash +11.0%", "electric +9.5%"], &BEST),
                worn,
                item(3, "", &[], &["catalyst/ember"]),
            ],
            ..Shop::default()
        }))
    }

    /// A test's own clock and screen.
    struct Run {
        st: UiState,
        t: Instant,
        keeps: bool,
    }

    impl Run {
        fn new(keeps: bool) -> Run {
            Run {
                st: UiState::default(),
                t: Instant::now(),
                keeps,
            }
        }

        fn pass(&mut self, d: Duration) {
            self.t += d;
        }

        fn frame(&mut self, bag: &mut Bag, input: &UiInput, hub: &Hub) -> BagAction {
            let mut canvas = Recorder::new(1280.0, 720.0);
            let mut ui = Ui::begin(
                &mut canvas,
                &mut self.st,
                input,
                bag.page.name(),
                PANEL_UNITS,
            );
            let action = bag.frame(&mut ui, hub, self.keeps, self.t);
            ui.end();
            action
        }

        /// The screen as it stands once the answers that have come are on it: answers are
        /// taken after a frame is drawn, so the frame after shows them.
        fn look(&mut self, bag: &mut Bag, hub: &Hub) {
            let idle = UiInput::default();
            self.frame(bag, &idle, hub);
            self.frame(bag, &idle, hub);
        }

        fn click(&mut self, bag: &mut Bag, text: &str, hub: &Hub) -> BagAction {
            self.look(bag, hub);
            let at = self
                .st
                .find(text)
                .unwrap_or_else(|| panic!("nothing to click says {text:?}"))
                .rect
                .centre();
            let press = UiInput {
                cursor: at,
                pressed: true,
                down: true,
                ..Default::default()
            };
            self.frame(bag, &press, hub);
            let release = UiInput {
                cursor: at,
                released: true,
                ..Default::default()
            };
            self.frame(bag, &release, hub)
        }

        /// A drag from what says `from` to what says `onto`: a press, four moves, a
        /// release (LOOK.md 2.4).
        fn drag(&mut self, bag: &mut Bag, from: &str, onto: &str, hub: &Hub) -> BagAction {
            self.look(bag, hub);
            let a = self
                .st
                .find(from)
                .unwrap_or_else(|| panic!("nothing to drag says {from:?}"))
                .rect
                .centre();
            let b = self
                .st
                .find(onto)
                .unwrap_or_else(|| panic!("nothing to drop on says {onto:?}"))
                .rect
                .centre();
            let press = UiInput {
                cursor: a,
                last_cursor: a,
                pressed: true,
                down: true,
                ..Default::default()
            };
            self.frame(bag, &press, hub);
            let mut last = a;
            for i in 1..=4 {
                let t = i as f32 / 4.0;
                let at = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                let moved = UiInput {
                    cursor: at,
                    last_cursor: last,
                    down: true,
                    ..Default::default()
                };
                self.frame(bag, &moved, hub);
                last = at;
            }
            let release = UiInput {
                cursor: b,
                last_cursor: last,
                released: true,
                ..Default::default()
            };
            self.frame(bag, &release, hub)
        }

        fn typed(&mut self, bag: &mut Bag, text: &str, hub: &Hub) {
            let input = UiInput {
                text: text.into(),
                ..Default::default()
            };
            self.frame(bag, &input, hub);
        }

        fn shows(&self, text: &str) -> bool {
            self.st.shows(text)
        }

        fn offers(&self, text: &str) -> bool {
            self.st.find(text).is_some()
        }
    }

    #[test]
    fn coin_is_said_in_units_and_never_as_a_row_of_digits() {
        assert_eq!(coin_words(0), "0 s");
        assert_eq!(coin_words(-5), "0 s");
        assert_eq!(coin_words(40), "40 s");
        assert_eq!(coin_words(100), "1 g");
        assert_eq!(coin_words(215), "2 g 15 s");
        assert_eq!(coin_words(10_005), "100 g 5 s");
        // A price ten times another is a digit longer in gold, and the thousands stand apart.
        assert_eq!(coin_words(12_345_678), "123,456 g 78 s");
        assert_eq!(coin_words(MAX_PRICE), "100,000,000 g");
        let colours: Vec<[f32; 4]> = coin_parts(215).into_iter().map(|p| p.1).collect();
        assert_eq!(colours, [GOLD, SILVER]);
        // In a row: whole when it fits, else its largest units and a mark that there is
        // more; never a number cut in the middle, and never longer than the row has room.
        assert_eq!(coin_row(215, 14), "2 g 15 s");
        assert_eq!(coin_row(999_999, 14), "9,999 g 99 s");
        assert_eq!(coin_row(123_456_789, 14), "1,234,567 g+");
        assert_eq!(coin_row(MAX_PRICE - 1, 14), "99,999,999 g+");
        assert_eq!(coin_row(MAX_PRICE, 14), "100,000,000 g");
        for silver in [
            1,
            99,
            100,
            9_999,
            10_000,
            123_456,
            98_765_432,
            MAX_PRICE - 1,
            MAX_PRICE,
        ] {
            let row = coin_row(silver, PRICE_CHARS);
            assert!(row.chars().count() <= PRICE_CHARS, "{silver}: {row}");
            let whole = coin_words(silver);
            assert!(
                row == whole
                    || (row.ends_with('+') && whole.starts_with(row.trim_end_matches('+'))),
                "{silver}: {row} of {whole}"
            );
        }
    }

    /// A weapon the build's hands do not hold (ITEMS.md 2): the hub says so in the item's
    /// lines and the screen offers no Wear for it, nor takes it dropped on the weapon slot.
    #[test]
    fn a_weapon_the_build_does_not_hold_is_not_offered_to_wear() {
        let hub = shop();
        let mut staff = item(4, "staff", &["blunt +9.0%"], &["core/iron", "frame/oak"]);
        staff.fits = false;
        staff
            .does
            .push("this build's hands are for the sword: not a staff".into());
        hub.0.borrow_mut().items.push(staff);
        let mut run = Run::new(false);
        let mut bag = Bag::inventory(&hub, S, ME, run.t);
        run.look(&mut bag, &hub);
        assert!(run.offers("Wear"), "the sword is worn as before");
        run.click(&mut bag, "staff  blunt +9.0%", &hub);
        run.look(&mut bag, &hub);
        assert!(run.shows("this build's hands are for the sword: not a staff"));
        assert!(!run.offers("Wear") && run.offers("Store"));
    }

    #[test]
    fn the_inventory_shows_what_the_hub_says_and_wears_through_the_zone() {
        let hub = shop();
        let mut run = Run::new(false);
        let idle = UiInput::default();
        let mut bag = Bag::inventory(&hub, S, ME, run.t);
        // Before the hub's answer is taken nothing is said of what is carried: not that it
        // is nothing, and no button acts.
        run.frame(&mut bag, &idle, &hub);
        assert!(!run.shows("nothing is carried") && !run.offers("Wear"));
        run.look(&mut bag, &hub);
        // The purse in units, each row by the strongest thing the item does, and the one
        // picked in full: what it is, what it does, what is worn in its place (nothing
        // yet), what it is made of. All of it the hub's words.
        assert!(run.shows("2 g 15 s"));
        assert!(run.shows("sword  slash +11.0%") && run.shows("ember  a catalyst, for crafting"));
        assert!(run.shows("a weapon, 250 of 250") && run.shows("slash +11.0%  electric +9.5%"));
        assert!(run.shows("you wear nothing in its place"));
        assert!(run.shows("of boss scale, dragonbone, thunderstone, whalebone, opal, opal"));
        assert!(run.st.clipped.is_empty(), "{:?}", run.st.clipped);
        assert!(run.st.cut_cells.is_empty(), "{:?}", run.st.cut_cells);
        // Nothing to sell at without a stall here.
        assert!(!run.offers("Sell") && run.offers("Store"));
        // Enter is not a way to wear the first row (it is the chat's key a moment before).
        let enter = UiInput {
            keys: vec![Key::Enter],
            ..Default::default()
        };
        assert_eq!(run.frame(&mut bag, &enter, &hub), BagAction::None);

        // Wear: the zone is asked, not the hub; the screen waits and offers nothing
        // meanwhile; the zone's answer brings the inventory again.
        assert_eq!(
            run.click(&mut bag, "Wear", &hub),
            BagAction::Wear { item: 1 }
        );
        run.look(&mut bag, &hub);
        assert!(run.shows("asking the zone"));
        assert!(!run.offers("Wear") && !run.offers("Store"));
        // An answer about another item is not this screen's: it is said elsewhere, and
        // the screen goes on waiting.
        assert!(bag.worn(&hub, 2, Ok(())).is_some());
        run.look(&mut bag, &hub);
        assert!(run.shows("asking the zone"));
        hub.0.borrow_mut().items[0].worn = true;
        assert_eq!(bag.worn(&hub, 1, Ok(())), None);
        run.look(&mut bag, &hub);
        assert!(run.shows("sword  slash +11.0%  worn") && run.shows("done"));
        // One change a second: the button rests, then is there again. What is worn is
        // not for sale and not for the storage; it comes off.
        run.keeps = true;
        assert!(!run.offers("Take off"));
        run.pass(ZONE_GAP + Duration::from_millis(1));
        run.look(&mut bag, &hub);
        assert!(!run.offers("Sell") && !run.offers("Store"));
        assert_eq!(
            run.click(&mut bag, "Take off", &hub),
            BagAction::TakeOff { item: 1 }
        );
        // The zone's refusal is said as the zone said it, and the screen is usable again.
        assert_eq!(
            bag.worn(&hub, 1, Err("not in a fight: wait a moment".into())),
            None
        );
        run.look(&mut bag, &hub);
        assert!(run.shows("not in a fight: wait a moment"));
        // A zone that never answers is not waited for for ever.
        run.pass(ZONE_GAP + Duration::from_millis(1));
        run.click(&mut bag, "Take off", &hub);
        run.pass(PATIENCE + Duration::from_millis(1));
        run.look(&mut bag, &hub);
        assert!(run.shows("the zone did not answer: try again"));

        // The other sword is judged against the one worn; a part is worn nowhere.
        run.click(&mut bag, "sword  slash +2.0%", &hub);
        run.look(&mut bag, &hub);
        assert!(run.shows("you wear: slash +11.0%  electric +9.5%"));
        run.click(&mut bag, "ember", &hub);
        run.look(&mut bag, &hub);
        assert!(!run.offers("Wear"));
        assert!(run.shows("a catalyst, for crafting") && run.shows("of ember"));

        // Into the storage, seen there, and out again.
        run.click(&mut bag, "Store", &hub);
        run.look(&mut bag, &hub);
        assert!(run.shows("it is in the storage") && !run.offers("ember"));
        // What was picked went: nothing is picked now, and nothing acts until the person
        // picks again.
        assert!(run.shows("pick one to see it") && !run.offers("Store") && !run.offers("Wear"));
        run.click(&mut bag, "Storage", &hub);
        assert_eq!(bag.page, Page::Storage);
        run.look(&mut bag, &hub);
        assert!(run.shows("ember  a catalyst, for crafting"));
        run.click(&mut bag, "Take", &hub);
        run.look(&mut bag, &hub);
        assert!(run.shows("nothing is stored") && run.shows("it is in the inventory"));
        run.click(&mut bag, "Back", &hub);
        assert_eq!(bag.page, Page::Inventory);

        // Selling: a price in gold and silver, said back in words before it goes.
        run.click(&mut bag, "sword  slash +2.0%", &hub);
        run.click(&mut bag, "Sell", &hub);
        assert_eq!(bag.page, Page::Price);
        run.look(&mut bag, &hub);
        assert!(run.shows("name a price") && !run.offers("List it"));
        assert_eq!(run.st.focus(), Some("field:gold"));
        run.typed(&mut bag, "1", &hub);
        run.typed(&mut bag, "x", &hub);
        // An amount pasted with anything but digits in it is not taken: 12.50 is not 1250.
        run.typed(&mut bag, "2.50", &hub);
        run.click(&mut bag, "silver", &hub);
        run.typed(&mut bag, "205", &hub);
        run.look(&mut bag, &hub);
        assert!(
            run.shows("gold: 1") && run.shows("silver: 20"),
            "digits only, two of silver"
        );
        assert!(run.shows("1 g 20 s"));
        run.click(&mut bag, "List it", &hub);
        // The page changes between two frames: the frame that took the answer was still
        // the price's.
        run.look(&mut bag, &hub);
        assert_eq!(bag.page, Page::Inventory);
        assert!(run.shows("it is in your stall"));
        assert!(hub.0.borrow().asked.contains(&PlayerEcon::StallList {
            item: 2,
            price: 120
        }));
        // Escape closes.
        let escape = UiInput {
            keys: vec![Key::Escape],
            ..Default::default()
        };
        assert_eq!(run.frame(&mut bag, &escape, &hub), BagAction::Close);
    }

    #[test]
    fn what_was_picked_and_is_gone_is_not_replaced_by_its_neighbour() {
        // A keeper takes back what the buyer is looking at and puts something else in
        // its row: the buyer's Buy is refused, and after it nothing is picked.
        let hub = shop();
        hub.0.borrow_mut().listings = vec![ListingSummary {
            id: 71,
            item: item(11, "sword", &["slash +11.0%"], &BEST),
            price: 5,
        }];
        let mut run = Run::new(false);
        let mut bag = Bag::stall(&hub, S, ME, 5, "Smith", run.t);
        run.look(&mut bag, &hub);
        hub.0.borrow_mut().listings = vec![ListingSummary {
            id: 72,
            item: item(12, "sword", &["slash +1.0%"], &["core/tin", "frame/oak"]),
            price: 40_000,
        }];
        assert_eq!(
            run.click(&mut bag, "Buy", &hub),
            BagAction::Buy {
                stall: 5,
                listing: 71,
                price: 5
            }
        );
        assert_eq!(
            bag.bought(&hub, 71, Err("it is no longer for sale here".into())),
            None
        );
        run.pass(ZONE_GAP + Duration::from_millis(1));
        run.look(&mut bag, &hub);
        assert!(run.shows("it is no longer for sale here"));
        assert!(
            run.shows("sword  slash +1.0%  400 g"),
            "the other thing is listed"
        );
        assert!(
            run.shows("pick one to see it") && !run.offers("Buy"),
            "and it is not picked: a second click buys nothing"
        );
        // Picked by the person, it can be bought.
        run.click(&mut bag, "sword  slash +1.0%", &hub);
        assert_eq!(
            run.click(&mut bag, "Buy", &hub),
            BagAction::Buy {
                stall: 5,
                listing: 72,
                price: 40_000
            }
        );

        // The same on the price page: the item a price is being typed for is sold from
        // another client meanwhile. The price is never sent for another item.
        let hub = shop();
        let mut run = Run::new(true);
        let mut bag = Bag::inventory(&hub, S, ME, run.t);
        run.click(&mut bag, "Sell", &hub);
        assert_eq!(bag.page, Page::Price);
        run.typed(&mut bag, "5", &hub);
        hub.0.borrow_mut().items.remove(0);
        run.click(&mut bag, "List it", &hub);
        run.look(&mut bag, &hub);
        assert_eq!(bag.page, Page::Inventory);
        assert!(run.shows("it is not there any more"));
        assert!(
            hub.0
                .borrow()
                .asked
                .iter()
                .all(|op| !matches!(op, PlayerEcon::StallList { item, .. } if *item != 1)),
            "{:?}",
            hub.0.borrow().asked
        );
    }

    #[test]
    fn a_stall_is_bought_from_through_the_zone_at_the_price_shown() {
        let hub = shop();
        {
            let mut shop = hub.0.borrow_mut();
            shop.items[1].worn = true;
            shop.listings = vec![
                ListingSummary {
                    id: 71,
                    item: item(
                        11,
                        "sword",
                        &["slash +3.0%"],
                        &["core/dragonbone", "frame/oak"],
                    ),
                    price: 5,
                },
                ListingSummary {
                    id: 72,
                    item: item(12, "sword", &["slash +11.0%", "electric +9.5%"], &BEST),
                    price: 120,
                },
                ListingSummary {
                    id: 73,
                    item: item(13, "sword", &["slash +1.0%"], &["core/tin", "frame/oak"]),
                    price: 200,
                },
            ];
        }
        let mut run = Run::new(false);
        let mut bag = Bag::stall(&hub, S, ME, 5, "Smith", run.t);
        // Before the hub has answered: no "nothing is for sale", no comparison, no Buy.
        let idle = UiInput::default();
        run.frame(&mut bag, &idle, &hub);
        assert!(!run.shows("nothing is for sale here") && !run.shows("you wear"));
        assert!(!run.offers("Buy"));
        run.look(&mut bag, &hub);
        // Whose it is, what the buyer has, each thing with its price in units, and the one
        // picked beside what the buyer wears.
        assert!(run.shows("Smith's stall") && run.shows("you have") && run.shows("2 g 15 s"));
        assert!(run.shows("sword  slash +11.0%  1 g 20 s"));
        assert!(run.shows("sword  slash +3.0%  5 s"));
        assert!(run.shows("you wear: slash +2.0%"));
        assert!(run.st.clipped.is_empty(), "{:?}", run.st.clipped);
        assert!(run.st.cut_cells.is_empty(), "{:?}", run.st.cut_cells);

        // The second row is bought: the zone is asked, by the listing and at its price.
        run.click(&mut bag, "sword  slash +11.0%", &hub);
        assert_eq!(
            run.click(&mut bag, "Buy", &hub),
            BagAction::Buy {
                stall: 5,
                listing: 72,
                price: 120
            }
        );
        run.look(&mut bag, &hub);
        assert!(run.shows("asking the keeper") && !run.offers("Buy"));
        // Meanwhile somebody else bought the first row; nothing on the screen moves until
        // the zone answers this buy. An answer about another listing is not that answer.
        {
            let mut shop = hub.0.borrow_mut();
            shop.listings.remove(0);
            let bought = shop.listings.remove(0).item;
            shop.items.push(bought);
            shop.coin -= 120;
        }
        let asked = hub.0.borrow().asked.len();
        assert!(
            bag.bought(&hub, 71, Err("it is no longer for sale here".into()))
                .is_some()
        );
        run.look(&mut bag, &hub);
        assert!(run.shows("asking the keeper") && run.shows("sword  slash +3.0%  5 s"));
        assert_eq!(
            hub.0.borrow().asked.len(),
            asked,
            "the lists were not asked again"
        );
        assert_eq!(bag.bought(&hub, 72, Ok(())), None);
        run.look(&mut bag, &hub);
        assert!(run.shows("bought: it is in the inventory") && run.shows("95 s"));
        assert!(run.shows("sword  slash +1.0%  2 g") && !run.shows("slash +3.0%"));
        // What was bought is gone from the stall: nothing is picked, and Buy is off until
        // the person picks (and the zone's second has passed).
        assert!(!run.offers("Buy") && run.shows("pick one to see it"));
        run.pass(ZONE_GAP + Duration::from_millis(1));
        run.look(&mut bag, &hub);
        assert!(!run.offers("Buy"));
        run.click(&mut bag, "sword  slash +1.0%", &hub);
        // A refusal is said in the zone's words, and the purse and the stall asked again.
        assert_eq!(
            run.click(&mut bag, "Buy", &hub),
            BagAction::Buy {
                stall: 5,
                listing: 73,
                price: 200
            }
        );
        assert_eq!(bag.bought(&hub, 73, Err("not enough coin".into())), None);
        run.look(&mut bag, &hub);
        assert!(run.shows("not enough coin") && run.shows("sword  slash +1.0%  2 g"));
        // A zone that drops the request is not waited for for ever.
        run.pass(ZONE_GAP + Duration::from_millis(1));
        run.click(&mut bag, "Buy", &hub);
        run.pass(PATIENCE + Duration::from_millis(1));
        run.look(&mut bag, &hub);
        assert!(run.shows("the keeper did not answer: try again"));

        // The purchase went through and the lists could not be asked for again: the
        // purchase is what is said, the lists are called old, and nothing acts on them.
        run.pass(ZONE_GAP + Duration::from_millis(1));
        run.click(&mut bag, "Buy", &hub);
        hub.0.borrow_mut().deaf = Some(HubError::Busy);
        assert_eq!(bag.bought(&hub, 73, Ok(())), None);
        run.pass(ZONE_GAP + Duration::from_millis(1));
        run.look(&mut bag, &hub);
        assert!(
            run.shows("bought: it is in the inventory. This may be old: close it and look again")
        );
        assert!(!run.offers("Buy"));
        hub.0.borrow_mut().deaf = None;

        // At one's own stall the same screen takes things back instead.
        hub.0.borrow_mut().mine = true;
        let mut run = Run::new(true);
        let mut bag = Bag::stall(&hub, S, ME, 5, "Smith", run.t);
        run.look(&mut bag, &hub);
        assert!(run.shows("your stall") && !run.offers("Buy"));
        assert_eq!(run.click(&mut bag, "Take back", &hub), BagAction::None);
        run.look(&mut bag, &hub);
        assert!(run.shows("it is back in the inventory") && run.shows("nothing is for sale here"));
        assert!(
            hub.0
                .borrow()
                .asked
                .contains(&PlayerEcon::StallUnlist { listing: 73 })
        );
        // A stall that closed says so.
        hub.0.borrow_mut().closed = true;
        let mut bag = Bag::stall(&hub, S, ME, 5, "Smith", run.t);
        run.look(&mut bag, &hub);
        assert!(run.shows("this stall has closed"));
        let escape = UiInput {
            keys: vec![Key::Escape],
            ..Default::default()
        };
        assert_eq!(run.frame(&mut bag, &escape, &hub), BagAction::Close);
    }

    #[test]
    fn refusals_and_late_answers_leave_the_screen_true() {
        // A thing moved from another client: the hub says it is not this holder's, and
        // the screen says it is not there and shows what is (not "log in again").
        let hub = shop();
        let mut run = Run::new(false);
        let mut bag = Bag::inventory(&hub, S, ME, run.t);
        run.look(&mut bag, &hub);
        let gone = hub.0.borrow_mut().items.remove(0);
        hub.0.borrow_mut().stored.push(gone);
        run.click(&mut bag, "Store", &hub);
        run.look(&mut bag, &hub);
        assert!(run.shows("it is not there any more") && !run.shows("log in again"));
        assert!(!run.shows("sword  slash +11.0%") && run.shows("pick one to see it"));
        // The same from the storage: a Take of what is no longer stored asks the storage
        // again.
        run.click(&mut bag, "Storage", &hub);
        run.look(&mut bag, &hub);
        let back = hub.0.borrow_mut().stored.remove(0);
        hub.0.borrow_mut().items.push(back);
        run.click(&mut bag, "Take", &hub);
        run.look(&mut bag, &hub);
        assert!(run.shows("it is not there any more") && run.shows("nothing is stored"));

        // A session that ended is said as that, when a list is refused for it.
        hub.0.borrow_mut().deaf = Some(HubError::Unauthorized);
        let mut bag = Bag::inventory(&hub, S, ME, run.t);
        run.look(&mut bag, &hub);
        assert!(run.shows("the session has ended: leave and log in again"));
        assert!(
            !run.shows("nothing is carried"),
            "nothing was said of what is carried"
        );
        hub.0.borrow_mut().deaf = None;

        // Answers that arrive in another order than they were asked: the storage is asked
        // for, then a thing is put into it and the storage asked for again; the first
        // answer (an empty storage) arrives last, and is not taken.
        let hub = shop();
        let mut bag = Bag::inventory(&hub, S, ME, run.t);
        run.look(&mut bag, &hub);
        hub.0.borrow_mut().hold = true;
        run.click(&mut bag, "Storage", &hub);
        let empty = hub.0.borrow_mut().held.remove(0);
        hub.0.borrow_mut().hold = false;
        let moved = hub.0.borrow_mut().items.remove(2);
        hub.0.borrow_mut().stored.push(moved);
        bag.read(&hub, List::Storage);
        run.look(&mut bag, &hub);
        assert!(run.shows("ember  a catalyst, for crafting"));
        empty.0.fill(empty.1);
        run.look(&mut bag, &hub);
        assert!(
            run.shows("ember  a catalyst, for crafting") && !run.shows("nothing is stored"),
            "an older answer is not what is"
        );
    }

    #[test]
    fn every_page_is_whole_at_every_size() {
        for size in SIZES {
            let hub = shop();
            {
                // The longest things these screens are ever given.
                let mut shop = hub.0.borrow_mut();
                shop.coin = MAX_PRICE;
                let mut robe = item(
                    4,
                    "crossbow",
                    &["electric -11.1%", "other elements -9.9%"],
                    &BEST,
                );
                robe.what = "an armour, 250 of 250".into();
                shop.items.push(robe.clone());
                shop.items[1].worn = true;
                shop.items[1].does = vec!["slash +11.0%".into(), "air +9.5%".into()];
                // A part of the layer and the material with the longest names.
                shop.items[2] = item(3, "", &[], &["catalyst/thunderstone"]);
                shop.stored = shop.items.clone();
                shop.listings = (0..12)
                    .map(|i| ListingSummary {
                        id: i,
                        item: if i % 2 == 0 {
                            robe.clone()
                        } else {
                            item(30 + i, "", &[], &["catalyst/thunderstone"])
                        },
                        price: [MAX_PRICE - 1, 9_999_999, 12_345_678, 21_540][i as usize % 4],
                    })
                    .collect();
            }
            let t = Instant::now();
            let mut st = UiState::default();
            let mut pages = vec![
                Bag::inventory(&hub, S, ME, t),
                Bag::stall(&hub, S, ME, 5, "Smith", t),
            ];
            pages[0].picked = 3;
            let mut storage = Bag::inventory(&hub, S, ME, t);
            storage.page = Page::Storage;
            storage.read(&hub, List::Storage);
            let mut price = Bag::inventory(&hub, S, ME, t);
            price.page = Page::Price;
            price.selling = Some(4);
            price.price = ["99999999".into(), "99".into()];
            pages.extend([storage, price]);
            // The stall as its keeper sees it (the answer is made when it is asked for),
            // and one whose keeper's name is as long as a name gets.
            hub.0.borrow_mut().mine = true;
            pages.push(Bag::stall(&hub, S, ME, 5, "Smith", t));
            hub.0.borrow_mut().mine = false;
            hub.0.borrow_mut().closed = true;
            pages.push(Bag::stall(&hub, S, ME, 5, "A name of twenty-four b.", t));
            hub.0.borrow_mut().closed = false;
            // The longest things these screens say under their lists.
            let notices = [
                "one thing at a time: try again in a moment",
                "you are between zones: try again in a moment",
                "the session has ended: leave and log in again",
                "the hub could not be asked: try again",
            ];
            for (n, bag) in pages.iter_mut().enumerate() {
                // Three times: the first frame is drawn before the hub's answers are
                // taken, the last with a notice and with a list called old.
                for round in 0..3 {
                    if round == 2 {
                        bag.say(notices[n % notices.len()], true);
                        bag.old[0] = true;
                    }
                    let mut canvas = Recorder::new(size.0, size.1);
                    let input = UiInput::default();
                    let mut ui =
                        Ui::begin(&mut canvas, &mut st, &input, bag.page.name(), PANEL_UNITS);
                    bag.frame(&mut ui, &hub, true, t);
                    ui.end();
                    tidy(&canvas, &st, PANEL_UNITS);
                    assert!(
                        st.clipped.is_empty() && st.cut_cells.is_empty(),
                        "{} at {size:?}, round {round}: cut {:?} {:?}",
                        bag.page.name(),
                        st.clipped,
                        st.cut_cells
                    );
                }
            }
        }
    }

    /// The item bar in the inventory (LOOK.md 3.2, ITEMS.md 4): four slots under the
    /// grid with the cells' stacks and keys; a stack dragged onto one sets it and the hub
    /// is told; two cells dragged onto each other swap; a cell dragged into the grid
    /// empties; gear is not a stack and sets nothing.
    #[test]
    fn a_stack_is_set_on_the_bar_by_a_drag_and_the_hub_keeps_it() {
        let hub = shop();
        {
            let mut shop = hub.0.borrow_mut();
            let mut kit = item(7, "kit", &["heals 300, used with F"], &[]);
            kit.template = "kit".into();
            kit.components.clear();
            kit.quantity = 3;
            kit.cap = 5;
            kit.what = "kit ×3 of 5".into();
            shop.items.push(kit);
            shop.bar = vec![None, None, None, None];
        }
        let mut run = Run::new(false);
        let mut bag = Bag::inventory(&hub, S, ME, run.t);
        run.look(&mut bag, &hub);
        assert!(
            run.offers("bar F")
                && run.offers("bar 8")
                && run.offers("bar 9")
                && run.offers("bar 0")
        );
        assert_eq!(bag.bar().map(<[_]>::len), Some(4));
        // The kit onto the second cell: the hub is told, and the row shows it there.
        assert_eq!(
            run.drag(&mut bag, "kit ×3  heals 300, used with F", "bar 8", &hub),
            BagAction::None
        );
        run.look(&mut bag, &hub);
        assert!(run.shows("the bar is set"), "{:?}", bag.notice);
        assert_eq!(
            hub.0.borrow().bar,
            vec![None, Some("kit".to_string()), None, None]
        );
        assert_eq!(bag.bar(), Some(hub.0.borrow().bar.as_slice()));
        // A sword is gear, not a stack: dropped on a cell it sets nothing.
        run.drag(&mut bag, "sword  slash +11.0%", "bar 9", &hub);
        run.look(&mut bag, &hub);
        assert_eq!(hub.0.borrow().bar[2], None);
        // The cell onto the first: swapped, under F.
        run.drag(&mut bag, "bar 8", "bar F", &hub);
        run.look(&mut bag, &hub);
        assert_eq!(
            hub.0.borrow().bar,
            vec![Some("kit".to_string()), None, None, None]
        );
        // The stack from the grid onto another cell while it sits on F: it moves (a kind
        // sits on one cell).
        run.drag(&mut bag, "kit ×3  heals 300, used with F", "bar 0", &hub);
        run.look(&mut bag, &hub);
        assert_eq!(
            hub.0.borrow().bar,
            vec![None, None, None, Some("kit".to_string())]
        );
        // The cell into the grid: empty again.
        run.drag(&mut bag, "bar 0", "sword  slash +11.0%", &hub);
        run.look(&mut bag, &hub);
        assert_eq!(hub.0.borrow().bar, vec![None, None, None, None]);
        assert!(run.st.clipped.is_empty(), "{:?}", run.st.clipped);
    }
}

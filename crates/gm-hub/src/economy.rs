//! The economy (ECONOMY.md): holders, items, coin, the ledger, trades, stalls, escrow
//! contracts, drops, crafting, storage, guild chests and hires. Every public function is one
//! database transaction; `move_coin` and `move_item` are the only writers of balances and
//! ownership.

use gm_core::sim::BAR_CELLS;
use std::time::Duration;

use gm_content::items::{ItemContent, Place};
use gm_core::matrix::Gear;
use sqlx::postgres::PgPool;
use sqlx::{Postgres, Row, Transaction};

pub const INVENTORY_SLOTS: i32 = 24;
pub const STORAGE_SLOTS: i32 = 60;
pub const STALL_SLOTS: i32 = 12;
pub const STALL_HOURS: i64 = 48;
pub const HIRE_BURN_PER_CENT: i64 = 30;
pub const HIRE_WINDOW_HOURS: i64 = 12;
pub const HIRES_BEFORE_DEMOTION: i64 = 3;
/// The most any one amount may be, in silver (ECONOMY.md 2): a hundred million gold.
pub const MAX_PRICE: i64 = 10_000_000_000;
pub const LAYERS: [&str; 5] = ["shard", "core", "catalyst", "frame", "gem"];
pub const MAX_GEMS: usize = 2;
/// The most parts an item is made of: a shard, a core, a catalyst, a frame and the gems.
pub const MAX_PARTS: usize = 4 + MAX_GEMS;
pub const CHEST_SLOTS: i32 = 48;
/// An active contract the zone never reported on is refunded after this long.
pub const CONTRACT_TIMEOUT_MINUTES: i32 = 120;
/// What every mover says to a worn item (ITEMS.md 2).
pub const WORN: &str = "it is worn: take it off first";
/// Whether a build whose abilities hold `hands` (the props, `hub::hands`) may wear an
/// item of `template` in `place` (ITEMS.md 2): a weapon only when its model is one of
/// them; anything else, yes. `Err` carries the words for the player: "this build's hands
/// are for the musket, the pistol and the dagger: not a staff".
pub fn fits(
    content: &ItemContent,
    hands: &[String],
    template: &str,
    place: Option<Place>,
) -> Result<(), String> {
    if place != Some(Place::Weapon) {
        return Ok(());
    }
    let model = content.model(template).unwrap_or(template);
    if hands.iter().any(|h| h == model) {
        return Ok(());
    }
    let held: Vec<String> = hands.iter().map(|h| format!("the {h}")).collect();
    let held = match held.len() {
        0 => "nothing".to_string(),
        1 => held[0].clone(),
        n => format!("{} and {}", held[..n - 1].join(", "), held[n - 1]),
    };
    Err(format!(
        "this build's hands are for {held}: not a {template}"
    ))
}

/// What is said of an item somebody would wear and does not carry.
pub const NOT_CARRIED: &str = "that is not in the inventory";

/// Which components survive a decomposition (ECONOMY.md 10): ⌊k / 2⌋ of them, ordered by a
/// hash of the item id and the component's index. The crafter cannot choose the item id, so
/// padding a craft with junk does not steer what comes back.
pub fn salvage_indices(item: i64, k: usize) -> Vec<usize> {
    let key = |i: usize| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in item
            .to_le_bytes()
            .into_iter()
            .chain((i as u64).to_le_bytes())
        {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    };
    let mut order: Vec<usize> = (0..k).collect();
    order.sort_by_key(|&i| (key(i), i));
    order.truncate(k / 2);
    order.sort_unstable();
    order
}

type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum EconError {
    #[error("not found")]
    NotFound,
    #[error("not yours")]
    Forbidden,
    #[error("not enough coin")]
    Insufficient,
    #[error("no room")]
    Full,
    #[error("wrong state: {0}")]
    State(String),
    #[error("too soon after the last change")]
    Cooldown,
    #[error("invalid: {0}")]
    Invalid(String),
    /// The database chose this transaction as a deadlock or serialization victim; nothing
    /// happened and the request can be repeated.
    #[error("busy, try again")]
    Busy,
    #[error("internal error")]
    Internal,
}

fn internal(e: sqlx::Error) -> EconError {
    // A constraint the code should have checked first still decides (the database is the
    // last line): unique violations are conflicts, check violations are invalid moves.
    if let sqlx::Error::Database(d) = &e {
        if d.is_unique_violation() {
            return EconError::State("taken".into());
        }
        if d.is_check_violation() {
            return EconError::Insufficient;
        }
        // Migration 0009's trigger: a worn item was about to leave its wearer.
        if d.code().as_deref() == Some("GM001") {
            return EconError::State(WORN.into());
        }
        if matches!(d.code().as_deref(), Some("40P01" | "40001")) {
            return EconError::Busy;
        }
    }
    tracing::error!("economy database: {e}");
    EconError::Internal
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub layer: String,
    pub material: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub template: String,
    pub components: Vec<Component>,
    /// Somebody wears it (ITEMS.md 2): only ever true in its wearer's inventory.
    pub worn: bool,
    /// How many (MODES.md 11.1): 1 for gear and parts, a stack's count for a stack.
    pub quantity: u32,
}

/// A stack in a character's inventory, as a zone reads it (MODES.md 11.2).
/// What `Economy::gear` reads of a character (ITEMS.md 3.3): the reading's number, the
/// gear term, the templates worn, the stacks carried and the item bar (LOOK.md 3.2).
pub type Reading = (u64, Gear, [String; 2], Vec<Stack>, Vec<Option<String>>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stack {
    pub item: i64,
    pub template: String,
    pub quantity: u32,
    pub heals: Option<i32>,
}

/// The stack templates the hub knows (MODES.md 11.1): cap and heal by template key. Set
/// from the content at start; a template not here is gear or a part and never merges.
pub type StackCaps = std::collections::HashMap<String, (u32, Option<i32>)>;

/// The words a buyer, a trader or an operator is told at the cap.
pub fn at_the_cap(template: &str) -> String {
    format!("you carry all the {template}s you can")
}

/// What a stall has for sale: the listing's id, the item and its price in silver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub id: i64,
    pub item: Item,
    pub price: i64,
}

/// A hire in its window that nobody ended (COMPANIONS.md 3.3). `build` is the avatar's
/// stored build as the database holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveHire {
    pub id: i64,
    pub avatar: i64,
    pub name: String,
    pub build: serde_json::Value,
    pub expires_unix: i64,
}

/// A character listed for hire (ECONOMY.md 11).
#[derive(Clone, Debug, PartialEq)]
pub struct TavernRow {
    pub character: i64,
    pub name: String,
    pub build: serde_json::Value,
    pub price: i64,
    /// Hires in the last 12 h.
    pub hires: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Boss dead and the buyer present (alive or not): the sellers are paid.
    Completed,
    /// The party wiped: the buyer is refunded, collateral returns to the leader.
    Wipe,
    /// The party abandoned: the buyer is refunded and takes the collateral.
    Abandon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeStatus {
    Waiting,
    Committed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeState {
    Open,
    Committed,
    Cancelled,
}

/// One side of a trade: its coin, whether it accepted, what it offers.
pub type TradeSide = (i64, bool, Vec<Item>);

/// A trade as one of its two sees it (`Economy::trade_view`).
#[derive(Clone, Debug)]
pub struct TradeSeen {
    pub state: TradeState,
    /// The version an accept must name.
    pub version: i32,
    /// Milliseconds until an accept is taken (the mutation lock, ECONOMY.md 6).
    pub wait_ms: u32,
    /// The other's name.
    pub with: String,
    /// Both play in one zone.
    pub together: bool,
    pub mine: TradeSide,
    pub theirs: TradeSide,
}

/// A trade nobody has touched for this long is called off.
pub const TRADE_IDLE_MINUTES: i32 = 10;
/// What a hirer is told when the price is no longer the one it was shown.
pub const PRICE_CHANGED: &str = "the price changed: look again";
/// The listing's build cannot be played any more (PARTY.md 7).
pub const NOT_FOR_HIRE: &str = "that character is not for hire now: its owner must list it again";

/// What somebody is told who accepts a trade whose other side has gone elsewhere.
pub const NOT_HERE: &str = "the other is not here any more: the trade is called off";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Supply {
    /// Coin in the world: every holder except the source and the sink.
    pub circulating: i64,
    /// Coin ever created (the ledger rows out of the source).
    pub created: i64,
    /// Coin ever destroyed.
    pub burned: i64,
}

#[derive(Clone)]
pub struct Economy {
    pool: PgPool,
    /// The stack templates (MODES.md 11.1), from the content.
    pub stacks: StackCaps,
    /// The trade window's accept cooldown (ECONOMY.md 6); 3 s in production.
    pub trade_cooldown: Duration,
    /// An accept is taken only while both characters play in one zone (ECONOMY.md 6);
    /// off in the tests of the window itself, whose characters are rows and play nowhere.
    pub trades_need_a_zone: bool,
}

/// The layer a material belongs to: materials are named `layer/name` (`core/iron`).
pub fn material_layer(material: &str) -> Result<&str, EconError> {
    let (layer, name) = material
        .split_once('/')
        .ok_or_else(|| EconError::Invalid(format!("material {material:?} is not layer/name")))?;
    if !LAYERS.contains(&layer) || name.is_empty() || material.len() > 64 {
        return Err(EconError::Invalid(format!("unknown material {material:?}")));
    }
    Ok(layer)
}

fn check_price(price: i64) -> Result<(), EconError> {
    if price <= 0 || price > MAX_PRICE {
        return Err(EconError::Invalid("price out of range".into()));
    }
    Ok(())
}

// ---------- the two movers ----------

/// Lock holders in ascending id order (one lock order everywhere: no deadlocks) and return
/// `(id, kind, capacity, coin)` for each.
async fn lock_holders(
    tx: &mut Tx<'_>,
    ids: &[i64],
) -> Result<Vec<(i64, String, i32, i64)>, EconError> {
    let mut sorted: Vec<i64> = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::with_capacity(sorted.len());
    for id in sorted {
        let r =
            sqlx::query("select id, kind, capacity, coin from holders where id = $1 for update")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(internal)?
                .ok_or(EconError::NotFound)?;
        out.push((
            r.try_get("id").map_err(internal)?,
            r.try_get("kind").map_err(internal)?,
            r.try_get("capacity").map_err(internal)?,
            r.try_get("coin").map_err(internal)?,
        ));
    }
    Ok(out)
}

/// Move coin between two holders and write the ledger row (ECONOMY.md 5).
async fn move_coin(
    tx: &mut Tx<'_>,
    from: i64,
    to: i64,
    amount: i64,
    reason: &str,
    reference: i64,
) -> Result<(), EconError> {
    if amount == 0 {
        return Ok(());
    }
    if amount < 0 || from == to {
        return Err(EconError::Invalid("coin move".into()));
    }
    // The source and the sink keep no balance and take no lock (every drop in the world would
    // queue on one row); what they created and destroyed is the sum of their ledger rows.
    let (source, sink) = singletons(tx).await?;
    if to == source || from == sink {
        return Err(EconError::Invalid("coin move".into()));
    }
    let real: Vec<i64> = [from, to]
        .into_iter()
        .filter(|h| *h != source && *h != sink)
        .collect();
    let locked = lock_holders(tx, &real).await?;
    if from != source {
        let src = locked
            .iter()
            .find(|h| h.0 == from)
            .ok_or(EconError::NotFound)?;
        if src.3 < amount {
            return Err(EconError::Insufficient);
        }
        sqlx::query("update holders set coin = coin - $2 where id = $1")
            .bind(from)
            .bind(amount)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
    }
    if to != sink {
        sqlx::query("update holders set coin = coin + $2 where id = $1")
            .bind(to)
            .bind(amount)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
    }
    sqlx::query("insert into coin_ledger (from_holder, to_holder, amount, reason, ref) values ($1, $2, $3, $4, $5)")
        .bind(from)
        .bind(to)
        .bind(amount)
        .bind(reason)
        .bind(reference)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    Ok(())
}

/// `move_item` knowing the stack templates (MODES.md 11.1): the `Economy`'s methods pass
/// theirs; the free functions above know none and move every row as one thing.
#[allow(clippy::too_many_arguments)]
async fn move_item_stacking(
    tx: &mut Tx<'_>,
    item: i64,
    expect_from: i64,
    to: i64,
    reason: &str,
    reference: i64,
    overflow: bool,
    stacks: &StackCaps,
) -> Result<(), EconError> {
    // Holders first (ascending), then the item row: the same order everywhere.
    let locked = lock_holders(tx, &[expect_from, to]).await?;
    let row = sqlx::query("select holder_id from items where id = $1 for update")
        .bind(item)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
    let holder: i64 = row.try_get("holder_id").map_err(internal)?;
    if holder != expect_from {
        return Err(EconError::Forbidden);
    }
    if is_worn(tx, item).await? {
        return Err(EconError::State(WORN.into()));
    }
    if expect_from != to {
        let target = locked
            .iter()
            .find(|h| h.0 == to)
            .ok_or(EconError::NotFound)?;
        // A stack into an inventory or a storage (MODES.md 11.1): onto the stack of its
        // template already there, up to the cap; past the cap, refused in words before
        // anything moves; nothing of it stays separate.
        if let Some(cap) = stacks.get(&row_template(tx, item).await?).map(|s| s.0)
            && matches!(target.1.as_str(), "character" | "storage")
        {
            let quantity = row_quantity(tx, item).await?;
            if let Some(into) = absorb_stack(tx, to, item, quantity, cap).await? {
                sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, $3, $4, $5)")
                    .bind(item)
                    .bind(expect_from)
                    .bind(to)
                    .bind(reason)
                    .bind(reference)
                    .execute(&mut **tx)
                    .await
                    .map_err(internal)?;
                // The moving row is spent into the one there: whatever still names it
                // (a listing just sold, a trade just committed) lets go first.
                unname_item(tx, item).await?;
                sqlx::query("delete from items where id = $1")
                    .bind(item)
                    .execute(&mut **tx)
                    .await
                    .map_err(internal)?;
                let _ = into;
                return Ok(());
            }
        }
        if target.2 > 0 && !overflow {
            let count: i64 = sqlx::query("select count(*) from items where holder_id = $1")
                .bind(to)
                .fetch_one(&mut **tx)
                .await
                .map_err(internal)?
                .try_get(0)
                .map_err(internal)?;
            if count >= target.2 as i64 {
                return Err(EconError::Full);
            }
        }
        sqlx::query("update items set holder_id = $2 where id = $1")
            .bind(item)
            .bind(to)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
    }
    sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, $3, $4, $5)")
        .bind(item)
        .bind(expect_from)
        .bind(to)
        .bind(reason)
        .bind(reference)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    Ok(())
}

// ---------- holders ----------

async fn character_holder(tx: &mut Tx<'_>, character: i64) -> Result<i64, EconError> {
    sqlx::query("insert into holders (kind, character_id, capacity) values ('character', $1, $2) on conflict do nothing")
        .bind(character)
        .bind(INVENTORY_SLOTS)
        .execute(&mut **tx)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref d) if d.is_foreign_key_violation() => EconError::NotFound,
            e => internal(e),
        })?;
    sqlx::query("select id from holders where kind = 'character' and character_id = $1")
        .bind(character)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?
        .try_get("id")
        .map_err(internal)
}

async fn account_of(tx: &mut Tx<'_>, character: i64) -> Result<i64, EconError> {
    sqlx::query("select account_id from characters where id = $1")
        .bind(character)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?
        .try_get("account_id")
        .map_err(internal)
}

async fn storage_holder(tx: &mut Tx<'_>, character: i64) -> Result<i64, EconError> {
    let account = account_of(tx, character).await?;
    sqlx::query("insert into holders (kind, account_id, capacity) values ('storage', $1, $2) on conflict do nothing")
        .bind(account)
        .bind(STORAGE_SLOTS)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    sqlx::query("select id from holders where kind = 'storage' and account_id = $1")
        .bind(account)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)
}

async fn ground_holder(tx: &mut Tx<'_>, zone: &str) -> Result<i64, EconError> {
    sqlx::query("insert into holders (kind, zone) values ('ground', $1) on conflict do nothing")
        .bind(zone)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    sqlx::query("select id from holders where kind = 'ground' and zone = $1")
        .bind(zone)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)
}

/// `(source, sink)`.
async fn singletons(tx: &mut Tx<'_>) -> Result<(i64, i64), EconError> {
    Ok((singleton(tx, "source").await?, singleton(tx, "sink").await?))
}

async fn singleton(tx: &mut Tx<'_>, kind: &str) -> Result<i64, EconError> {
    sqlx::query("select id from holders where kind = $1")
        .bind(kind)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)
}

async fn new_holder(tx: &mut Tx<'_>, kind: &str, capacity: i32) -> Result<i64, EconError> {
    sqlx::query("insert into holders (kind, capacity) values ($1, $2) returning id")
        .bind(kind)
        .bind(capacity)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)
}

async fn components_of(tx: &mut Tx<'_>, item: i64) -> Result<Vec<Component>, EconError> {
    let rows = sqlx::query(
        "select layer, material from item_components where item_id = $1 order by \
         array_position(array['shard','core','catalyst','frame','gem'], layer), position",
    )
    .bind(item)
    .fetch_all(&mut **tx)
    .await
    .map_err(internal)?;
    rows.iter()
        .map(|r| {
            Ok(Component {
                layer: r.try_get("layer").map_err(internal)?,
                material: r.try_get("material").map_err(internal)?,
            })
        })
        .collect()
}

async fn create_component(tx: &mut Tx<'_>, holder: i64, material: &str) -> Result<i64, EconError> {
    let layer = material_layer(material)?;
    let id: i64 = sqlx::query(
        "insert into items (template, holder_id) values ('component', $1) returning id",
    )
    .bind(holder)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal)?
    .try_get("id")
    .map_err(internal)?;
    sqlx::query(
        "insert into item_components (item_id, layer, position, material) values ($1, $2, 0, $3)",
    )
    .bind(id)
    .bind(layer)
    .bind(material)
    .execute(&mut **tx)
    .await
    .map_err(internal)?;
    Ok(id)
}

/// The character's row, held against a change of where it is until the transaction ends
/// (a claim, a handoff and a save take it whole), and the proof that it plays in `zone`:
/// not offline, not on its way anywhere.
async fn playing_in(tx: &mut Tx<'_>, character: i64, zone: &str) -> Result<(), EconError> {
    let r =
        sqlx::query("select location_kind, location_zone from characters where id = $1 for share")
            .bind(character)
            .fetch_optional(&mut **tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?;
    let kind: String = r.try_get("location_kind").map_err(internal)?;
    let at: Option<String> = r.try_get("location_zone").map_err(internal)?;
    if kind != "zone" || at.as_deref() != Some(zone) {
        return Err(EconError::Forbidden);
    }
    Ok(())
}

/// What the character's worn items do (ITEMS.md 3.2), within a transaction.
/// The sixteen numbers of what a character wears, and the templates worn by place
/// (weapon, armour; empty for nothing).
async fn gear_in(
    tx: &mut Tx<'_>,
    character: i64,
    content: &ItemContent,
) -> Result<(Gear, [String; 2]), EconError> {
    let rows = sqlx::query(
        "select w.slot, i.id, i.template from worn w join items i on i.id = w.item_id \
         where w.character_id = $1",
    )
    .bind(character)
    .fetch_all(&mut **tx)
    .await
    .map_err(internal)?;
    let mut gear = Gear::NONE;
    let mut templates: [String; 2] = Default::default();
    for r in rows {
        let slot: String = r.try_get("slot").map_err(internal)?;
        let id: i64 = r.try_get("id").map_err(internal)?;
        let template: String = r.try_get("template").map_err(internal)?;
        let parts = components_of(tx, id).await?;
        let materials = parts.iter().map(|c| c.material.as_str());
        // Content that no longer knows the template, or knows it as another kind than
        // the place it was put into, gives nothing (and is still named: what is worn is
        // worn, whatever it does).
        match content.edges(&template, materials) {
            Some((Place::Weapon, edges, _)) if slot == Place::Weapon.name() => gear.dealt = edges,
            Some((Place::Armour, edges, _)) if slot == Place::Armour.name() => gear.taken = edges,
            _ => {}
        }
        if slot == Place::Weapon.name() {
            templates[0] = template;
        } else if slot == Place::Armour.name() {
            templates[1] = template;
        }
    }
    Ok((gear, templates))
}

/// The stacks of a character's inventory (MODES.md 11.2), in the same reading as its gear.
/// A character that does not exist carries none.
/// The item bar's cells (LOOK.md 3.2): the row the character arranged, else the default
/// made of what it carries now (the first stack that heals, on the first cell).
async fn bar_in(
    tx: &mut Tx<'_>,
    character: i64,
    stacks: &[Stack],
) -> Result<Vec<Option<String>>, EconError> {
    let row =
        sqlx::query("select cell_1, cell_2, cell_3, cell_4 from bars where character_id = $1")
            .bind(character)
            .fetch_optional(&mut **tx)
            .await
            .map_err(internal)?;
    match row {
        Some(r) => Ok((1..=BAR_CELLS)
            .map(|i| r.try_get::<Option<String>, _>(format!("cell_{i}").as_str()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?),
        None => {
            let mut bar = vec![None; BAR_CELLS];
            bar[0] = stacks
                .iter()
                .find(|s| s.heals.is_some())
                .map(|s| s.template.clone());
            Ok(bar)
        }
    }
}

async fn stacks_in(
    tx: &mut Tx<'_>,
    character: i64,
    stacks: &StackCaps,
) -> Result<Vec<Stack>, EconError> {
    let Some(inv) = sqlx::query(
        "select h.id from holders h where h.character_id = $1 and h.kind = 'character'",
    )
    .bind(character)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal)?
    else {
        return Ok(Vec::new());
    };
    let inv: i64 = inv.try_get("id").map_err(internal)?;
    let rows =
        sqlx::query("select id, template, quantity from items where holder_id = $1 order by id")
            .bind(inv)
            .fetch_all(&mut **tx)
            .await
            .map_err(internal)?;
    let mut out = Vec::new();
    for r in rows {
        let template: String = r.try_get("template").map_err(internal)?;
        if let Some((_, heals)) = stacks.get(&template) {
            let quantity: i32 = r.try_get("quantity").map_err(internal)?;
            out.push(Stack {
                item: r.try_get("id").map_err(internal)?,
                template,
                quantity: quantity.max(0) as u32,
                heals: *heals,
            });
        }
    }
    Ok(out)
}

/// Whether somebody wears the item (ITEMS.md 2). Asked with the item's row locked.
async fn is_worn(tx: &mut Tx<'_>, item: i64) -> Result<bool, EconError> {
    Ok(sqlx::query("select 1 from worn where item_id = $1")
        .bind(item)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .is_some())
}

/// What an item may be made of (ECONOMY.md 10): one core and one frame, at most one shard
/// and one catalyst, at most two gems, and nothing of a layer its template has no room for.
fn check_parts(
    parts: &[Component],
    template: &str,
    layers: Option<&[String]>,
) -> Result<(), EconError> {
    if let Some(layers) = layers
        && let Some(odd) = parts.iter().find(|c| !layers.contains(&c.layer))
    {
        return Err(EconError::Invalid(format!(
            "a {template} takes no {}",
            odd.layer
        )));
    }
    let count = |layer: &str| parts.iter().filter(|c| c.layer == layer).count();
    if count("core") != 1 || count("frame") != 1 {
        return Err(EconError::Invalid(
            "a craft needs exactly one core and one frame".into(),
        ));
    }
    if count("shard") > 1 || count("catalyst") > 1 || count("gem") > MAX_GEMS {
        return Err(EconError::Invalid("too many components for a layer".into()));
    }
    Ok(())
}

/// A new item of `template` made of `parts`, in `holder`. The caller writes its move.
/// Whatever still names an item row that is about to be spent into another (a listing
/// just sold, a trade just committed) lets go of it.
async fn unname_item(tx: &mut Tx<'_>, item: i64) -> Result<(), EconError> {
    sqlx::query("delete from listings where item_id = $1")
        .bind(item)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    sqlx::query("delete from trade_items where item_id = $1")
        .bind(item)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    Ok(())
}

/// The template and the quantity of an item row (the row is held by the caller).
async fn row_template(tx: &mut Tx<'_>, item: i64) -> Result<String, EconError> {
    sqlx::query("select template from items where id = $1")
        .bind(item)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?
        .try_get("template")
        .map_err(internal)
}

async fn row_quantity(tx: &mut Tx<'_>, item: i64) -> Result<u32, EconError> {
    let q: i32 = sqlx::query("select quantity from items where id = $1")
        .bind(item)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?
        .try_get("quantity")
        .map_err(internal)?;
    Ok(q.max(0) as u32)
}

/// `quantity` of a stack arrives in `holder` (MODES.md 11.1): onto the row of the same
/// template there, if one is, up to `cap`; `Some(row)` when it was taken up, `None` when
/// the holder has no such stack yet (the caller makes or moves a row, within the cap).
/// Past the cap either way: refused in words, and nothing has changed. `except` is the
/// arriving row itself, never merged into itself.
async fn absorb_stack(
    tx: &mut Tx<'_>,
    holder: i64,
    except: i64,
    quantity: u32,
    cap: u32,
) -> Result<Option<i64>, EconError> {
    let template = row_template(tx, except).await?;
    let there = sqlx::query(
        "select id, quantity from items where holder_id = $1 and template = $2 and id <> $3          order by id for update",
    )
    .bind(holder)
    .bind(&template)
    .bind(except)
    .fetch_optional(&mut **tx)
    .await
    .map_err(internal)?;
    match there {
        Some(r) => {
            let id: i64 = r.try_get("id").map_err(internal)?;
            let have: i32 = r.try_get("quantity").map_err(internal)?;
            if have.max(0) as u32 + quantity > cap {
                return Err(EconError::Invalid(at_the_cap(&template)));
            }
            sqlx::query("update items set quantity = quantity + $2 where id = $1")
                .bind(id)
                .bind(quantity as i32)
                .execute(&mut **tx)
                .await
                .map_err(internal)?;
            Ok(Some(id))
        }
        None => {
            if quantity > cap {
                return Err(EconError::Invalid(at_the_cap(&template)));
            }
            Ok(None)
        }
    }
}

async fn create_item(
    tx: &mut Tx<'_>,
    holder: i64,
    template: &str,
    parts: &[Component],
) -> Result<i64, EconError> {
    create_item_n(tx, holder, template, parts, 1).await
}

async fn create_item_n(
    tx: &mut Tx<'_>,
    holder: i64,
    template: &str,
    parts: &[Component],
    quantity: u32,
) -> Result<i64, EconError> {
    let id: i64 = sqlx::query(
        "insert into items (template, holder_id, quantity) values ($1, $2, $3) returning id",
    )
    .bind(template)
    .bind(holder)
    .bind(quantity.clamp(1, 1000) as i32)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal)?
    .try_get("id")
    .map_err(internal)?;
    let mut pos = std::collections::HashMap::<&str, i16>::new();
    for c in parts {
        let p = pos.entry(c.layer.as_str()).or_insert(0);
        sqlx::query("insert into item_components (item_id, layer, position, material) values ($1, $2, $3, $4)")
            .bind(id)
            .bind(&c.layer)
            .bind(*p)
            .bind(&c.material)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
        *p += 1;
    }
    Ok(id)
}

/// An item leaves every open trade it is offered in, and those trades lose their accepts:
/// nobody commits to an offer that silently shrank. What finished trades wrote down of it
/// stays written.
async fn leave_open_offers(tx: &mut Tx<'_>, item: i64) -> Result<(), EconError> {
    sqlx::query(
        "update trades set a_accepted = false, b_accepted = false, changed_at = now(), version = version + 1 \
         where state = 'open' and id in (select trade_id from trade_items where item_id = $1)",
    )
    .bind(item)
    .execute(&mut **tx)
    .await
    .map_err(internal)?;
    sqlx::query(
        "delete from trade_items where item_id = $1 \
         and trade_id in (select id from trades where state = 'open')",
    )
    .bind(item)
    .execute(&mut **tx)
    .await
    .map_err(internal)?;
    Ok(())
}

/// An item about to be destroyed leaves every trade: the open ones as above, and the rows
/// finished trades kept of it, which refer to an item that will not be there.
async fn detach_from_trades(tx: &mut Tx<'_>, item: i64) -> Result<(), EconError> {
    leave_open_offers(tx, item).await?;
    sqlx::query("delete from trade_items where item_id = $1")
        .bind(item)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    Ok(())
}

async fn has_room(tx: &mut Tx<'_>, holder: i64, incoming: i64) -> Result<bool, EconError> {
    let r = sqlx::query("select capacity, (select count(*) from items where holder_id = $1) as n from holders where id = $1")
        .bind(holder)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal)?;
    let cap: i32 = r.try_get("capacity").map_err(internal)?;
    let n: i64 = r.try_get("n").map_err(internal)?;
    Ok(cap == 0 || n + incoming <= cap as i64)
}

impl Economy {
    pub fn new(pool: PgPool) -> Economy {
        Economy {
            pool,
            stacks: StackCaps::new(),
            trade_cooldown: Duration::from_secs(3),
            trades_need_a_zone: true,
        }
    }

    /// Learn the stack templates from the content (MODES.md 11.1).
    pub fn with_stacks(mut self, content: &ItemContent) -> Economy {
        self.set_stacks(content);
        self
    }

    pub fn set_stacks(&mut self, content: &ItemContent) {
        self.stacks = content
            .templates
            .iter()
            .filter_map(|t| content.stack(&t.id).map(|s| (t.id.clone(), s)))
            .collect();
    }

    /// The one item mover, knowing this economy's stacks (MODES.md 11.1).
    async fn mv(
        &self,
        tx: &mut Tx<'_>,
        item: i64,
        expect_from: i64,
        to: i64,
        reason: &str,
        reference: i64,
    ) -> Result<(), EconError> {
        move_item_stacking(
            tx,
            item,
            expect_from,
            to,
            reason,
            reference,
            false,
            &self.stacks,
        )
        .await
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn begin(&self) -> Result<Tx<'_>, EconError> {
        self.pool.begin().await.map_err(internal)
    }

    // ---------- reads ----------

    /// A character's coin and items.
    pub async fn inventory(&self, character: i64) -> Result<(i64, Vec<Item>), EconError> {
        let mut tx = self.begin().await?;
        let holder = character_holder(&mut tx, character).await?;
        let out = self.holder_contents(&mut tx, holder).await?;
        tx.commit().await.map_err(internal)?;
        Ok(out)
    }

    /// The account storage behind a character.
    pub async fn storage(&self, character: i64) -> Result<(i64, Vec<Item>), EconError> {
        let mut tx = self.begin().await?;
        let holder = storage_holder(&mut tx, character).await?;
        let out = self.holder_contents(&mut tx, holder).await?;
        tx.commit().await.map_err(internal)?;
        Ok(out)
    }

    async fn holder_contents(
        &self,
        tx: &mut Tx<'_>,
        holder: i64,
    ) -> Result<(i64, Vec<Item>), EconError> {
        let coin: i64 = sqlx::query("select coin from holders where id = $1")
            .bind(holder)
            .fetch_one(&mut **tx)
            .await
            .map_err(internal)?
            .try_get("coin")
            .map_err(internal)?;
        let rows = sqlx::query(
            "select id, template, quantity, exists (select 1 from worn w where w.item_id = items.id) as worn \
             from items where holder_id = $1 order by id",
        )
        .bind(holder)
        .fetch_all(&mut **tx)
        .await
        .map_err(internal)?;
        let mut items = Vec::with_capacity(rows.len());
        for r in rows {
            let id: i64 = r.try_get("id").map_err(internal)?;
            let quantity: i32 = r.try_get("quantity").map_err(internal)?;
            items.push(Item {
                id,
                template: r.try_get("template").map_err(internal)?,
                components: components_of(tx, id).await?,
                worn: r.try_get("worn").map_err(internal)?,
                quantity: quantity.max(0) as u32,
            });
        }
        Ok((coin, items))
    }

    // ---------- what is worn (ITEMS.md 2) ----------

    /// A character playing in `zone` puts an item of its own inventory into its place
    /// (the template's kind says which); whatever was there is taken off and stays in the
    /// inventory. The item leaves every trade it was offered in: what is worn is not for
    /// sale. Returns a reading of what the character's worn items do, made after the
    /// change was committed.
    ///
    /// The character's row is held against a change of where it is until this commits: a
    /// claim by another zone either sees what was put on, or comes first and this is
    /// refused.
    ///
    /// `hands` is what the character's build holds (`hub::hands`): a weapon whose model
    /// none of its abilities holds is refused in words (ITEMS.md 2).
    pub async fn wear(
        &self,
        character: i64,
        zone: &str,
        item: i64,
        content: &ItemContent,
        hands: &[String],
    ) -> Result<Reading, EconError> {
        let mut tx = self.begin().await?;
        playing_in(&mut tx, character, zone).await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        // Whose it is, before anything of anybody else's is touched.
        let holder: i64 = sqlx::query("select holder_id from items where id = $1")
            .bind(item)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?
            .try_get("holder_id")
            .map_err(internal)?;
        if holder != inv {
            return Err(EconError::Invalid(NOT_CARRIED.into()));
        }
        // Holder, trades, item: the lock order of a trade commit.
        leave_open_offers(&mut tx, item).await?;
        let r = sqlx::query("select holder_id, template from items where id = $1 for update")
            .bind(item)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?;
        let holder: i64 = r.try_get("holder_id").map_err(internal)?;
        if holder != inv {
            return Err(EconError::Invalid(NOT_CARRIED.into()));
        }
        let template: String = r.try_get("template").map_err(internal)?;
        let place = content
            .place(&template)
            .ok_or_else(|| EconError::Invalid("that cannot be worn".into()))?;
        fits(content, hands, &template, Some(place)).map_err(EconError::Invalid)?;
        sqlx::query("delete from worn where character_id = $1 and slot = $2")
            .bind(character)
            .bind(place.name())
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        sqlx::query("insert into worn (character_id, slot, item_id) values ($1, $2, $3)")
            .bind(character)
            .bind(place.name())
            .bind(item)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        self.gear(character, content).await
    }

    /// A character playing in `zone` takes an item off: it stays in the inventory. Taking
    /// off what is not worn changes nothing and is no error (the answer says what is worn
    /// now, which is what was asked for). An item whose template the content no longer
    /// knows comes off like any other.
    pub async fn take_off(
        &self,
        character: i64,
        zone: &str,
        item: i64,
        content: &ItemContent,
    ) -> Result<Reading, EconError> {
        let mut tx = self.begin().await?;
        playing_in(&mut tx, character, zone).await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        sqlx::query("delete from worn where character_id = $1 and item_id = $2")
            .bind(character)
            .bind(item)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        self.gear(character, content).await
    }

    /// A reading of what the character's worn items do to damage (ITEMS.md 3.2), as its
    /// zone applies it: the weapon's edges are what it deals more of, the armour's what it
    /// takes less of. A character that does not exist, or wears nothing, has none.
    ///
    /// The number orders readings (ITEMS.md 3.3). It is drawn **before** the rows are
    /// read, and each statement sees what was committed before it began: so the reading a
    /// change makes of itself (after its commit) has a larger number than any reading that
    /// could have missed it. A zone that keeps, for each character, the reading with the
    /// largest number holds what the hub holds, in whatever order the answers arrive.
    pub async fn gear(&self, character: i64, content: &ItemContent) -> Result<Reading, EconError> {
        let mut conn = self.pool.acquire().await.map_err(internal)?;
        let seq: i64 = sqlx::query("select nextval('gear_seq') as seq")
            .fetch_one(&mut *conn)
            .await
            .map_err(internal)?
            .try_get("seq")
            .map_err(internal)?;
        let mut tx = sqlx::Acquire::begin(&mut *conn).await.map_err(internal)?;
        let (gear, templates) = gear_in(&mut tx, character, content).await?;
        let stacks = stacks_in(&mut tx, character, &self.stacks).await?;
        let bar = bar_in(&mut tx, character, &stacks).await?;
        tx.commit().await.map_err(internal)?;
        Ok((seq as u64, gear, templates, stacks, bar))
    }

    /// The item bar (LOOK.md 3.2, ITEMS.md 4): the template on each of the four cells as
    /// the character arranged it; the default while it never has (the first stack it
    /// carries that heals, on the first cell: the kit under `F`).
    pub async fn bar(&self, character: i64) -> Result<Vec<Option<String>>, EconError> {
        let mut conn = self.pool.acquire().await.map_err(internal)?;
        let mut tx = sqlx::Acquire::begin(&mut *conn).await.map_err(internal)?;
        let stacks = stacks_in(&mut tx, character, &self.stacks).await?;
        let bar = bar_in(&mut tx, character, &stacks).await?;
        tx.commit().await.map_err(internal)?;
        Ok(bar)
    }

    /// Arrange the item bar: four cells, each a stack template the content knows or
    /// nothing. The arrangement is kept whether or not the character carries the stacks
    /// (a cell stands empty until it does again); a template that is not a stack is
    /// refused in words.
    pub async fn set_bar(&self, character: i64, cells: &[Option<String>]) -> Result<(), EconError> {
        if cells.len() != BAR_CELLS {
            return Err(EconError::Invalid(format!("a bar has {BAR_CELLS} cells")));
        }
        for (i, t) in cells.iter().enumerate() {
            let Some(t) = t else { continue };
            if !self.stacks.contains_key(t) {
                return Err(EconError::Invalid(format!("{t:?} is not a stack")));
            }
            if cells[..i].iter().any(|c| c.as_ref() == Some(t)) {
                return Err(EconError::Invalid(format!("{t:?} twice on the bar")));
            }
        }
        let exists: Option<i64> = sqlx::query("select id from characters where id = $1")
            .bind(character)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .map(|r| r.try_get("id"))
            .transpose()
            .map_err(internal)?;
        if exists.is_none() {
            return Err(EconError::NotFound);
        }
        sqlx::query(
            "insert into bars (character_id, cell_1, cell_2, cell_3, cell_4) values ($1, $2, $3, $4, $5) \
             on conflict (character_id) do update set cell_1 = excluded.cell_1, cell_2 = excluded.cell_2, \
             cell_3 = excluded.cell_3, cell_4 = excluded.cell_4",
        )
        .bind(character)
        .bind(&cells[0])
        .bind(&cells[1])
        .bind(&cells[2])
        .bind(&cells[3])
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    /// Money supply (ECONOMY.md 1.2).
    pub async fn supply(&self) -> Result<Supply, EconError> {
        let r = sqlx::query(
            "select (select coalesce(sum(coin), 0) from holders where kind not in ('source', 'sink'))::bigint as circulating, \
             (select coalesce(sum(amount), 0) from coin_ledger where from_holder = \
              (select id from holders where kind = 'source'))::bigint as created, \
             (select coalesce(sum(amount), 0) from coin_ledger where to_holder = \
              (select id from holders where kind = 'sink'))::bigint as burned",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        Ok(Supply {
            circulating: r.try_get("circulating").map_err(internal)?,
            created: r.try_get("created").map_err(internal)?,
            burned: r.try_get("burned").map_err(internal)?,
        })
    }

    /// The ledger invariant: created = circulating + burned, and every holder's balance
    /// equals what the ledger says it received minus what it sent. Returns the number of
    /// holders whose balance disagrees with the ledger (0 = sound).
    pub async fn audit(&self) -> Result<i64, EconError> {
        let s = self.supply().await?;
        if s.created != s.circulating + s.burned {
            return Ok(-1);
        }
        let n: i64 = sqlx::query(
            "select count(*) from holders h where h.kind not in ('source', 'sink') and h.coin <> \
             coalesce((select sum(amount) from coin_ledger where to_holder = h.id), 0) - \
             coalesce((select sum(amount) from coin_ledger where from_holder = h.id), 0)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?
        .try_get(0)
        .map_err(internal)?;
        // What is worn is in its wearer's own inventory, and in no stall and no offer.
        let astray: i64 = sqlx::query(
            "select count(*) from worn w join items i on i.id = w.item_id where \
             i.holder_id is distinct from (select h.id from holders h where h.kind = 'character' \
             and h.character_id = w.character_id) \
             or exists (select 1 from listings l where l.item_id = w.item_id) \
             or exists (select 1 from trade_items t join trades tr on tr.id = t.trade_id \
             where t.item_id = w.item_id and tr.state = 'open')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?
        .try_get(0)
        .map_err(internal)?;
        Ok(n + astray)
    }

    // ---------- drops (zone-reported) ----------

    /// Create components for the members a boss split named (ECONOMY.md 9). A full inventory
    /// sends the component to the zone's ground, never to nowhere. Returns the item ids.
    pub async fn grant_components(
        &self,
        zone: &str,
        grants: &[(i64, String)],
        reference: i64,
    ) -> Result<Vec<i64>, EconError> {
        let mut tx = self.begin().await?;
        let source = singleton(&mut tx, "source").await?;
        let mut ids = Vec::with_capacity(grants.len());
        let mut holders = Vec::with_capacity(grants.len());
        for (character, _) in grants {
            holders.push(character_holder(&mut tx, *character).await?);
        }
        lock_holders(&mut tx, &holders).await?;
        for ((_, material), &holder) in grants.iter().zip(&holders) {
            let target = if has_room(&mut tx, holder, 1).await? {
                holder
            } else {
                ground_holder(&mut tx, zone).await?
            };
            let id = create_component(&mut tx, target, material).await?;
            sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, $3, 'drop', $4)")
                .bind(id)
                .bind(source)
                .bind(target)
                .bind(reference)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            ids.push(id);
        }
        tx.commit().await.map_err(internal)?;
        Ok(ids)
    }

    /// Everything one kill gives, as one transaction that happens once (ECONOMY.md 9):
    /// `components` are `(recipient, material)`, a recipient of `None` being the zone's
    /// ground (a recipient who has left); `coin` is `(character, silver)`. Returns the item
    /// ids, or `None` when this kill of this zone was paid before: the report was a repeat.
    pub async fn grant_kill(
        &self,
        zone: &str,
        reference: i64,
        components: &[(Option<i64>, String)],
        coin: &[(i64, i64)],
    ) -> Result<Option<Vec<i64>>, EconError> {
        if coin
            .iter()
            .any(|(_, amount)| *amount <= 0 || *amount > MAX_PRICE)
        {
            return Err(EconError::Invalid("amount".into()));
        }
        let mut tx = self.begin().await?;
        let claimed = sqlx::query(
            "insert into kills (zone, ref) values ($1, $2) on conflict do nothing returning ref",
        )
        .bind(zone)
        .bind(reference)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
        if claimed.is_none() {
            return Ok(None);
        }
        let source = singleton(&mut tx, "source").await?;
        let mut targets = Vec::with_capacity(components.len());
        let mut holders = Vec::new();
        for (character, _) in components {
            let holder = match character {
                Some(c) => Some(character_holder(&mut tx, *c).await?),
                None => None,
            };
            holders.extend(holder);
            targets.push(holder);
        }
        let mut purses = Vec::with_capacity(coin.len());
        for (character, _) in coin {
            let holder = character_holder(&mut tx, *character).await?;
            holders.push(holder);
            purses.push(holder);
        }
        lock_holders(&mut tx, &holders).await?;
        let mut ids = Vec::with_capacity(components.len());
        for ((_, material), holder) in components.iter().zip(&targets) {
            let target = match holder {
                Some(h) if has_room(&mut tx, *h, 1).await? => *h,
                _ => ground_holder(&mut tx, zone).await?,
            };
            let id = create_component(&mut tx, target, material).await?;
            sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, $3, 'drop', $4)")
                .bind(id)
                .bind(source)
                .bind(target)
                .bind(reference)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            ids.push(id);
        }
        for ((_, amount), holder) in coin.iter().zip(&purses) {
            move_coin(&mut tx, source, *holder, *amount, "drop", reference).await?;
        }
        tx.commit().await.map_err(internal)?;
        Ok(Some(ids))
    }

    /// A coin drop: source → character.
    pub async fn grant_coin(
        &self,
        character: i64,
        amount: i64,
        reference: i64,
    ) -> Result<(), EconError> {
        self.grant_coin_as(character, amount, "drop", reference)
            .await
    }

    /// An operator's hand (ITEMS.md 4): a made item appears in a character's inventory,
    /// out of the source like a drop and under the reason `grant` in the log. What it is
    /// made of obeys a craft's rule; a full inventory refuses (nothing goes to the ground).
    pub async fn grant_item(
        &self,
        character: i64,
        template: &str,
        materials: &[String],
        layers: Option<&[String]>,
    ) -> Result<i64, EconError> {
        if template.is_empty() || template == "component" || template.len() > 48 {
            return Err(EconError::Invalid("template".into()));
        }
        let mut parts = Vec::with_capacity(materials.len());
        for m in materials {
            parts.push(Component {
                layer: material_layer(m)?.to_string(),
                material: m.clone(),
            });
        }
        check_parts(&parts, template, layers)?;
        let mut tx = self.begin().await?;
        let source = singleton(&mut tx, "source").await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        if !has_room(&mut tx, inv, 1).await? {
            return Err(EconError::Full);
        }
        let id = create_item(&mut tx, inv, template, &parts).await?;
        sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, $3, 'grant', 0)")
            .bind(id)
            .bind(source)
            .bind(inv)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// An operator hands a character `quantity` of a stack (MODES.md 11.4): onto the stack
    /// it carries, up to the cap, or a new row within it; refused in words at the cap, and
    /// for a full inventory. Out of the source under `grant`, as every grant.
    pub async fn grant_stack(
        &self,
        character: i64,
        template: &str,
        quantity: u32,
    ) -> Result<i64, EconError> {
        let (cap, _) = *self
            .stacks
            .get(template)
            .ok_or_else(|| EconError::Invalid(format!("{template:?} is not a stack")))?;
        if quantity == 0 {
            return Err(EconError::Invalid("a quantity is more than nothing".into()));
        }
        let mut tx = self.begin().await?;
        let source = singleton(&mut tx, "source").await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        let there = sqlx::query(
            "select id, quantity from items where holder_id = $1 and template = $2 order by id for update",
        )
        .bind(inv)
        .bind(template)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
        let id = match there {
            Some(r) => {
                let id: i64 = r.try_get("id").map_err(internal)?;
                let have: i32 = r.try_get("quantity").map_err(internal)?;
                if have.max(0) as u32 + quantity > cap {
                    return Err(EconError::Invalid(at_the_cap(template)));
                }
                sqlx::query("update items set quantity = quantity + $2 where id = $1")
                    .bind(id)
                    .bind(quantity as i32)
                    .execute(&mut *tx)
                    .await
                    .map_err(internal)?;
                id
            }
            None => {
                if quantity > cap {
                    return Err(EconError::Invalid(at_the_cap(template)));
                }
                if !has_room(&mut tx, inv, 1).await? {
                    return Err(EconError::Full);
                }
                create_item_n(&mut tx, inv, template, &[], quantity).await?
            }
        };
        sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, $3, 'grant', $4)")
            .bind(id)
            .bind(source)
            .bind(inv)
            .bind(quantity as i64)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// A character spent `quantity` of a stack it carries (MODES.md 11.2: a reload's
    /// rounds, a kit used), as its zone says: the stack is that much smaller, gone at
    /// nothing, under `consume` in the item log (the one item sink beside decomposition).
    /// Refused when the stack is not the character's or holds less: the zone then takes
    /// the reading it is answered.
    pub async fn consume(&self, character: i64, item: i64, quantity: u32) -> Result<(), EconError> {
        if quantity == 0 {
            return Ok(());
        }
        let mut tx = self.begin().await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        let r =
            sqlx::query("select holder_id, template, quantity from items where id = $1 for update")
                .bind(item)
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal)?
                .ok_or(EconError::NotFound)?;
        let holder: i64 = r.try_get("holder_id").map_err(internal)?;
        let template: String = r.try_get("template").map_err(internal)?;
        let have: i32 = r.try_get("quantity").map_err(internal)?;
        if holder != inv {
            return Err(EconError::Forbidden);
        }
        if !self.stacks.contains_key(&template) {
            return Err(EconError::Invalid(format!("{template:?} is not a stack")));
        }
        if (have.max(0) as u32) < quantity {
            return Err(EconError::State(format!(
                "only {have} of the {template}s are carried"
            )));
        }
        sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, null, 'consume', $3)")
            .bind(item)
            .bind(inv)
            .bind(quantity as i64)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        if have.max(0) as u32 == quantity {
            sqlx::query("delete from items where id = $1")
                .bind(item)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        } else {
            sqlx::query("update items set quantity = quantity - $2 where id = $1")
                .bind(item)
                .bind(quantity as i32)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        tx.commit().await.map_err(internal)
    }

    /// The same under another reason in the ledger (`grant`: an operator's hand).
    pub async fn grant_coin_as(
        &self,
        character: i64,
        amount: i64,
        reason: &str,
        reference: i64,
    ) -> Result<(), EconError> {
        if amount <= 0 || amount > MAX_PRICE {
            return Err(EconError::Invalid("amount".into()));
        }
        let mut tx = self.begin().await?;
        let source = singleton(&mut tx, "source").await?;
        let holder = character_holder(&mut tx, character).await?;
        move_coin(&mut tx, source, holder, amount, reason, reference).await?;
        tx.commit().await.map_err(internal)
    }

    // ---------- storage, ground, guild chests ----------

    pub async fn storage_deposit(&self, character: i64, item: i64) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let inv = character_holder(&mut tx, character).await?;
        let sto = storage_holder(&mut tx, character).await?;
        self.mv(&mut tx, item, inv, sto, "deposit", 0).await?;
        tx.commit().await.map_err(internal)
    }

    pub async fn storage_withdraw(&self, character: i64, item: i64) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let inv = character_holder(&mut tx, character).await?;
        let sto = storage_holder(&mut tx, character).await?;
        self.mv(&mut tx, item, sto, inv, "withdraw", 0).await?;
        tx.commit().await.map_err(internal)
    }

    /// Drop an item on the ground of a zone; it persists there (PLAN.md 5.1).
    pub async fn drop_item(&self, character: i64, item: i64, zone: &str) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let inv = character_holder(&mut tx, character).await?;
        let ground = ground_holder(&mut tx, zone).await?;
        self.mv(&mut tx, item, inv, ground, "ground", 0).await?;
        tx.commit().await.map_err(internal)
    }

    pub async fn pickup(&self, character: i64, item: i64, zone: &str) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let inv = character_holder(&mut tx, character).await?;
        let ground = ground_holder(&mut tx, zone).await?;
        self.mv(&mut tx, item, ground, inv, "pickup", 0).await?;
        tx.commit().await.map_err(internal)
    }

    pub async fn guild_create(&self, name: &str, founder: i64) -> Result<i64, EconError> {
        let name = name.trim();
        if name.is_empty() || name.len() > 24 {
            return Err(EconError::Invalid("guild name".into()));
        }
        let mut tx = self.begin().await?;
        let id: i64 = sqlx::query("insert into guilds (name) values ($1) returning id")
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?
            .try_get("id")
            .map_err(internal)?;
        sqlx::query("insert into guild_members (guild_id, character_id, rank) values ($1, $2, 9)")
            .bind(id)
            .bind(founder)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    pub async fn guild_add(&self, guild: i64, character: i64, rank: i16) -> Result<(), EconError> {
        sqlx::query("insert into guild_members (guild_id, character_id, rank) values ($1, $2, $3)")
            .bind(guild)
            .bind(character)
            .bind(rank.clamp(0, 9))
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(())
    }

    /// A chest in the guild hall; `min_rank` 0 is the free-for-all chest (PLAN.md 5.7).
    pub async fn chest_create(
        &self,
        guild: i64,
        min_rank: i16,
        capacity: i32,
    ) -> Result<i64, EconError> {
        sqlx::query("insert into holders (kind, guild_id, min_rank, capacity) values ('guild_chest', $1, $2, $3) returning id")
            .bind(guild)
            .bind(min_rank.clamp(0, 9))
            .bind(if capacity <= 0 { CHEST_SLOTS } else { capacity.min(200) })
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?
            .try_get("id")
            .map_err(internal)
    }

    async fn chest_access(
        tx: &mut Tx<'_>,
        chest: i64,
        character: i64,
    ) -> Result<(i16, i16), EconError> {
        let r = sqlx::query(
            "select h.min_rank, m.rank from holders h join guild_members m on m.guild_id = h.guild_id \
             where h.id = $1 and h.kind = 'guild_chest' and m.character_id = $2",
        )
        .bind(chest)
        .bind(character)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::Forbidden)?;
        Ok((
            r.try_get("min_rank").map_err(internal)?,
            r.try_get("rank").map_err(internal)?,
        ))
    }

    pub async fn chest_deposit(
        &self,
        character: i64,
        chest: i64,
        item: i64,
    ) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        Self::chest_access(&mut tx, chest, character).await?;
        let inv = character_holder(&mut tx, character).await?;
        self.mv(&mut tx, item, inv, chest, "deposit", 0).await?;
        tx.commit().await.map_err(internal)
    }

    pub async fn chest_withdraw(
        &self,
        character: i64,
        chest: i64,
        item: i64,
    ) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let (min_rank, rank) = Self::chest_access(&mut tx, chest, character).await?;
        if rank < min_rank {
            return Err(EconError::Forbidden);
        }
        let inv = character_holder(&mut tx, character).await?;
        self.mv(&mut tx, item, chest, inv, "withdraw", 0).await?;
        tx.commit().await.map_err(internal)
    }

    // ---------- crafting ----------

    /// Combine component items into one item (ECONOMY.md 10): core and frame mandatory, at
    /// most one shard and one catalyst, at most two gems. The components are consumed.
    ///
    /// `layers` are the layers the template has room for, when the content knows it: a
    /// part of any other layer is refused (a cuirass takes no catalyst).
    pub async fn craft(
        &self,
        character: i64,
        template: &str,
        components: &[i64],
        layers: Option<&[String]>,
    ) -> Result<i64, EconError> {
        if template.is_empty() || template == "component" || template.len() > 48 {
            return Err(EconError::Invalid("template".into()));
        }
        // No craft takes more parts than there are places for them: asked for more, the
        // hub does nothing at all (each id below costs it statements before it is looked at).
        if components.len() > MAX_PARTS {
            return Err(EconError::Invalid("too many components".into()));
        }
        let mut ids = components.to_vec();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != components.len() || ids.is_empty() {
            return Err(EconError::Invalid("components".into()));
        }
        let mut tx = self.begin().await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        let mut parts: Vec<Component> = Vec::new();
        // Trades before items (the lock order of a trade commit).
        for &id in &ids {
            detach_from_trades(&mut tx, id).await?;
        }
        for &id in &ids {
            let r = sqlx::query("select holder_id, template from items where id = $1 for update")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal)?
                .ok_or(EconError::NotFound)?;
            let holder: i64 = r.try_get("holder_id").map_err(internal)?;
            let tpl: String = r.try_get("template").map_err(internal)?;
            if holder != inv {
                return Err(EconError::Forbidden);
            }
            if tpl != "component" {
                return Err(EconError::Invalid(
                    "only components can be crafted with".into(),
                ));
            }
            parts.extend(components_of(&mut tx, id).await?);
        }
        check_parts(&parts, template, layers)?;
        let new_id = create_item(&mut tx, inv, template, &parts).await?;
        for &id in &ids {
            sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, null, 'craft', $3)")
                .bind(id)
                .bind(inv)
                .bind(new_id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            sqlx::query("delete from items where id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, null, $2, 'craft', 0)")
            .bind(new_id)
            .bind(inv)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(new_id)
    }

    /// Decompose an item (ECONOMY.md 10): ⌊k / 2⌋ components come back, chosen by
    /// `salvage_indices`; the rest are destroyed.
    pub async fn decompose(&self, character: i64, item: i64) -> Result<Vec<i64>, EconError> {
        let mut tx = self.begin().await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        // Trades before items (the lock order of a trade commit); undone if the checks fail.
        detach_from_trades(&mut tx, item).await?;
        let r = sqlx::query("select holder_id, template from items where id = $1 for update")
            .bind(item)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?;
        let holder: i64 = r.try_get("holder_id").map_err(internal)?;
        let tpl: String = r.try_get("template").map_err(internal)?;
        if holder != inv {
            return Err(EconError::Forbidden);
        }
        if is_worn(&mut tx, item).await? {
            return Err(EconError::State(WORN.into()));
        }
        let parts = components_of(&mut tx, item).await?;
        if tpl == "component" || parts.len() < 2 {
            return Err(EconError::Invalid("nothing to decompose".into()));
        }
        let kept: Vec<&Component> = salvage_indices(item, parts.len())
            .into_iter()
            .map(|i| &parts[i])
            .collect();
        // The item goes, the kept components arrive: net slots needed.
        if !has_room(&mut tx, inv, kept.len() as i64 - 1).await? {
            return Err(EconError::Full);
        }
        sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, null, 'decompose', 0)")
            .bind(item)
            .bind(inv)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        sqlx::query("delete from items where id = $1")
            .bind(item)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        let mut out = Vec::with_capacity(kept.len());
        for c in kept {
            let id = create_component(&mut tx, inv, &c.material).await?;
            sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, null, $2, 'decompose', $3)")
                .bind(id)
                .bind(inv)
                .bind(item)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            out.push(id);
        }
        tx.commit().await.map_err(internal)?;
        Ok(out)
    }

    // ---------- trade window (ECONOMY.md 6) ----------

    /// Two characters that play in `zone` open a trade (PARTY.md 6): the zone saw them
    /// stand together and both ask. A character has one open trade: whatever either
    /// still had open is called off.
    pub async fn trade_open_in(&self, a: i64, b: i64, zone: &str) -> Result<i64, EconError> {
        if a == b {
            return Err(EconError::Invalid("a trade needs two characters".into()));
        }
        let mut tx = self.begin().await?;
        // Characters first, ascending, as everywhere; whole, so that two openings for one
        // character come one after the other and each calls the other's off.
        let here: Vec<i64> = sqlx::query(
            "select id from characters where id in ($1, $2) and location_kind = 'zone' \
             and location_zone = $3 order by id for update",
        )
        .bind(a)
        .bind(b)
        .bind(zone)
        .fetch_all(&mut *tx)
        .await
        .map_err(internal)?
        .iter()
        .map(|r| r.try_get("id").map_err(internal))
        .collect::<Result<_, _>>()?;
        if here.len() != 2 {
            return Err(EconError::Forbidden);
        }
        character_holder(&mut tx, a).await?;
        character_holder(&mut tx, b).await?;
        sqlx::query(
            "update trades set state = 'cancelled' where state = 'open' \
             and (a_character in ($1, $2) or b_character in ($1, $2))",
        )
        .bind(a)
        .bind(b)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        let id: i64 = sqlx::query(
            "insert into trades (a_character, b_character) values ($1, $2) returning id",
        )
        .bind(a)
        .bind(b)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// A trade between two characters wherever they are (tests of the window itself; a
    /// player's trade is opened by its zone, `trade_open_in`).
    pub async fn trade_open(&self, a: i64, b: i64) -> Result<i64, EconError> {
        if a == b {
            return Err(EconError::Invalid("a trade needs two characters".into()));
        }
        let mut tx = self.begin().await?;
        character_holder(&mut tx, a).await?;
        character_holder(&mut tx, b).await?;
        let id: i64 = sqlx::query(
            "insert into trades (a_character, b_character) values ($1, $2) returning id",
        )
        .bind(a)
        .bind(b)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// Lock an open trade and say which side `character` is.
    async fn trade_side(
        tx: &mut Tx<'_>,
        trade: i64,
        character: i64,
    ) -> Result<(char, i64, i64), EconError> {
        let r = sqlx::query(
            "select a_character, b_character, state from trades where id = $1 for update",
        )
        .bind(trade)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let state: String = r.try_get("state").map_err(internal)?;
        if state != "open" {
            return Err(EconError::State(state));
        }
        let a: i64 = r.try_get("a_character").map_err(internal)?;
        let b: i64 = r.try_get("b_character").map_err(internal)?;
        if character == a {
            Ok(('a', a, b))
        } else if character == b {
            Ok(('b', a, b))
        } else {
            Err(EconError::Forbidden)
        }
    }

    /// The mutation lock: any change clears both accepts and restarts the cooldown.
    async fn trade_touched(tx: &mut Tx<'_>, trade: i64) -> Result<(), EconError> {
        sqlx::query("update trades set a_accepted = false, b_accepted = false, changed_at = now(), version = version + 1 where id = $1")
            .bind(trade)
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
        Ok(())
    }

    pub async fn trade_offer_item(
        &self,
        trade: i64,
        character: i64,
        item: i64,
    ) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        // The holder first (then the trade row, then the item: one lock order everywhere):
        // putting the item on and offering it cannot both go through.
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[inv]).await?;
        let (side, _, _) = Self::trade_side(&mut tx, trade, character).await?;
        let holder: i64 = sqlx::query("select holder_id from items where id = $1")
            .bind(item)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?
            .try_get("holder_id")
            .map_err(internal)?;
        if holder != inv {
            return Err(EconError::Forbidden);
        }
        if is_worn(&mut tx, item).await? {
            return Err(EconError::State(WORN.into()));
        }
        sqlx::query("insert into trade_items (trade_id, side, item_id) values ($1, $2, $3)")
            .bind(trade)
            .bind(side.to_string())
            .bind(item)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        Self::trade_touched(&mut tx, trade).await?;
        tx.commit().await.map_err(internal)
    }

    pub async fn trade_retract_item(
        &self,
        trade: i64,
        character: i64,
        item: i64,
    ) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let (side, _, _) = Self::trade_side(&mut tx, trade, character).await?;
        let n = sqlx::query(
            "delete from trade_items where trade_id = $1 and item_id = $2 and side = $3",
        )
        .bind(trade)
        .bind(item)
        .bind(side.to_string())
        .execute(&mut *tx)
        .await
        .map_err(internal)?
        .rows_affected();
        if n == 0 {
            return Err(EconError::NotFound);
        }
        Self::trade_touched(&mut tx, trade).await?;
        tx.commit().await.map_err(internal)
    }

    pub async fn trade_set_coin(
        &self,
        trade: i64,
        character: i64,
        coin: i64,
    ) -> Result<(), EconError> {
        if !(0..=MAX_PRICE).contains(&coin) {
            return Err(EconError::Invalid("coin".into()));
        }
        let mut tx = self.begin().await?;
        let (side, _, _) = Self::trade_side(&mut tx, trade, character).await?;
        let q = if side == 'a' {
            "update trades set a_coin = $2 where id = $1"
        } else {
            "update trades set b_coin = $2 where id = $1"
        };
        sqlx::query(q)
            .bind(trade)
            .bind(coin)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        Self::trade_touched(&mut tx, trade).await?;
        tx.commit().await.map_err(internal)
    }

    /// What a participant sees of a trade, whatever its state (PARTY.md 6).
    pub async fn trade_view(&self, trade: i64, character: i64) -> Result<TradeSeen, EconError> {
        // Three statements and no transaction: a window asks for this once a second
        // (PARTY.md 6), and read committed would give a transaction no one view anyway. An
        // offer that changes between the statements is seen with a version the accept of
        // which is refused ("the offer changed").
        let r = sqlx::query(
            "select t.a_character, t.b_character, t.a_coin, t.b_coin, t.a_accepted, t.b_accepted, \
             t.version, t.state, extract(epoch from (now() - t.changed_at))::float8 as age, \
             ca.name as a_name, cb.name as b_name, \
             (ca.location_kind = 'zone' and cb.location_kind = 'zone' \
              and ca.location_zone = cb.location_zone) as together \
             from trades t join characters ca on ca.id = t.a_character \
             join characters cb on cb.id = t.b_character where t.id = $1",
        )
        .bind(trade)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let a: i64 = r.try_get("a_character").map_err(internal)?;
        let b: i64 = r.try_get("b_character").map_err(internal)?;
        if character != a && character != b {
            return Err(EconError::Forbidden);
        }
        // Both offers' items, then every component of all of them, in one statement each.
        let rows = sqlx::query(
            "select t.side, i.id, i.template, i.quantity from trade_items t join items i on i.id = t.item_id \
             where t.trade_id = $1 order by t.side, i.id",
        )
        .bind(trade)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let mut offered: Vec<(String, Item)> = Vec::with_capacity(rows.len());
        for row in rows {
            offered.push((
                row.try_get("side").map_err(internal)?,
                Item {
                    id: row.try_get("id").map_err(internal)?,
                    template: row.try_get("template").map_err(internal)?,
                    components: Vec::new(),
                    // A worn item is in no offer: wearing it took it out of every one.
                    worn: false,
                    quantity: row.try_get::<i32, _>("quantity").map_err(internal)?.max(0) as u32,
                },
            ));
        }
        let ids: Vec<i64> = offered.iter().map(|(_, i)| i.id).collect();
        let parts = sqlx::query(
            "select item_id, layer, material from item_components where item_id = any($1) \
             order by item_id, array_position(array['shard','core','catalyst','frame','gem'], layer), position",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        for part in parts {
            let item: i64 = part.try_get("item_id").map_err(internal)?;
            if let Some((_, i)) = offered.iter_mut().find(|(_, i)| i.id == item) {
                i.components.push(Component {
                    layer: part.try_get("layer").map_err(internal)?,
                    material: part.try_get("material").map_err(internal)?,
                });
            }
        }
        let mut sides = Vec::new();
        for side in ["a", "b"] {
            let items: Vec<Item> = offered
                .iter()
                .filter(|(s, _)| s == side)
                .map(|(_, i)| i.clone())
                .collect();
            let coin: i64 = r
                .try_get(format!("{side}_coin").as_str())
                .map_err(internal)?;
            let accepted: bool = r
                .try_get(format!("{side}_accepted").as_str())
                .map_err(internal)?;
            sides.push((coin, accepted, items));
        }
        let age: f64 = r.try_get("age").map_err(internal)?;
        let wait = (self.trade_cooldown.as_secs_f64() - age).max(0.0);
        let state: String = r.try_get("state").map_err(internal)?;
        let together: Option<bool> = r.try_get("together").map_err(internal)?;
        let (a_name, b_name): (String, String) = (
            r.try_get("a_name").map_err(internal)?,
            r.try_get("b_name").map_err(internal)?,
        );
        let theirs = sides.pop().unwrap();
        let mine = sides.pop().unwrap();
        let (mine, theirs, with) = if character == a {
            (mine, theirs, b_name)
        } else {
            (theirs, mine, a_name)
        };
        Ok(TradeSeen {
            state: match state.as_str() {
                "open" => TradeState::Open,
                "committed" => TradeState::Committed,
                _ => TradeState::Cancelled,
            },
            version: r.try_get("version").map_err(internal)?,
            wait_ms: (wait * 1000.0).ceil() as u32,
            with,
            together: together.unwrap_or(false),
            mine,
            theirs,
        })
    }

    /// A character is somewhere else now (a zone claimed it): whatever trade it had open
    /// is called off (PARTY.md 6). Returns how many.
    pub async fn trades_end_of(&self, character: i64) -> Result<u64, EconError> {
        Ok(sqlx::query(
            "update trades set state = 'cancelled' where state = 'open' \
             and (a_character = $1 or b_character = $1)",
        )
        .bind(character)
        .execute(&self.pool)
        .await
        .map_err(internal)?
        .rows_affected())
    }

    /// Trades nobody has touched for `TRADE_IDLE_MINUTES` are called off: none waits,
    /// accepted on one side, for a day when its two meet again. Returns how many.
    pub async fn trades_expire(&self) -> Result<u64, EconError> {
        Ok(sqlx::query(
            "update trades set state = 'cancelled' where state = 'open' \
             and changed_at < now() - make_interval(mins => $1)",
        )
        .bind(TRADE_IDLE_MINUTES)
        .execute(&self.pool)
        .await
        .map_err(internal)?
        .rows_affected())
    }

    /// The offer version to show and to accept.
    /// The two characters of a trade (MODES.md 11.2: both zones are told what moved).
    pub async fn trade_characters(&self, trade: i64) -> Result<(i64, i64), EconError> {
        let r = sqlx::query("select a_character, b_character from trades where id = $1")
            .bind(trade)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?;
        Ok((
            r.try_get("a_character").map_err(internal)?,
            r.try_get("b_character").map_err(internal)?,
        ))
    }

    pub async fn trade_version(&self, trade: i64) -> Result<i32, EconError> {
        sqlx::query("select version from trades where id = $1")
            .bind(trade)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?
            .try_get("version")
            .map_err(internal)
    }

    pub async fn trade_cancel(&self, trade: i64, character: i64) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        Self::trade_side(&mut tx, trade, character).await?;
        sqlx::query("update trades set state = 'cancelled' where id = $1")
            .bind(trade)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)
    }

    /// Accept the offers as they stand. Refused within the cooldown after any change. When
    /// both sides have accepted, the swap commits in this transaction or not at all; a swap
    /// that no longer verifies clears both accepts and reports why.
    ///
    /// `version` is the offer version the client was showing: a client that has not yet seen
    /// the latest change cannot accept it blind.
    pub async fn trade_accept(
        &self,
        trade: i64,
        character: i64,
        version: i32,
    ) -> Result<TradeStatus, EconError> {
        let mut tx = self.begin().await?;
        // One lock order everywhere: the characters, their holders, the trade row, items.
        // (Read without a lock here: whose it is and whether it is open; a stranger is
        // told nothing more than Forbidden, and changes nothing.)
        let who = sqlx::query("select a_character, b_character, state from trades where id = $1")
            .bind(trade)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?;
        let (first, second): (i64, i64) = (
            who.try_get("a_character").map_err(internal)?,
            who.try_get("b_character").map_err(internal)?,
        );
        if character != first && character != second {
            return Err(EconError::Forbidden);
        }
        let state: String = who.try_get("state").map_err(internal)?;
        if state != "open" {
            return Err(EconError::State(state));
        }
        // A trade is between two characters in one zone (ECONOMY.md 6): held against a
        // change of where either is until this is decided.
        let places = sqlx::query(
            "select location_kind, location_zone from characters where id in ($1, $2) \
             order by id for share",
        )
        .bind(first)
        .bind(second)
        .fetch_all(&mut *tx)
        .await
        .map_err(internal)?;
        let place = |i: usize| -> Result<Option<String>, EconError> {
            let kind: String = places[i].try_get("location_kind").map_err(internal)?;
            let zone: Option<String> = places[i].try_get("location_zone").map_err(internal)?;
            Ok(zone.filter(|_| kind == "zone"))
        };
        let together = places.len() == 2 && place(0)?.is_some() && place(0)? == place(1)?;
        if self.trades_need_a_zone && !together {
            sqlx::query("update trades set state = 'cancelled' where id = $1 and state = 'open'")
                .bind(trade)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            tx.commit().await.map_err(internal)?;
            return Err(EconError::State(NOT_HERE.into()));
        }
        let early_a = character_holder(&mut tx, first).await?;
        let early_b = character_holder(&mut tx, second).await?;
        lock_holders(&mut tx, &[early_a, early_b]).await?;
        let (side, a, b) = Self::trade_side(&mut tx, trade, character).await?;
        let r = sqlx::query(
            "select extract(epoch from (now() - changed_at))::float8 as age, a_accepted, b_accepted, a_coin, b_coin, \
             version from trades where id = $1",
        )
        .bind(trade)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        let current: i32 = r.try_get("version").map_err(internal)?;
        if current != version {
            return Err(EconError::State("the offer changed".into()));
        }
        let age: f64 = r.try_get("age").map_err(internal)?;
        if age < self.trade_cooldown.as_secs_f64() {
            return Err(EconError::Cooldown);
        }
        let mut a_ok: bool = r.try_get("a_accepted").map_err(internal)?;
        let mut b_ok: bool = r.try_get("b_accepted").map_err(internal)?;
        let a_coin: i64 = r.try_get("a_coin").map_err(internal)?;
        let b_coin: i64 = r.try_get("b_coin").map_err(internal)?;
        if side == 'a' {
            a_ok = true;
        } else {
            b_ok = true;
        }
        sqlx::query("update trades set a_accepted = $2, b_accepted = $3 where id = $1")
            .bind(trade)
            .bind(a_ok)
            .bind(b_ok)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        if !(a_ok && b_ok) {
            tx.commit().await.map_err(internal)?;
            return Ok(TradeStatus::Waiting);
        }
        // Both accepted: verify everything again, then move everything.
        let ha = character_holder(&mut tx, a).await?;
        let hb = character_holder(&mut tx, b).await?;
        let locked = lock_holders(&mut tx, &[ha, hb]).await?;
        let rows = sqlx::query(
            "select side, item_id from trade_items where trade_id = $1 order by item_id",
        )
        .bind(trade)
        .fetch_all(&mut *tx)
        .await
        .map_err(internal)?;
        let mut from_a = Vec::new();
        let mut from_b = Vec::new();
        for r in &rows {
            let s: String = r.try_get("side").map_err(internal)?;
            let id: i64 = r.try_get("item_id").map_err(internal)?;
            if s == "a" {
                from_a.push(id)
            } else {
                from_b.push(id)
            }
        }
        let mut problem: Option<EconError> = None;
        for (ids, holder) in [(&from_a, ha), (&from_b, hb)] {
            for &id in ids.iter() {
                let h: Option<i64> =
                    sqlx::query("select holder_id from items where id = $1 for update")
                        .bind(id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(internal)?
                        .map(|r| r.try_get("holder_id"))
                        .transpose()
                        .map_err(internal)?;
                if h != Some(holder) {
                    problem = Some(EconError::State(
                        "an offered item is no longer there".into(),
                    ));
                }
            }
        }
        let coin = |h: i64| locked.iter().find(|x| x.0 == h).map_or(0, |x| x.3);
        if coin(ha) < a_coin || coin(hb) < b_coin {
            problem = Some(EconError::Insufficient);
        }
        for (holder, incoming, outgoing) in [
            (ha, from_b.len(), from_a.len()),
            (hb, from_a.len(), from_b.len()),
        ] {
            if !has_room(&mut tx, holder, incoming as i64 - outgoing as i64).await? {
                problem = Some(EconError::Full);
            }
        }
        if let Some(e) = problem {
            Self::trade_touched(&mut tx, trade).await?;
            tx.commit().await.map_err(internal)?;
            return Err(e);
        }
        for (ids, from, to) in [(&from_a, ha, hb), (&from_b, hb, ha)] {
            for &id in ids.iter() {
                // A stack goes onto the other's stack, up to the cap (MODES.md 11.1); past
                // it the trade is refused whole, nothing of it done.
                let mut absorbed = false;
                if let Some(cap) = self
                    .stacks
                    .get(&row_template(&mut tx, id).await?)
                    .map(|s| s.0)
                {
                    let quantity = row_quantity(&mut tx, id).await?;
                    absorbed = absorb_stack(&mut tx, to, id, quantity, cap)
                        .await?
                        .is_some();
                }
                sqlx::query("insert into item_moves (item_id, from_holder, to_holder, reason, ref) values ($1, $2, $3, 'trade', $4)")
                    .bind(id)
                    .bind(from)
                    .bind(to)
                    .bind(trade)
                    .execute(&mut *tx)
                    .await
                    .map_err(internal)?;
                if absorbed {
                    unname_item(&mut tx, id).await?;
                    sqlx::query("delete from items where id = $1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await
                        .map_err(internal)?;
                } else {
                    sqlx::query("update items set holder_id = $2 where id = $1")
                        .bind(id)
                        .bind(to)
                        .execute(&mut *tx)
                        .await
                        .map_err(internal)?;
                }
            }
        }
        move_coin(&mut tx, ha, hb, a_coin, "trade", trade).await?;
        move_coin(&mut tx, hb, ha, b_coin, "trade", trade).await?;
        sqlx::query("update trades set state = 'committed' where id = $1")
            .bind(trade)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(TradeStatus::Committed)
    }

    // ---------- stalls (ECONOMY.md 7) ----------

    pub async fn stall_open(
        &self,
        character: i64,
        zone: &str,
        tile_x: i32,
        tile_y: i32,
    ) -> Result<i64, EconError> {
        let mut tx = self.begin().await?;
        character_holder(&mut tx, character).await?;
        // A closed stall may overflow the storage; no new stall until that is cleared, so a
        // stall is never extra storage.
        let sto = storage_holder(&mut tx, character).await?;
        if !has_room(&mut tx, sto, 0).await? {
            return Err(EconError::Full);
        }
        // One stall per character: asked first, so an owner asking again (a keeper after a
        // restart, a stall left in another zone) is refused without the database logging
        // a constraint violation for each ask.
        if sqlx::query("select 1 from stalls where owner_character = $1")
            .bind(character)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .is_some()
        {
            return Err(EconError::State("you already have a stall".into()));
        }
        let holder = new_holder(&mut tx, "stall", STALL_SLOTS).await?;
        let id: i64 = sqlx::query(
            "insert into stalls (owner_character, zone, tile_x, tile_y, expires, holder_id) \
             values ($1, $2, $3, $4, now() + make_interval(hours => $5), $6) returning id",
        )
        .bind(character)
        .bind(zone)
        .bind(tile_x)
        .bind(tile_y)
        .bind(STALL_HOURS as i32)
        .bind(holder)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// `(stall id, holder)` of the character's stall, with the stall row locked. With a
    /// zone named, the stall must stand in it.
    async fn stall_of_owner(
        tx: &mut Tx<'_>,
        character: i64,
        zone: Option<&str>,
    ) -> Result<(i64, i64), EconError> {
        let r = sqlx::query(
            "select id, holder_id, zone from stalls where owner_character = $1 for update",
        )
        .bind(character)
        .fetch_optional(&mut **tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let at: String = r.try_get("zone").map_err(internal)?;
        if zone.is_some_and(|z| z != at) {
            return Err(EconError::NotFound);
        }
        Ok((
            r.try_get("id").map_err(internal)?,
            r.try_get("holder_id").map_err(internal)?,
        ))
    }

    /// Take a listing out of the character's own stall in `zone`, back into its inventory
    /// (which must have room).
    pub async fn stall_unlist(
        &self,
        character: i64,
        zone: &str,
        listing: i64,
    ) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let (stall, holder) = Self::stall_of_owner(&mut tx, character, Some(zone)).await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[holder, inv]).await?;
        let item: i64 =
            sqlx::query("select item_id from listings where id = $1 and stall_id = $2 for update")
                .bind(listing)
                .bind(stall)
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal)?
                .ok_or(EconError::NotFound)?
                .try_get("item_id")
                .map_err(internal)?;
        self.mv(&mut tx, item, holder, inv, "withdraw", stall)
            .await?;
        sqlx::query("delete from listings where id = $1")
            .bind(listing)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)
    }

    /// Put an item of the inventory up for sale in the character's own stall, which
    /// stands in `zone` (the zone the character plays in: a stall is not a bag that is
    /// reached from the other end of the world).
    pub async fn stall_list(
        &self,
        character: i64,
        zone: &str,
        item: i64,
        price: i64,
    ) -> Result<i64, EconError> {
        check_price(price)?;
        let mut tx = self.begin().await?;
        let (stall, holder) = Self::stall_of_owner(&mut tx, character, Some(zone)).await?;
        let inv = character_holder(&mut tx, character).await?;
        self.mv(&mut tx, item, inv, holder, "deposit", stall)
            .await?;
        let id: i64 = sqlx::query(
            "insert into listings (stall_id, item_id, price) values ($1, $2, $3) returning id",
        )
        .bind(stall)
        .bind(item)
        .bind(price)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// What a stall has for sale and whose it is: `(owner, the owner's name, listings)`.
    /// From anywhere: looking is what a town board does (ITEMS.md 1).
    pub async fn stall_view(&self, stall: i64) -> Result<(i64, String, Vec<Listing>), EconError> {
        let mut tx = self.begin().await?;
        let r = sqlx::query(
            "select s.owner_character, c.name from stalls s \
             join characters c on c.id = s.owner_character where s.id = $1",
        )
        .bind(stall)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let rows = sqlx::query(
            "select l.id, l.price, l.item_id, i.template, i.quantity from listings l \
             join items i on i.id = l.item_id where l.stall_id = $1 order by l.id",
        )
        .bind(stall)
        .fetch_all(&mut *tx)
        .await
        .map_err(internal)?;
        let mut listings = Vec::with_capacity(rows.len());
        for row in rows {
            let item: i64 = row.try_get("item_id").map_err(internal)?;
            listings.push(Listing {
                id: row.try_get("id").map_err(internal)?,
                price: row.try_get("price").map_err(internal)?,
                item: Item {
                    id: item,
                    template: row.try_get("template").map_err(internal)?,
                    components: components_of(&mut tx, item).await?,
                    worn: false,
                    quantity: row.try_get::<i32, _>("quantity").map_err(internal)?.max(0) as u32,
                },
            });
        }
        tx.commit().await.map_err(internal)?;
        Ok((
            r.try_get("owner_character").map_err(internal)?,
            r.try_get("name").map_err(internal)?,
            listings,
        ))
    }

    /// Buy a listing of the stall the buyer stands at: coin to the owner and the item to
    /// the buyer in one transaction. The price the buyer saw must be the price paid, and
    /// the listing must be that stall's (the zone vouched for the place, not for the id).
    pub async fn stall_buy(
        &self,
        buyer: i64,
        zone: &str,
        stall: i64,
        listing: i64,
        expected_price: i64,
    ) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        // Holders first, then the listing row: the same order as a closing stall.
        let who = sqlx::query(
            "select s.id, s.zone, s.holder_id, s.owner_character from listings l join stalls s on s.id = l.stall_id where l.id = $1",
        )
        .bind(listing)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        if who.try_get::<i64, _>("id").map_err(internal)? != stall
            || who.try_get::<String, _>("zone").map_err(internal)? != zone
        {
            return Err(EconError::NotFound);
        }
        let owner: i64 = who.try_get("owner_character").map_err(internal)?;
        if owner == buyer {
            return Err(EconError::Invalid("that is your own stall".into()));
        }
        let buyer_h = character_holder(&mut tx, buyer).await?;
        let owner_h = character_holder(&mut tx, owner).await?;
        lock_holders(
            &mut tx,
            &[
                buyer_h,
                owner_h,
                who.try_get("holder_id").map_err(internal)?,
            ],
        )
        .await?;
        let r = sqlx::query(
            "select l.item_id, l.price, s.holder_id, s.id as stall_id from listings l \
             join stalls s on s.id = l.stall_id where l.id = $1 for update of l",
        )
        .bind(listing)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let item: i64 = r.try_get("item_id").map_err(internal)?;
        let price: i64 = r.try_get("price").map_err(internal)?;
        let stall_holder: i64 = r.try_get("holder_id").map_err(internal)?;
        let stall: i64 = r.try_get("stall_id").map_err(internal)?;
        if price != expected_price {
            return Err(EconError::State("the price changed".into()));
        }
        move_coin(&mut tx, buyer_h, owner_h, price, "stall_sale", stall).await?;
        self.mv(&mut tx, item, stall_holder, buyer_h, "stall_sale", stall)
            .await?;
        sqlx::query("delete from listings where id = $1")
            .bind(listing)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)
    }

    /// Post a buy order: the owner escrows `price × quantity` into the stall.
    pub async fn buy_order_post(
        &self,
        character: i64,
        material: &str,
        price: i64,
        quantity: i32,
    ) -> Result<i64, EconError> {
        check_price(price)?;
        material_layer(material)?;
        if !(1..=1000).contains(&quantity) {
            return Err(EconError::Invalid("quantity".into()));
        }
        let total = price
            .checked_mul(quantity as i64)
            .ok_or_else(|| EconError::Invalid("order too large".into()))?;
        let mut tx = self.begin().await?;
        let (stall, holder) = Self::stall_of_owner(&mut tx, character, None).await?;
        let inv = character_holder(&mut tx, character).await?;
        move_coin(&mut tx, inv, holder, total, "buy_order", stall).await?;
        let id: i64 = sqlx::query("insert into buy_orders (stall_id, material, price, quantity) values ($1, $2, $3, $4) returning id")
            .bind(stall)
            .bind(material)
            .bind(price)
            .bind(quantity)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?
            .try_get("id")
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// Fill one unit of a buy order with a matching component: the item goes to the stall
    /// owner's storage-side (the stall holder), the escrowed price to the seller.
    pub async fn buy_order_fill(
        &self,
        seller: i64,
        order: i64,
        item: i64,
    ) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        // Holders first, then the order row: the same order as a cancel or a closing stall.
        let who = sqlx::query(
            "select s.holder_id from buy_orders o join stalls s on s.id = o.stall_id where o.id = $1",
        )
        .bind(order)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let seller_h = character_holder(&mut tx, seller).await?;
        lock_holders(
            &mut tx,
            &[seller_h, who.try_get("holder_id").map_err(internal)?],
        )
        .await?;
        let r = sqlx::query(
            "select o.material, o.price, o.quantity, s.holder_id, s.owner_character, s.id as stall_id from buy_orders o \
             join stalls s on s.id = o.stall_id where o.id = $1 for update of o",
        )
        .bind(order)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let material: String = r.try_get("material").map_err(internal)?;
        let price: i64 = r.try_get("price").map_err(internal)?;
        let quantity: i32 = r.try_get("quantity").map_err(internal)?;
        let stall_holder: i64 = r.try_get("holder_id").map_err(internal)?;
        let owner: i64 = r.try_get("owner_character").map_err(internal)?;
        if quantity <= 0 {
            return Err(EconError::State("the order is filled".into()));
        }
        if owner == seller {
            return Err(EconError::Invalid("that is your own order".into()));
        }
        let parts = components_of(&mut tx, item).await?;
        let tpl: Option<String> = sqlx::query("select template from items where id = $1")
            .bind(item)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .map(|r| r.try_get("template"))
            .transpose()
            .map_err(internal)?;
        if tpl.as_deref() != Some("component") || parts.len() != 1 || parts[0].material != material
        {
            return Err(EconError::Invalid(
                "the item does not match the order".into(),
            ));
        }
        self.mv(&mut tx, item, seller_h, stall_holder, "buy_order", order)
            .await?;
        move_coin(&mut tx, stall_holder, seller_h, price, "buy_order", order).await?;
        sqlx::query("update buy_orders set quantity = quantity - 1 where id = $1")
            .bind(order)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)
    }

    /// Cancel a buy order: the unspent escrow returns to the owner.
    pub async fn buy_order_cancel(&self, character: i64, order: i64) -> Result<(), EconError> {
        let mut tx = self.begin().await?;
        let (stall, holder) = Self::stall_of_owner(&mut tx, character, None).await?;
        let inv = character_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[holder, inv]).await?;
        let r = sqlx::query(
            "select price, quantity from buy_orders where id = $1 and stall_id = $2 for update",
        )
        .bind(order)
        .bind(stall)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let price: i64 = r.try_get("price").map_err(internal)?;
        let quantity: i32 = r.try_get("quantity").map_err(internal)?;
        move_coin(
            &mut tx,
            holder,
            inv,
            price * quantity as i64,
            "withdraw",
            order,
        )
        .await?;
        sqlx::query("delete from buy_orders where id = $1")
            .bind(order)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)
    }

    /// Close the stall (or let it expire): everything goes back to the owner, items to the
    /// inventory, then to storage, which may overflow. It never fails for lack of room, so
    /// the tile is always freed. Returns the stall's id and its zone, for whoever must be told.
    pub async fn stall_close(&self, character: i64) -> Result<(i64, String), EconError> {
        let mut tx = self.begin().await?;
        let (stall, holder) = Self::stall_of_owner(&mut tx, character, None).await?;
        let zone: String = sqlx::query("select zone from stalls where id = $1")
            .bind(stall)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?
            .try_get("zone")
            .map_err(internal)?;
        let inv = character_holder(&mut tx, character).await?;
        let sto = storage_holder(&mut tx, character).await?;
        lock_holders(&mut tx, &[holder, inv, sto]).await?;
        let coin: i64 = sqlx::query("select coin from holders where id = $1")
            .bind(holder)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?
            .try_get("coin")
            .map_err(internal)?;
        move_coin(&mut tx, holder, inv, coin, "withdraw", stall).await?;
        let items: Vec<i64> = sqlx::query("select id from items where holder_id = $1 order by id")
            .bind(holder)
            .fetch_all(&mut *tx)
            .await
            .map_err(internal)?
            .iter()
            .map(|r| r.try_get("id"))
            .collect::<Result<_, _>>()
            .map_err(internal)?;
        sqlx::query("delete from listings where stall_id = $1")
            .bind(stall)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        for item in items {
            let target = if has_room(&mut tx, inv, 1).await? {
                inv
            } else {
                sto
            };
            move_item_stacking(
                &mut tx,
                item,
                holder,
                target,
                "withdraw",
                stall,
                true,
                &self.stacks,
            )
            .await?;
        }
        sqlx::query("delete from buy_orders where stall_id = $1")
            .bind(stall)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        sqlx::query("delete from stalls where id = $1")
            .bind(stall)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        // The emptied holder stays: the ledger rows that name it are the stall's history.
        tx.commit().await.map_err(internal)?;
        Ok((stall, zone))
    }

    /// The open stalls of `zone` (or the one stall `only`), with what a zone shows of their
    /// keepers: `(id, tile_x, tile_y, owner, owner name, build, model hash and frame while
    /// the model is active)`.
    #[allow(clippy::type_complexity)]
    pub async fn stalls_in(
        &self,
        zone: &str,
        only: Option<i64>,
    ) -> Result<
        Vec<(
            i64,
            i32,
            i32,
            i64,
            String,
            serde_json::Value,
            Option<(Vec<u8>, i16)>,
        )>,
        EconError,
    > {
        let rows = sqlx::query(
            "select s.id, s.tile_x, s.tile_y, s.owner_character, c.name, c.build, m.hash, m.frame \
             from stalls s join characters c on c.id = s.owner_character \
             left join models m on m.hash = c.model and m.status = 'active' \
             where s.zone = $1 and ($2::bigint is null or s.id = $2) order by s.id",
        )
        .bind(zone)
        .bind(only)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        rows.iter()
            .map(|r| {
                let hash: Option<Vec<u8>> = r.try_get("hash").map_err(internal)?;
                let frame: Option<i16> = r.try_get("frame").map_err(internal)?;
                Ok((
                    r.try_get("id").map_err(internal)?,
                    r.try_get("tile_x").map_err(internal)?,
                    r.try_get("tile_y").map_err(internal)?,
                    r.try_get("owner_character").map_err(internal)?,
                    r.try_get("name").map_err(internal)?,
                    r.try_get("build").map_err(internal)?,
                    hash.zip(frame),
                ))
            })
            .collect()
    }

    /// Owners of stalls past their 48 h (the hub closes them on a timer).
    pub async fn stalls_expired(&self) -> Result<Vec<i64>, EconError> {
        sqlx::query("select owner_character from stalls where expires <= now()")
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?
            .iter()
            .map(|r| r.try_get("owner_character").map_err(internal))
            .collect()
    }

    // ---------- escrow contracts (ECONOMY.md 8) ----------

    pub async fn contract_post(
        &self,
        buyer: i64,
        instance: &str,
        price: i64,
        collateral: i64,
    ) -> Result<i64, EconError> {
        check_price(price)?;
        if !(0..=MAX_PRICE).contains(&collateral) || instance.is_empty() || instance.len() > 64 {
            return Err(EconError::Invalid("contract".into()));
        }
        let mut tx = self.begin().await?;
        character_holder(&mut tx, buyer).await?;
        let id: i64 = sqlx::query("insert into contracts (buyer_character, instance, price, collateral) values ($1, $2, $3, $4) returning id")
            .bind(buyer)
            .bind(instance)
            .bind(price)
            .bind(collateral)
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?
            .try_get("id")
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(id)
    }

    /// Only an open contract can be cancelled, and only by its buyer.
    pub async fn contract_cancel(&self, buyer: i64, contract: i64) -> Result<(), EconError> {
        let n = sqlx::query("update contracts set state = 'cancelled', ended = now() where id = $1 and buyer_character = $2 and state = 'open'")
            .bind(contract)
            .bind(buyer)
            .execute(&self.pool)
            .await
            .map_err(internal)?
            .rows_affected();
        if n == 0 {
            Err(EconError::State("not an open contract of yours".into()))
        } else {
            Ok(())
        }
    }

    /// The party accepts: price and collateral lock into escrow; nobody can cancel after.
    pub async fn contract_accept(
        &self,
        leader: i64,
        contract: i64,
        sellers: &[i64],
    ) -> Result<(), EconError> {
        if sellers.is_empty() || sellers.len() > 16 || !sellers.contains(&leader) {
            return Err(EconError::Invalid(
                "the party must include its leader".into(),
            ));
        }
        let mut tx = self.begin().await?;
        let r = sqlx::query("select buyer_character, price, collateral, state from contracts where id = $1 for update")
            .bind(contract)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?;
        let state: String = r.try_get("state").map_err(internal)?;
        if state != "open" {
            return Err(EconError::State(state));
        }
        let buyer: i64 = r.try_get("buyer_character").map_err(internal)?;
        let price: i64 = r.try_get("price").map_err(internal)?;
        let collateral: i64 = r.try_get("collateral").map_err(internal)?;
        if sellers.contains(&buyer) {
            return Err(EconError::Invalid("the buyer cannot carry themself".into()));
        }
        let escrow = new_holder(&mut tx, "escrow", 0).await?;
        let buyer_h = character_holder(&mut tx, buyer).await?;
        let leader_h = character_holder(&mut tx, leader).await?;
        lock_holders(&mut tx, &[buyer_h, leader_h]).await?;
        move_coin(&mut tx, buyer_h, escrow, price, "escrow_lock", contract).await?;
        move_coin(
            &mut tx,
            leader_h,
            escrow,
            collateral,
            "escrow_lock",
            contract,
        )
        .await?;
        for &s in sellers {
            character_holder(&mut tx, s).await?;
            sqlx::query("insert into contract_sellers (contract_id, character_id) values ($1, $2) on conflict do nothing")
                .bind(contract)
                .bind(s)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        sqlx::query("update contracts set state = 'active', leader_character = $2, escrow_holder = $3, started = now() where id = $1")
            .bind(contract)
            .bind(leader)
            .bind(escrow)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)
    }

    /// The zone's outcome report. Idempotent: only an active contract changes, once.
    /// Returns whether this report decided it.
    pub async fn contract_report(
        &self,
        contract: i64,
        outcome: Outcome,
    ) -> Result<bool, EconError> {
        let mut tx = self.begin().await?;
        let r = sqlx::query("select buyer_character, leader_character, price, collateral, state, escrow_holder from contracts where id = $1 for update")
            .bind(contract)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?;
        let state: String = r.try_get("state").map_err(internal)?;
        if state != "active" {
            return Ok(false);
        }
        let buyer: i64 = r.try_get("buyer_character").map_err(internal)?;
        let leader: i64 = r
            .try_get::<Option<i64>, _>("leader_character")
            .map_err(internal)?
            .ok_or(EconError::Internal)?;
        let price: i64 = r.try_get("price").map_err(internal)?;
        let collateral: i64 = r.try_get("collateral").map_err(internal)?;
        let escrow: i64 = r
            .try_get::<Option<i64>, _>("escrow_holder")
            .map_err(internal)?
            .ok_or(EconError::Internal)?;
        let buyer_h = character_holder(&mut tx, buyer).await?;
        let leader_h = character_holder(&mut tx, leader).await?;
        // Everyone this report can pay, locked in one ordered call.
        let mut all = vec![escrow, buyer_h, leader_h];
        for r in sqlx::query("select character_id from contract_sellers where contract_id = $1")
            .bind(contract)
            .fetch_all(&mut *tx)
            .await
            .map_err(internal)?
        {
            let seller: i64 = r.try_get("character_id").map_err(internal)?;
            all.push(character_holder(&mut tx, seller).await?);
        }
        lock_holders(&mut tx, &all).await?;
        let (new_state, label) = match outcome {
            Outcome::Completed => {
                let sellers: Vec<i64> = sqlx::query("select character_id from contract_sellers where contract_id = $1 order by character_id")
                    .bind(contract)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(internal)?
                    .iter()
                    .map(|r| r.try_get("character_id"))
                    .collect::<Result<_, _>>()
                    .map_err(internal)?;
                let each = price / sellers.len() as i64;
                let remainder = price - each * sellers.len() as i64;
                for &s in &sellers {
                    let h = character_holder(&mut tx, s).await?;
                    let extra = if s == leader { remainder } else { 0 };
                    move_coin(&mut tx, escrow, h, each + extra, "escrow_pay", contract).await?;
                }
                move_coin(
                    &mut tx,
                    escrow,
                    leader_h,
                    collateral,
                    "escrow_refund",
                    contract,
                )
                .await?;
                ("paid", "completed")
            }
            Outcome::Wipe => {
                move_coin(&mut tx, escrow, buyer_h, price, "escrow_refund", contract).await?;
                move_coin(
                    &mut tx,
                    escrow,
                    leader_h,
                    collateral,
                    "escrow_refund",
                    contract,
                )
                .await?;
                ("refunded", "wipe")
            }
            Outcome::Abandon => {
                move_coin(
                    &mut tx,
                    escrow,
                    buyer_h,
                    price + collateral,
                    "escrow_refund",
                    contract,
                )
                .await?;
                ("refunded", "abandon")
            }
        };
        sqlx::query("update contracts set state = $2, outcome = $3, ended = now() where id = $1 and state = 'active'")
            .bind(contract)
            .bind(new_state)
            .bind(label)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(true)
    }

    /// Active contracts nobody reported on for two hours are refunded as abandoned, so a
    /// party cannot hold a buyer's coin hostage by stalling. Returns how many.
    pub async fn contracts_expire(&self) -> Result<usize, EconError> {
        let ids: Vec<i64> = sqlx::query(
            "select id from contracts where state = 'active' and started < now() - make_interval(mins => $1)",
        )
        .bind(CONTRACT_TIMEOUT_MINUTES)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<_, _>>()
        .map_err(internal)?;
        let mut n = 0;
        for id in ids {
            if self.contract_report(id, Outcome::Abandon).await? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// The zone a contract's run happens in: only that zone may report its outcome.
    pub async fn contract_instance(&self, contract: i64) -> Result<String, EconError> {
        sqlx::query("select instance from contracts where id = $1")
            .bind(contract)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?
            .try_get("instance")
            .map_err(internal)
    }

    pub async fn contract_state(&self, contract: i64) -> Result<String, EconError> {
        sqlx::query("select state from contracts where id = $1")
            .bind(contract)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .ok_or(EconError::NotFound)?
            .try_get("state")
            .map_err(internal)
    }

    // ---------- tavern hires (ECONOMY.md 11) ----------

    /// List a character in the tavern at a price. The listing keeps the build the
    /// character has now (PARTY.md 7): that is what the tavern shows and what a hire
    /// buys, whatever its owner makes of the character afterwards. Listing again lists
    /// the build of that moment.
    pub async fn hire_list(&self, character: i64, price: i64) -> Result<(), EconError> {
        check_price(price)?;
        sqlx::query(
            "insert into hire_listings (character_id, price, build) \
             select $1, $2, c.build from characters c where c.id = $1 \
             on conflict (character_id) do update set price = excluded.price, build = excluded.build",
        )
        .bind(character)
        .bind(price)
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(())
    }

    /// The price a character is listed for hire at, if it is.
    pub async fn hire_listed(&self, character: i64) -> Result<Option<i64>, EconError> {
        sqlx::query("select price from hire_listings where character_id = $1")
            .bind(character)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?
            .map(|r| r.try_get("price").map_err(internal))
            .transpose()
    }

    /// Take a character off the tavern's list. Hires of it that run are not ended.
    pub async fn hire_unlist(&self, character: i64) -> Result<(), EconError> {
        sqlx::query("delete from hire_listings where character_id = $1")
            .bind(character)
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(())
    }

    /// Hire an offline avatar: 30% of the price burns, 70% goes to the avatar. An account
    /// cannot hire its own characters (the burn would otherwise be the only cost of moving
    /// coin between alts, and the hire the way to farm with them). `capacity` is the hirer's
    /// squad capacity (COMPANIONS.md 3.3): a hire beyond it, or a second copy of an avatar
    /// already in the squad, is refused before any coin moves. Returns the hire's id and the
    /// amount burned.
    ///
    /// `shown`: the price the hirer was shown (PARTY.md 7); when the listing's is another
    /// now, nothing is hired. `None`: whatever it costs (the hub's own callers).
    /// `fit`: whether the listed build (what the hire buys) can be played at all; one that
    /// cannot (the content changed under it) is not hired, and nothing is paid.
    pub async fn hire(
        &self,
        hirer: i64,
        avatar: i64,
        capacity: usize,
        shown: Option<i64>,
        fit: impl Fn(&serde_json::Value) -> bool,
    ) -> Result<(i64, i64), EconError> {
        let mut tx = self.begin().await?;
        let r = sqlx::query(
            "select l.price, l.build, c.account_id, c.location_kind from hire_listings l join characters c on c.id = l.character_id \
             where l.character_id = $1 for update of l, c",
        )
        .bind(avatar)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(EconError::NotFound)?;
        let price: i64 = r.try_get("price").map_err(internal)?;
        if shown.is_some_and(|shown| shown != price) {
            return Err(EconError::State(PRICE_CHANGED.into()));
        }
        let build: Option<serde_json::Value> = r.try_get("build").map_err(internal)?;
        if !build.as_ref().is_some_and(&fit) {
            return Err(EconError::State(NOT_FOR_HIRE.into()));
        }
        let avatar_account: i64 = r.try_get("account_id").map_err(internal)?;
        let location: String = r.try_get("location_kind").map_err(internal)?;
        if location != "offline" {
            return Err(EconError::State("the avatar's owner is playing it".into()));
        }
        if account_of(&mut tx, hirer).await? == avatar_account {
            return Err(EconError::Invalid(
                "you cannot hire your own character".into(),
            ));
        }
        let burn = price * HIRE_BURN_PER_CENT / 100;
        let hirer_h = character_holder(&mut tx, hirer).await?;
        let avatar_h = character_holder(&mut tx, avatar).await?;
        let sink = singleton(&mut tx, "sink").await?;
        lock_holders(&mut tx, &[hirer_h, avatar_h]).await?;
        // Under the hirer's holder lock, so two hires at once cannot both see room.
        let squad: Vec<i64> = sqlx::query(
            "select avatar_character from hires where hirer_character = $1 and ended is null \
             and at > now() - make_interval(hours => $2)",
        )
        .bind(hirer)
        .bind(HIRE_WINDOW_HOURS as i32)
        .fetch_all(&mut *tx)
        .await
        .map_err(internal)?
        .iter()
        .map(|r| r.try_get("avatar_character").map_err(internal))
        .collect::<Result<_, _>>()?;
        if squad.contains(&avatar) {
            return Err(EconError::State(
                "that avatar is already in your squad".into(),
            ));
        }
        if squad.len() >= capacity {
            return Err(EconError::State("your squad is full".into()));
        }
        // What is hired is what was listed: the hire keeps the listing's build.
        let hire_id: i64 = sqlx::query(
            "insert into hires (avatar_character, hirer_character, price, burned, build) \
             values ($1, $2, $3, $4, $5) returning id",
        )
        .bind(avatar)
        .bind(hirer)
        .bind(price)
        .bind(burn)
        .bind(build)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?
        .try_get("id")
        .map_err(internal)?;
        move_coin(&mut tx, hirer_h, sink, burn, "hire_burn", hire_id).await?;
        move_coin(&mut tx, hirer_h, avatar_h, price - burn, "hire", hire_id).await?;
        tx.commit().await.map_err(internal)?;
        Ok((hire_id, burn))
    }

    /// The active hires of `hirer`, oldest first (COMPANIONS.md 3.3).
    pub async fn squad(&self, hirer: i64) -> Result<Vec<ActiveHire>, EconError> {
        let rows = sqlx::query(
            "select h.id, h.avatar_character, c.name, coalesce(h.build, c.build) as build, \
             extract(epoch from h.at + make_interval(hours => $2))::bigint as expires \
             from hires h join characters c on c.id = h.avatar_character \
             where h.hirer_character = $1 and h.ended is null and h.at > now() - make_interval(hours => $2) \
             order by h.at, h.id",
        )
        .bind(hirer)
        .bind(HIRE_WINDOW_HOURS as i32)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        rows.iter()
            .map(|r| {
                Ok(ActiveHire {
                    id: r.try_get("id").map_err(internal)?,
                    avatar: r.try_get("avatar_character").map_err(internal)?,
                    name: r.try_get("name").map_err(internal)?,
                    build: r.try_get("build").map_err(internal)?,
                    expires_unix: r.try_get("expires").map_err(internal)?,
                })
            })
            .collect()
    }

    /// The hirer sends a hired avatar away before its time. Nothing is refunded.
    pub async fn dismiss(&self, hirer: i64, hire: i64) -> Result<(), EconError> {
        let n = sqlx::query(
            "update hires set ended = now() where id = $1 and hirer_character = $2 and ended is null \
             and at > now() - make_interval(hours => $3)",
        )
        .bind(hire)
        .bind(hirer)
        .bind(HIRE_WINDOW_HOURS as i32)
        .execute(&self.pool)
        .await
        .map_err(internal)?
        .rows_affected();
        if n == 0 {
            return Err(EconError::NotFound);
        }
        Ok(())
    }

    /// The avatar's owner took the character back (ECONOMY.md 11): every active hire of it
    /// ends, without a refund. Returns `(hire, hirer)` for each.
    pub async fn end_hires_of(&self, avatar: i64) -> Result<Vec<(i64, i64)>, EconError> {
        let rows = sqlx::query(
            "update hires set ended = now() where avatar_character = $1 and ended is null \
             and at > now() - make_interval(hours => $2) returning id, hirer_character",
        )
        .bind(avatar)
        .bind(HIRE_WINDOW_HOURS as i32)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get("id").map_err(internal)?,
                    r.try_get("hirer_character").map_err(internal)?,
                ))
            })
            .collect()
    }

    /// The tavern list, avatars hired three or more times in the last 12 h sorted last
    /// (diminishing priority).
    /// The tavern's list: every listed character whose owner is offline, with the build
    /// its listing was made with (a listing made before builds were kept with them is
    /// not served: its owner lists again).
    pub async fn tavern(&self) -> Result<Vec<TavernRow>, EconError> {
        let rows = sqlx::query(
            "select l.character_id, c.name, l.build, l.price, (select count(*) from hires h where h.avatar_character = l.character_id \
             and h.at > now() - make_interval(hours => $1)) as recent from hire_listings l \
             join characters c on c.id = l.character_id where c.location_kind = 'offline' and l.build is not null \
             order by (select count(*) from hires h where h.avatar_character = l.character_id \
             and h.at > now() - make_interval(hours => $1)) >= $2, l.price, l.character_id limit 200",
        )
        .bind(HIRE_WINDOW_HOURS as i32)
        .bind(HIRES_BEFORE_DEMOTION)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        rows.iter()
            .map(|r| {
                Ok(TavernRow {
                    character: r.try_get("character_id").map_err(internal)?,
                    name: r.try_get("name").map_err(internal)?,
                    build: r.try_get("build").map_err(internal)?,
                    price: r.try_get("price").map_err(internal)?,
                    hires: r.try_get("recent").map_err(internal)?,
                })
            })
            .collect()
    }
}

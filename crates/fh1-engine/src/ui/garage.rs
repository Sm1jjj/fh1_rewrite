//! Garage (My cars) and Autoshow (P10, 2026-10-08; docs/CUSTOMIZE.md "Garage and Autoshow"): the pause menu's car
//! browser (browser.rs) in two more modes, on the credits ledger and car ownership of progression::wallet (dc).
//!
//! - **My cars**: the owned cars (`wallet::owned`). Enter drives one; Delete / R3 sells it after a confirm dialog
//!   (carflow IDS_Sell_Car_Confirm): (BaseCost + the credits spent on its parts) x SellValueScale 0.5 (wallet::sell_price;
//!   the parts term is customize.rs `CarLook::spent`). Not the car being driven, not the last car, not a barn find.
//! - **Autoshow**: every FH1 car for sale (`wallet::price`: gamedb BaseCost; not traffic, unicorns or BaseCost 0), by
//!   manufacturer, with price, class / PI and stats. Enter on a car not owned asks "BUY CAR?" (carflow IDS_Buy_Confirm),
//!   pays and drives it; short of credits the dialog says INSUFFICIENT CR! (carflow IDS_Buy_Insufficient_Credits_Title).
//! - The credits balance shows on the Garage / car / Customize pages; the selected car's photo is the game's own
//!   `ui/textures/thumbnails/thumbnail_<Data_Car.Id>.png` (ui group, 153 selectable cars).
//!
//! `FH1_OWNERSHIP=0` (wallet::ownership_on) = the old Change car list of every car, no prices.
//! STOPGAP: drawn with the pause menu's panel, not the game's Anark screens (115_c_buy_mfrselect, 108_c_buy_carselect,
//! 114_c_buy_buycar_color, 106_c_car_list), whose SUPER_STACKER / scrolling-list contracts aren't driven yet.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;

use super::browser::{CarBrowser, CarCatalog};
use super::customize::CarLooks;
use super::notify::HudNotify;
use super::{Menu, Page};
use crate::progression::wallet::{self, BuyError, SellError};
use crate::progression::{fmt_num, Profile};
use crate::Garage;

/// Which cars the car page lists.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Mode {
    /// Every car (Change car; ownership off).
    #[default]
    All,
    /// My cars.
    Owned,
    /// The autoshow.
    Shop,
}

/// Ownership gates the car lists (Garage = My cars + Autoshow).
pub fn shop_on() -> bool {
    wallet::ownership_on()
}

/// Row of "Customize" on the Garage page.
pub fn customize_row() -> usize {
    if shop_on() { 2 } else { 1 }
}

#[derive(Clone, Copy, Debug)]
enum Req {
    Buy(usize),
    Sell(usize),
}

/// A yes / no dialog over the car page.
pub struct Confirm {
    pub title: String,
    pub message: String,
    req: Req,
}

/// The garage / autoshow state, held by ui.rs `Menu`.
#[derive(Default)]
pub struct ShopState {
    pub mode: Mode,
    /// Rebuild the car list (opened, or a car was bought / sold).
    rebuild: bool,
    pub confirm: Option<Confirm>,
    /// Last result (bought / sold / refused), under the page title.
    pub notice: Option<String>,
    requests: Vec<Req>,
    /// A car just bought: ui.rs drives it and closes the menu.
    pub drive: Option<usize>,
    /// Garage indices owned, and the display name / price / sell value of the listed cars (filled on rebuild).
    owned: HashSet<usize>,
    names: HashMap<usize, String>,
    prices: HashMap<usize, i64>,
    sell: HashMap<usize, i64>,
}

impl ShopState {
    /// The car page opens in `mode`.
    pub fn open(&mut self, mode: Mode) {
        self.mode = mode;
        self.rebuild = mode != Mode::All;
        self.confirm = None;
        self.notice = None;
        self.drive = None;
    }

    /// Enter on car `i`: drive it (true), or ask to buy it in the autoshow (false).
    pub fn pick(&mut self, i: usize) -> bool {
        if self.mode != Mode::Shop || self.owned.contains(&i) {
            return true;
        }
        let name = self.names.get(&i).cloned().unwrap_or_default();
        self.confirm = Some(match self.prices.get(&i) {
            Some(&p) => Confirm { title: "BUY CAR?".into(), message: format!("Do you want to buy the {name} for {} CR?", fmt_num(p)), req: Req::Buy(i) },
            None => {
                self.notice = Some(format!("The {name} is not for sale"));
                return false;
            }
        });
        false
    }

    /// Delete / R3 on car `i` in My cars.
    pub fn ask_sell(&mut self, i: usize) {
        if self.mode != Mode::Owned {
            return;
        }
        let name = self.names.get(&i).cloned().unwrap_or_default();
        match self.sell.get(&i) {
            Some(&v) => self.confirm = Some(Confirm { title: "SELL CAR".into(), message: format!("Are you sure you want to sell your {name} for {} CR?", fmt_num(v)), req: Req::Sell(i) }),
            None => self.notice = Some(format!("The {name} can't be sold")),
        }
    }

    /// The dialog's answer.
    pub fn answer(&mut self, yes: bool) {
        if let Some(c) = self.confirm.take() {
            if yes {
                self.requests.push(c.req);
            }
        }
    }

    /// The hint line of the car page in this mode.
    pub fn hint_extra(&self) -> &'static str {
        match self.mode {
            Mode::Owned => "      R3 Del  sell",
            Mode::Shop => "      A Enter  buy / drive",
            Mode::All => "",
        }
    }
}

pub struct GaragePlugin;

impl Plugin for GaragePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, run_shop.after(super::menu_input).after(super::menu_mouse).before(super::draw_menu));
    }
}

/// Carry out buy / sell answers, then rebuild the car list when needed.
#[allow(clippy::too_many_arguments)]
fn run_shop(
    mut menu: ResMut<Menu>,
    mut profile: Option<ResMut<Profile>>,
    events: Option<Res<crate::race::Events>>,
    garage: Res<Garage>,
    looks: Res<CarLooks>,
    mut notes: MessageWriter<HudNotify>,
) {
    let menu = &mut *menu;
    if menu.shop.requests.is_empty() && !menu.shop.rebuild {
        return;
    }
    let (Some(p), Some(ev)) = (profile.as_deref_mut(), events.as_deref()) else {
        menu.shop.requests.clear();
        menu.shop.rebuild = false;
        menu.shop.notice = Some("Career data not loaded (progression off or the events group missing)".into());
        menu.dirty = true;
        return;
    };
    let career = &ev.career;
    let current = garage.cars.get(garage.current).map(String::as_str);
    for req in std::mem::take(&mut menu.shop.requests) {
        match req {
            Req::Buy(i) => {
                let Some(car) = garage.cars.get(i) else { continue };
                let name = menu.shop.names.get(&i).cloned().unwrap_or_else(|| car.clone());
                menu.shop.notice = Some(match wallet::buy(p, career, car) {
                    Ok(paid) => {
                        notes.write(HudNotify { lines: vec!["CAR ADDED TO GARAGE".into(), name.to_uppercase()] });
                        menu.shop.drive = Some(i);
                        format!("Bought the {name} for {} CR", fmt_num(paid))
                    }
                    Err(BuyError::NotEnough { need, have }) => format!("INSUFFICIENT CR!  The {name} costs {} CR, you have {} CR", fmt_num(need), fmt_num(have)),
                    Err(BuyError::AlreadyOwned) => format!("You already own the {name}"),
                    Err(BuyError::NotForSale) => format!("The {name} is not for sale"),
                });
                menu.shop.rebuild = true;
            }
            Req::Sell(i) => {
                let Some(car) = garage.cars.get(i) else { continue };
                let name = menu.shop.names.get(&i).cloned().unwrap_or_else(|| car.clone());
                menu.shop.notice = Some(match wallet::sell(p, career, car, current, looks.spent(car)) {
                    Ok(v) => format!("Sold the {name} for {} CR", fmt_num(v)),
                    Err(SellError::Current) => "You can't do this to the car you are currently in. Get in another car first.".into(),
                    Err(SellError::LastCar) => "You can't sell your last car".into(),
                    Err(SellError::CantSell) => "You cannot sell Barn finds".into(),
                    Err(SellError::NotOwned) => format!("You don't own the {name}"),
                });
                menu.shop.rebuild = true;
            }
        }
    }
    menu.dirty = true;
    if !std::mem::take(&mut menu.shop.rebuild) || menu.page != Page::Cars || menu.shop.mode == Mode::All {
        return;
    }
    // The list: the full catalog filtered by mode, tagged with price / sell value.
    let catalog = menu.catalog.get_or_insert_with(|| super::browser::catalog_for(&garage.assets, &garage.cars)).clone();
    let owned: HashSet<usize> = wallet::owned(p).iter().filter_map(|o| garage.cars.iter().position(|c| *c == o.car)).collect();
    let mut tags = HashMap::new();
    let (mut names, mut prices, mut sell) = (HashMap::new(), HashMap::new(), HashMap::new());
    let sub: CarCatalog = match menu.shop.mode {
        Mode::Owned => catalog.subset(|e| owned.contains(&e.index)),
        _ => catalog.subset(|e| e.game == "FH1" && garage.cars.get(e.index).is_some_and(|c| wallet::price(career, c).is_some())),
    };
    for e in &sub.entries {
        let Some(car) = garage.cars.get(e.index) else { continue };
        names.insert(e.index, format!("{} {}", e.maker, e.name));
        match menu.shop.mode {
            Mode::Owned => {
                let v = wallet::sell_price(career, p, car, looks.spent(car));
                if let Some(v) = v {
                    sell.insert(e.index, v);
                }
                tags.insert(e.index, ("Sell value".to_string(), v.map_or_else(|| "Can't be sold".into(), |v| format!("{} CR", fmt_num(v)))));
            }
            _ => {
                let price = wallet::price(career, car);
                if let Some(c) = price {
                    prices.insert(e.index, c);
                }
                let label = if owned.contains(&e.index) { "Owned".to_string() } else { price.map_or_else(|| "Not for sale".into(), |c| format!("{} CR", fmt_num(c))) };
                tags.insert(e.index, ("Price".to_string(), label));
            }
        }
    }
    if sub.entries.is_empty() && menu.shop.notice.is_none() {
        menu.shop.notice = Some(match menu.shop.mode {
            Mode::Owned => "No cars owned".into(),
            _ => "No cars for sale (re-run fh1setup: the events group carries the prices)".into(),
        });
    }
    let keep = menu.cars.as_ref().and_then(|b| b.selected()).map(|e| e.index);
    // Opens on the selected car (after a buy / sell), else the car being driven.
    menu.cars = Some(CarBrowser::new(std::sync::Arc::new(sub), keep.unwrap_or(garage.current)).with_tags(tags));
    menu.shop.owned = owned;
    menu.shop.names = names;
    menu.shop.prices = prices;
    menu.shop.sell = sell;
}

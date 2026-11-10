//! Keymap planning: which key presses produce a character in the compositor's XKB keymap.
//!
//! Pure code (apart from reading the keymap fd) with no compositor involved, so it is
//! unit-tested with keymaps compiled from RMLVO names.
//!
//! [`KeyTable::new`] probes the keymap once with an `xkb::State` per layout group: for each
//! combination of the modifiers murmur presses itself (none, Shift, Level3, both) and each
//! Caps Lock state, it records which key yields which keysym. That mirrors what the compositor
//! and the focused client compute for the same key events, including Caps Lock's
//! capitalization rules. The result is plain data, so it can live in async code
//! (`xkb::Keymap` is not `Send`).
//!
//! Caps Lock is the only modifier state the table plans for. Any other modifier the user holds,
//! latches or locks would change what the keys produce (or turn them into shortcuts), so
//! [`KeyTable::blocks_typing`] makes typing wait for it, except for locks that provably change
//! none of the planned keys (Num Lock on ordinary layouts).

use std::{collections::HashMap, os::fd::OwnedFd, os::unix::fs::FileExt};

use xkbcommon::xkb;

/// XKB keycodes are Linux evdev keycodes plus 8. `ei_keyboard.key` takes evdev keycodes.
const EVDEV_OFFSET: u32 = 8;

/// Upper bound for a keymap sent by the compositor (real ones are ~40-100 KiB).
const MAX_KEYMAP_SIZE: usize = 16 << 20;

/// Errors from loading a keymap.
#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    /// The advertised size is zero or implausibly large.
    #[error("keymap size {0} is not plausible")]
    Size(usize),
    /// Reading the keymap fd failed.
    #[error("cannot read keymap: {0}")]
    Read(#[from] std::io::Error),
    /// libxkbcommon rejected the keymap.
    #[error("keymap failed to compile")]
    Compile,
}

/// Modifier and layout state as reported by the compositor (`ei_keyboard.modifiers`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModState {
    /// Modifiers held down (physically or by any emulated device).
    pub depressed: u32,
    /// Latched modifiers (sticky keys).
    pub latched: u32,
    /// Locked modifiers (Caps Lock, Num Lock).
    pub locked: u32,
    /// Active layout group.
    pub group: u32,
}

/// One character: tap `key` while holding the listed modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    /// Linux evdev keycode.
    pub key: u32,
    /// Hold the group's Shift key.
    pub shift: bool,
    /// Hold the group's Level3 (AltGr) key.
    pub level3: bool,
}

/// A key press or release for `ei_keyboard.key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// Linux evdev keycode.
    pub key: u32,
    /// `true` for press, `false` for release.
    pub pressed: bool,
}

/// Key events for a piece of text, plus what had to be left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Events in order. Every press has a matching release.
    pub events: Vec<KeyEvent>,
    /// Characters not available in the active layout group (skipped).
    pub unmapped: usize,
    /// Control characters (newline, tab, ...), which are never typed.
    pub control: usize,
}

/// Modifier combinations murmur can produce, cheapest first. The order decides which key wins
/// when several produce the same keysym: fewest modifiers (lowest shift level) first.
const COMBOS: [(bool, bool); 4] = [(false, false), (true, false), (false, true), (true, true)];

#[derive(Debug, Default)]
struct Group {
    /// Evdev keycode murmur presses for Shift in this group.
    shift: Option<u32>,
    /// Evdev keycode murmur presses for Level3 (AltGr) in this group.
    level3: Option<u32>,
    /// Keysym → stroke, indexed by Caps Lock state (0 = off, 1 = on).
    strokes: [HashMap<u32, Stroke>; 2],
}

/// How to type characters with one compiled keymap.
#[derive(Debug, Default)]
pub struct KeyTable {
    layouts: Vec<String>,
    groups: Vec<Group>,
    /// The `Lock` (Caps Lock) bit in the compositor's modifier masks.
    lock_mask: u32,
    /// Modifiers other than Caps Lock whose lock (or latch) changes none of the planned keys in
    /// any group, e.g. Num Lock on ordinary layouts (but not on Neo, where it is a level lock).
    /// Never includes Control, Alt (Mod1), Super (Mod4), Shift or Level3 (Mod5).
    harmless_mask: u32,
    /// See [`KeyTable::probe_key`]: the key and the modifiers it sets.
    probe: Option<(u32, u32)>,
}

impl KeyTable {
    /// Reads and compiles the keymap the compositor sent with `ei_keyboard.keymap`.
    ///
    /// Blocking (reads the fd and compiles the keymap): call it from `spawn_blocking`.
    pub fn from_fd(fd: OwnedFd, size: usize) -> Result<Self, KeymapError> {
        if size == 0 || size > MAX_KEYMAP_SIZE {
            return Err(KeymapError::Size(size));
        }
        let mut buf = vec![0u8; size];
        // Positional read: the fd may share its file offset with other clients of the compositor.
        std::fs::File::from(fd).read_exact_at(&mut buf, 0)?;
        // The keymap string is NUL-terminated inside `size`.
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        let text = String::from_utf8(buf).map_err(|_| KeymapError::Compile)?;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &ctx,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        )
        .ok_or(KeymapError::Compile)?;
        Ok(Self::new(&keymap))
    }

    /// Builds the table by probing `keymap` (see the module docs).
    pub fn new(keymap: &xkb::Keymap) -> Self {
        let lock_mask = mod_mask(keymap, xkb::MOD_NAME_CAPS);
        let blocking_mask = mod_mask(keymap, xkb::MOD_NAME_CTRL)
            | mod_mask(keymap, xkb::MOD_NAME_ALT)
            | mod_mask(keymap, xkb::MOD_NAME_LOGO);
        let layouts = (0..keymap.num_layouts())
            .map(|g| keymap.layout_get_name(g).to_owned())
            .collect();
        let groups: Vec<Group> = (0..keymap.num_layouts())
            .map(|g| build_group(keymap, g, lock_mask, blocking_mask))
            .collect();
        let level_mask = mod_mask(keymap, xkb::MOD_NAME_SHIFT)
            | mod_mask(keymap, xkb::MOD_NAME_ISO_LEVEL3_SHIFT);
        let harmless_mask = harmless_locks(
            keymap,
            &groups,
            lock_mask,
            blocking_mask | level_mask | lock_mask,
        );
        Self {
            layouts,
            groups,
            lock_mask,
            harmless_mask,
            probe: probe_key(keymap, blocking_mask),
        }
    }

    /// Layout names, one per group (e.g. `["English (US)", "Lithuanian"]`).
    pub fn layouts(&self) -> &[String] {
        &self.layouts
    }

    /// A modifier key that is safe to tap whatever the current group: it produces
    /// `ISO_Level3_Shift` (or else `Shift`) in every group and sets no Control/Alt/Super.
    /// Tapping it makes the compositor report the full modifier state (group, Caps Lock, held
    /// keys), which KWin otherwise only sends after the first change.
    pub fn probe_key(&self) -> Option<u32> {
        self.probe.map(|(key, _)| key)
    }

    /// The modifiers [`KeyTable::probe_key`] sets while pressed (e.g. Mod5), or 0.
    pub fn probe_mask(&self) -> u32 {
        self.probe.map_or(0, |(_, mask)| mask)
    }

    /// `true` while typing now would go wrong (D-006):
    /// - any modifier held, other than Caps Lock and harmless locks such as Num Lock: Control,
    ///   Alt or Super would turn letters into shortcuts; Shift, AltGr or Neo's level 5 would
    ///   change the characters;
    /// - any modifier latched (sticky keys), including Caps Lock: it would apply to the first
    ///   key only, which the plan can't express;
    /// - any modifier locked other than Caps Lock and harmless locks: a locked Control
    ///   (sticky keys) would make shortcuts, a Shift Lock or Level3/5 lock wrong characters.
    ///
    /// Only meaningful while murmur itself holds no keys, i.e. between bursts after a `sync`
    /// round trip; the compositor's modifier events also echo murmur's own Shift/AltGr presses.
    /// Caps Lock doesn't block: [`KeyTable::plan`] accounts for it.
    pub fn blocks_typing(&self, mods: &ModState) -> bool {
        let modeled = self.lock_mask | self.harmless_mask;
        mods.depressed & !modeled != 0
            || mods.latched & !self.harmless_mask != 0
            || mods.locked & !modeled != 0
    }

    /// How to type `c` in the state `mods`, or `None` if the active group can't produce it.
    pub fn stroke(&self, c: char, mods: &ModState) -> Option<Stroke> {
        let group = self.groups.get(usize::try_from(mods.group).ok()?)?;
        // A latched Caps Lock blocks typing instead (see `blocks_typing`).
        let lock = (mods.depressed | mods.locked) & self.lock_mask;
        let caps = usize::from(lock != 0);
        let table = &group.strokes[caps];
        let sym = xkb::Keysym::from_char(c).raw();
        // Keymaps may also spell a character as a Unicode keysym (U+0105 instead of `aogonek`).
        let unicode = 0x0100_0000 | u32::from(c);
        table
            .get(&sym)
            .or_else(|| (unicode != sym).then(|| table.get(&unicode)).flatten())
            .copied()
    }

    /// Key events for `text` in the state `mods`. For each character: press the modifiers it
    /// needs, tap the key, release the modifiers. Characters the active group can't produce
    /// and control characters are skipped and counted.
    pub fn plan(&self, text: &str, mods: &ModState) -> Plan {
        let mut plan = Plan::default();
        let group = usize::try_from(mods.group)
            .ok()
            .and_then(|g| self.groups.get(g));
        for c in text.chars() {
            if c.is_control() {
                plan.control += 1;
                continue;
            }
            let (Some(group), Some(stroke)) = (group, self.stroke(c, mods)) else {
                plan.unmapped += 1;
                continue;
            };
            let held: Vec<u32> = [(stroke.shift, group.shift), (stroke.level3, group.level3)]
                .into_iter()
                .filter_map(|(need, key)| if need { key } else { None })
                .collect();
            let press = |key| KeyEvent { key, pressed: true };
            let release = |key| KeyEvent {
                key,
                pressed: false,
            };
            plan.events.extend(held.iter().copied().map(press));
            plan.events.push(press(stroke.key));
            plan.events.push(release(stroke.key));
            plan.events.extend(held.iter().rev().copied().map(release));
        }
        plan
    }
}

/// Bit for a named modifier, or 0 if the keymap doesn't have it.
fn mod_mask(keymap: &xkb::Keymap, name: &str) -> u32 {
    let idx = keymap.mod_get_index(name);
    if idx < 32 { 1 << idx } else { 0 }
}

fn build_group(keymap: &xkb::Keymap, group: u32, lock_mask: u32, blocking: u32) -> Group {
    let shift = modifier_key(
        keymap,
        group,
        "LFSH",
        &[xkb::Keysym::Shift_L, xkb::Keysym::Shift_R],
        blocking,
    );
    let level3 = modifier_key(
        keymap,
        group,
        "RALT",
        &[xkb::Keysym::ISO_Level3_Shift],
        blocking,
    );
    let mut strokes: [HashMap<u32, Stroke>; 2] = Default::default();
    for (caps, table) in strokes.iter_mut().enumerate() {
        let locked = if caps == 1 { lock_mask } else { 0 };
        for (want_shift, want_level3) in COMBOS {
            let mut held = Vec::new();
            match (want_shift, shift) {
                (true, None) => continue,
                (true, Some(k)) => held.push(k),
                (false, _) => {}
            }
            match (want_level3, level3) {
                (true, None) => continue,
                (true, Some(k)) => held.push(k),
                (false, _) => {}
            }
            let mut state = xkb::State::new(keymap);
            state.update_mask(0, 0, locked, 0, 0, group);
            for &k in &held {
                let _ = state.update_key(k, xkb::KeyDirection::Down);
            }
            for key in keycodes(keymap) {
                if held.contains(&key) {
                    continue;
                }
                let sym = state.key_get_one_sym(key);
                if sym == xkb::Keysym::NoSymbol {
                    continue;
                }
                // First hit wins: COMBOS and keycodes are iterated cheapest first.
                table.entry(sym.raw()).or_insert(Stroke {
                    key: key.raw() - EVDEV_OFFSET,
                    shift: want_shift,
                    level3: want_level3,
                });
            }
        }
    }
    Group {
        shift: shift.map(|k| k.raw() - EVDEV_OFFSET),
        level3: level3.map(|k| k.raw() - EVDEV_OFFSET),
        strokes,
    }
}

/// Finds the key that produces one of `syms` at level 1 of `group`, preferring the key named
/// `preferred` (e.g. `RALT` for AltGr). The key must set a modifier when pressed, and must not
/// set Control/Alt/Super.
fn modifier_key(
    keymap: &xkb::Keymap,
    group: u32,
    preferred: &str,
    syms: &[xkb::Keysym],
    blocking: u32,
) -> Option<xkb::Keycode> {
    let usable = |key| is_safe_modifier(keymap, key, group, syms, blocking);
    keymap
        .key_by_name(preferred)
        .filter(|&k| usable(k))
        .or_else(|| keycodes(keymap).find(|&k| usable(k)))
}

/// See [`KeyTable::probe_key`]: the evdev key and the modifiers it sets in any group.
fn probe_key(keymap: &xkb::Keymap, blocking: u32) -> Option<(u32, u32)> {
    let everywhere = |key, syms: &[xkb::Keysym]| {
        (0..keymap.num_layouts()).all(|g| is_safe_modifier(keymap, key, g, syms, blocking))
    };
    let key = keycodes(keymap)
        .find(|&k| everywhere(k, &[xkb::Keysym::ISO_Level3_Shift]))
        .or_else(|| {
            keymap
                .key_by_name("LFSH")
                .filter(|&k| everywhere(k, &[xkb::Keysym::Shift_L]))
        })
        .or_else(|| {
            keycodes(keymap).find(|&k| everywhere(k, &[xkb::Keysym::Shift_L, xkb::Keysym::Shift_R]))
        })?;
    let mask = (0..keymap.num_layouts())
        .map(|g| {
            let mut state = xkb::State::new(keymap);
            state.update_mask(0, 0, 0, 0, 0, g);
            let _ = state.update_key(key, xkb::KeyDirection::Down);
            state.serialize_mods(xkb::STATE_MODS_DEPRESSED)
        })
        .fold(0, |acc, m| acc | m);
    Some((key.raw() - EVDEV_OFFSET, mask))
}

/// Modifier bits (outside `exclude`) whose lock changes none of the planned strokes in any group
/// or Caps Lock state: locking them leaves every planned character unchanged.
fn harmless_locks(keymap: &xkb::Keymap, groups: &[Group], lock_mask: u32, exclude: u32) -> u32 {
    let mut harmless = 0;
    for idx in 0..keymap.num_mods().min(32) {
        let bit = 1u32 << idx;
        if bit & exclude != 0 {
            continue;
        }
        let unchanged = groups.iter().enumerate().all(|(g, group)| {
            let Ok(g) = u32::try_from(g) else {
                return false;
            };
            group.strokes.iter().enumerate().all(|(caps, table)| {
                let locked = bit | if caps == 1 { lock_mask } else { 0 };
                strokes_survive(keymap, group, g, locked, table)
            })
        });
        if unchanged {
            harmless |= bit;
        }
    }
    harmless
}

/// Every stroke in `table` still produces its keysym with `locked` modifiers in `group`.
fn strokes_survive(
    keymap: &xkb::Keymap,
    group: &Group,
    g: u32,
    locked: u32,
    table: &HashMap<u32, Stroke>,
) -> bool {
    COMBOS.iter().all(|&(want_shift, want_level3)| {
        let held: Vec<u32> = [(want_shift, group.shift), (want_level3, group.level3)]
            .into_iter()
            .filter_map(|(need, key)| if need { key } else { None })
            .collect();
        let mut state = xkb::State::new(keymap);
        state.update_mask(0, 0, locked, 0, 0, g);
        for &k in &held {
            let _ = state.update_key(xkb::Keycode::new(k + EVDEV_OFFSET), xkb::KeyDirection::Down);
        }
        table
            .iter()
            .filter(|(sym, s)| (s.shift, s.level3) == (want_shift, want_level3) && looked_up(**sym))
            .all(|(&sym, s)| {
                state
                    .key_get_one_sym(xkb::Keycode::new(s.key + EVDEV_OFFSET))
                    .raw()
                    == sym
            })
    })
}

/// Whether [`KeyTable::stroke`] ever looks up `sym`: it is the keysym (or the Unicode keysym)
/// of a printable character. Other keysyms in the table (`KP_Home`, `BackSpace`, ...) never
/// get typed, so a modifier that only changes those is harmless.
fn looked_up(sym: u32) -> bool {
    let Some(c) = char::from_u32(xkb::keysym_to_utf32(xkb::Keysym::new(sym))) else {
        return false;
    };
    c != '\0'
        && !c.is_control()
        && (xkb::Keysym::from_char(c).raw() == sym || 0x0100_0000 | u32::from(c) == sym)
}

/// Keycodes that have an evdev equivalent.
fn keycodes(keymap: &xkb::Keymap) -> impl Iterator<Item = xkb::Keycode> {
    (keymap.min_keycode().raw().max(EVDEV_OFFSET)..=keymap.max_keycode().raw())
        .map(xkb::Keycode::new)
}

/// `key` produces one of `syms` at level 1 of `group`, and pressing it sets some modifier but
/// not Control/Alt/Super.
fn is_safe_modifier(
    keymap: &xkb::Keymap,
    key: xkb::Keycode,
    group: u32,
    syms: &[xkb::Keysym],
    blocking: u32,
) -> bool {
    let produces = keymap
        .key_get_syms_by_level(key, group, 0)
        .first()
        .is_some_and(|s| syms.contains(s));
    if !produces {
        return false;
    }
    let mut state = xkb::State::new(keymap);
    state.update_mask(0, 0, 0, 0, 0, group);
    let _ = state.update_key(key, xkb::KeyDirection::Down);
    let set = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
    set != 0 && set & blocking == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Evdev keycodes from linux/input-event-codes.h.
    const KEY_1: u32 = 2;
    const KEY_Q: u32 = 16;
    const KEY_A: u32 = 30;
    const KEY_SLASH: u32 = 53;
    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_RIGHTALT: u32 = 100;
    const KEY_SPACE: u32 = 57;

    fn keymap(layout: &str) -> xkb::Keymap {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            layout,
            "",
            Some(String::new()),
            xkb::COMPILE_NO_FLAGS,
        )
        .expect("test keymap compiles")
    }

    fn table(layout: &str) -> KeyTable {
        KeyTable::new(&keymap(layout))
    }

    fn group(g: u32) -> ModState {
        ModState {
            group: g,
            ..ModState::default()
        }
    }

    fn stroke(key: u32, shift: bool, level3: bool) -> Option<Stroke> {
        Some(Stroke { key, shift, level3 })
    }

    #[test]
    fn us_letters_and_shifted_punctuation() {
        let t = table("us");
        let us = group(0);
        assert_eq!(t.stroke('a', &us), stroke(KEY_A, false, false));
        assert_eq!(t.stroke('A', &us), stroke(KEY_A, true, false));
        assert_eq!(t.stroke('/', &us), stroke(KEY_SLASH, false, false));
        assert_eq!(t.stroke('?', &us), stroke(KEY_SLASH, true, false));
        assert_eq!(t.stroke(' ', &us), stroke(KEY_SPACE, false, false));
        assert_eq!(t.stroke('1', &us), stroke(KEY_1, false, false));
        assert_eq!(t.stroke('ą', &us), None);
    }

    #[test]
    fn plan_wraps_each_char_in_its_modifiers() {
        let t = table("us");
        let plan = t.plan("A?", &group(0));
        let ev = |key, pressed| KeyEvent { key, pressed };
        assert_eq!(
            plan.events,
            vec![
                ev(KEY_LEFTSHIFT, true),
                ev(KEY_A, true),
                ev(KEY_A, false),
                ev(KEY_LEFTSHIFT, false),
                ev(KEY_LEFTSHIFT, true),
                ev(KEY_SLASH, true),
                ev(KEY_SLASH, false),
                ev(KEY_LEFTSHIFT, false),
            ]
        );
        assert_eq!((plan.unmapped, plan.control), (0, 0));
    }

    #[test]
    fn every_press_is_released() {
        let t = table("de");
        let plan = t.plan("Grüße @ 10€ {ok}! ", &group(0));
        assert_eq!(plan.unmapped, 0);
        let mut down = std::collections::HashSet::new();
        for e in &plan.events {
            if e.pressed {
                assert!(down.insert(e.key), "key {} pressed twice", e.key);
            } else {
                assert!(down.remove(&e.key), "key {} released while up", e.key);
            }
        }
        assert!(down.is_empty(), "keys left down: {down:?}");
    }

    #[test]
    fn lithuanian_letters_only_in_their_group() {
        let t = table("us,lt");
        assert_eq!(t.layouts(), ["English (US)", "Lithuanian"]);
        assert_eq!(t.stroke('ą', &group(0)), None);
        // `ą` is on the 1 key in lt; digits move to Level3 there.
        assert_eq!(t.stroke('ą', &group(1)), stroke(KEY_1, false, false));
        assert_eq!(t.stroke('1', &group(1)), stroke(KEY_1, false, true));
        assert_eq!(t.stroke('!', &group(1)), stroke(KEY_1, true, true));
        assert_eq!(t.plan("mąstau ", &group(0)).unmapped, 1);
        assert_eq!(t.plan("mąstau ", &group(1)).unmapped, 0);
        // Out-of-range group: nothing is typeable.
        assert_eq!(t.plan("ab", &group(7)).unmapped, 2);
    }

    #[test]
    fn german_at_sign_needs_altgr() {
        let t = table("de");
        assert_eq!(t.stroke('@', &group(0)), stroke(KEY_Q, false, true));
        let plan = t.plan("@", &group(0));
        assert_eq!(plan.events.first().map(|e| e.key), Some(KEY_RIGHTALT));
        assert_eq!(t.stroke('ß', &group(0)).map(|s| s.shift), Some(false));
    }

    #[test]
    fn caps_lock_inverts_the_shift_choice() {
        let km = keymap("us");
        let t = KeyTable::new(&km);
        let caps = ModState {
            locked: 1 << km.mod_get_index(xkb::MOD_NAME_CAPS),
            ..ModState::default()
        };
        assert_eq!(t.stroke('A', &caps), stroke(KEY_A, false, false));
        assert_eq!(t.stroke('a', &caps), stroke(KEY_A, true, false));
        // Caps Lock doesn't affect punctuation.
        assert_eq!(t.stroke('?', &caps), stroke(KEY_SLASH, true, false));
    }

    #[test]
    fn probe_key_is_level3_in_every_group() {
        // <LVL3> (evdev 84) is ISO_Level3_Shift in every group of xkeyboard-config keymaps,
        // unlike right Alt, which is Alt_R (a blocking modifier) on us.
        const KEY_LVL3: u32 = 84;
        for layout in ["us", "us,lt", "de,us", "lt"] {
            assert_eq!(table(layout).probe_key(), Some(KEY_LVL3), "{layout}");
        }
    }

    #[test]
    fn control_characters_are_never_typed() {
        let t = table("us");
        let plan = t.plan("a\nb\tc\r\u{8}\u{1b}", &group(0));
        assert_eq!(plan.control, 5);
        assert_eq!(plan.unmapped, 0);
        assert_eq!(plan.events.len(), 6);
    }

    fn keymap_variant(layout: &str, variant: &str, options: &str) -> xkb::Keymap {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            layout,
            variant,
            Some(options.to_owned()),
            xkb::COMPILE_NO_FLAGS,
        )
        .expect("test keymap compiles")
    }

    #[test]
    fn modifier_state_blocks_typing_unless_it_is_planned_for() {
        let km = keymap("de");
        let t = KeyTable::new(&km);
        let bit = |name| 1 << km.mod_get_index(name);
        let held = |name| ModState {
            depressed: bit(name),
            ..ModState::default()
        };
        let latched = |name| ModState {
            latched: bit(name),
            ..ModState::default()
        };
        let locked = |name| ModState {
            locked: bit(name),
            ..ModState::default()
        };
        // Control/Alt/Super would turn letters into shortcuts, however they are active
        // (a locked Control comes from sticky keys' lock mode). Shift/AltGr would change the
        // characters: held, latched (it would apply to the first key only) or locked.
        for name in [
            xkb::MOD_NAME_CTRL,
            xkb::MOD_NAME_ALT,
            xkb::MOD_NAME_LOGO,
            xkb::MOD_NAME_SHIFT,
            xkb::MOD_NAME_ISO_LEVEL3_SHIFT,
        ] {
            assert!(t.blocks_typing(&held(name)), "held {name} should block");
            assert!(
                t.blocks_typing(&latched(name)),
                "latched {name} should block"
            );
            assert!(t.blocks_typing(&locked(name)), "locked {name} should block");
        }
        let caps = ModState {
            depressed: bit(xkb::MOD_NAME_CAPS),
            locked: bit(xkb::MOD_NAME_CAPS),
            ..ModState::default()
        };
        assert!(
            !t.blocks_typing(&caps),
            "Caps Lock is planned for, not waited out"
        );
        assert!(
            t.blocks_typing(&latched(xkb::MOD_NAME_CAPS)),
            "a latched Caps Lock would apply to the first key only"
        );
        // Num Lock only affects the keypad, which the plan never uses.
        let num = ModState {
            depressed: bit(xkb::MOD_NAME_NUM),
            locked: bit(xkb::MOD_NAME_NUM),
            ..ModState::default()
        };
        assert!(
            !t.blocks_typing(&num),
            "Num Lock changes no planned key on de"
        );
        assert!(!t.blocks_typing(&ModState::default()));
    }

    #[test]
    fn num_lock_does_not_hold_text_on_common_layouts() {
        // KDE can turn Num Lock on at login: it must not hold dictation back forever.
        for (layout, variant) in [
            ("us", ""),
            ("de", ""),
            ("fr", ""),
            ("gb", ""),
            ("es", ""),
            ("it", ""),
            ("pl", ""),
            ("cz", ""),
            ("se", ""),
            ("ch", ""),
            ("tr", ""),
            ("jp", ""),
            ("us", "intl"),
            ("us,lt", ""),
            ("us,ru", ""),
            ("fr", "bepo"),
        ] {
            let km = keymap_variant(layout, variant, "");
            let t = KeyTable::new(&km);
            let lock = |name| 1u32 << km.mod_get_index(name);
            let mods = ModState {
                locked: lock(xkb::MOD_NAME_NUM) | lock(xkb::MOD_NAME_CAPS),
                ..ModState::default()
            };
            assert!(!t.blocks_typing(&mods), "{layout}({variant})");
        }
    }

    #[test]
    fn neo_level5_and_its_num_lock_level_lock_block_typing() {
        // Neo's level 5 (Mod3) and its level-4 lock (Num Lock's Mod2) turn letters into
        // navigation keys, Return, BackSpace and Delete.
        let km = keymap_variant("de", "neo", "");
        let t = KeyTable::new(&km);
        let bit = |name| 1u32 << km.mod_get_index(name);
        let held_mod3 = ModState {
            depressed: bit("Mod3"),
            ..ModState::default()
        };
        assert!(t.blocks_typing(&held_mod3), "held level 5 should block");
        let num_locked = ModState {
            locked: bit(xkb::MOD_NAME_NUM),
            ..ModState::default()
        };
        assert!(
            t.blocks_typing(&num_locked),
            "Neo's level lock should block"
        );
    }

    /// Replays `events` through an `xkb::State` in `mods`, like the compositor and the focused
    /// client do: returns the text the client sees, whether Control/Alt/Super was ever active
    /// and the depressed modifiers and group at the end.
    fn replay(km: &xkb::Keymap, events: &[KeyEvent], mods: &ModState) -> (String, bool, u32, u32) {
        let shortcut = mod_mask(km, xkb::MOD_NAME_CTRL)
            | mod_mask(km, xkb::MOD_NAME_ALT)
            | mod_mask(km, xkb::MOD_NAME_LOGO);
        let mut state = xkb::State::new(km);
        state.update_mask(mods.depressed, mods.latched, mods.locked, 0, 0, mods.group);
        let mut text = String::new();
        let mut shortcuts = false;
        for e in events {
            let key = xkb::Keycode::new(e.key + EVDEV_OFFSET);
            if e.pressed {
                text.push_str(&state.key_get_utf8(key));
                let _ = state.update_key(key, xkb::KeyDirection::Down);
            } else {
                let _ = state.update_key(key, xkb::KeyDirection::Up);
            }
            shortcuts |= state.serialize_mods(xkb::STATE_MODS_EFFECTIVE) & shortcut != 0;
        }
        (
            text,
            shortcuts,
            state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        )
    }

    #[test]
    fn every_typeable_character_round_trips_through_xkb() {
        let mut candidates: Vec<char> = (0x20u32..0x250)
            .chain(0x370..0x530)
            .chain(0x2010..0x2030)
            .chain(0x20a0..0x20c0)
            .filter_map(char::from_u32)
            .filter(|c| !c.is_control())
            .collect();
        candidates.extend("ąčęėįšųūž€«»„“”‘’–—…".chars());
        let configs = [
            ("us", "", ""),
            ("de", "", ""),
            ("fr", "", ""),
            ("us,lt", "", ""),
            ("us,ru", "", "grp:lshift_toggle"),
            ("cz", "", ""),
            ("us", "intl", ""),
            ("us", "dvorak", ""),
            ("fr", "bepo", ""),
            ("de", "neo", ""),
            ("tr", "", ""),
            ("de", "", "lv3:ralt_alt"),
            ("de,us", "", "grp:shifts_toggle"),
        ];
        let mut problems = Vec::new();
        for (layout, variant, options) in configs {
            let km = keymap_variant(layout, variant, options);
            let t = KeyTable::new(&km);
            let lock = |name| 1u32 << km.mod_get_index(name);
            for group in 0..km.num_layouts() {
                for locked in [0, lock(xkb::MOD_NAME_CAPS), lock(xkb::MOD_NAME_NUM)] {
                    let mods = ModState {
                        group,
                        locked,
                        ..ModState::default()
                    };
                    if t.blocks_typing(&mods) {
                        continue; // Waited out, never planned.
                    }
                    for &c in &candidates {
                        let plan = t.plan(&c.to_string(), &mods);
                        if plan.unmapped > 0 {
                            continue;
                        }
                        let got = replay(&km, &plan.events, &mods);
                        if got != (c.to_string(), false, 0, group) {
                            problems.push(format!(
                                "{layout}({variant}) [{options}] group {group} locked {locked:#x}: \
                                 {c:?} -> {got:?}"
                            ));
                        }
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn keymap_from_fd_ignores_the_file_offset() {
        use std::io::{Read, Write};
        let mut text = keymap("us,lt")
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
            .into_bytes();
        text.push(0);
        let path = std::env::temp_dir().join(format!("murmur-keymap-{}", std::process::id()));
        std::fs::File::create(&path)
            .and_then(|mut f| f.write_all(&text))
            .expect("write temp keymap");
        let mut file = std::fs::File::open(&path).expect("open temp keymap");
        std::fs::remove_file(&path).ok();
        // Leave the shared offset at the end, as another reader of the same fd might.
        file.read_to_end(&mut Vec::new()).expect("read temp keymap");
        let t = KeyTable::from_fd(file.into(), text.len()).expect("keymap loads");
        assert_eq!(t.layouts(), ["English (US)", "Lithuanian"]);
        let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
        assert!(matches!(
            KeyTable::from_fd(devnull.into(), 0),
            Err(KeymapError::Size(0))
        ));
    }
}

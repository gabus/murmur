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
    /// Control, Mod1 (Alt) and Mod4 (Super): while any is held, typed letters would become
    /// shortcuts.
    blocking_mask: u32,
    /// See [`KeyTable::probe_key`].
    probe_key: Option<u32>,
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
        let groups = (0..keymap.num_layouts())
            .map(|g| build_group(keymap, g, lock_mask, blocking_mask))
            .collect();
        Self {
            layouts,
            groups,
            lock_mask,
            blocking_mask,
            probe_key: probe_key(keymap, blocking_mask),
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
        self.probe_key
    }

    /// `true` while Control, Alt or Super is held: typing now would trigger shortcuts.
    /// Shift, Lock and Level3 don't count, because murmur handles those itself.
    pub fn blocks_typing(&self, mods: &ModState) -> bool {
        mods.depressed & self.blocking_mask != 0
    }

    /// How to type `c` in the state `mods`, or `None` if the active group can't produce it.
    pub fn stroke(&self, c: char, mods: &ModState) -> Option<Stroke> {
        let group = self.groups.get(usize::try_from(mods.group).ok()?)?;
        let caps = usize::from(mods.locked & self.lock_mask != 0);
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

/// See [`KeyTable::probe_key`].
fn probe_key(keymap: &xkb::Keymap, blocking: u32) -> Option<u32> {
    let everywhere = |key, syms: &[xkb::Keysym]| {
        (0..keymap.num_layouts()).all(|g| is_safe_modifier(keymap, key, g, syms, blocking))
    };
    keycodes(keymap)
        .find(|&k| everywhere(k, &[xkb::Keysym::ISO_Level3_Shift]))
        .or_else(|| {
            keymap
                .key_by_name("LFSH")
                .filter(|&k| everywhere(k, &[xkb::Keysym::Shift_L]))
        })
        .or_else(|| {
            keycodes(keymap).find(|&k| everywhere(k, &[xkb::Keysym::Shift_L, xkb::Keysym::Shift_R]))
        })
        .map(|k| k.raw() - EVDEV_OFFSET)
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

    #[test]
    fn held_ctrl_alt_super_block_but_shift_and_altgr_do_not() {
        let km = keymap("de");
        let t = KeyTable::new(&km);
        let held = |name| ModState {
            depressed: 1 << km.mod_get_index(name),
            ..ModState::default()
        };
        for name in [xkb::MOD_NAME_CTRL, xkb::MOD_NAME_ALT, xkb::MOD_NAME_LOGO] {
            assert!(t.blocks_typing(&held(name)), "{name} should block");
        }
        for name in [
            xkb::MOD_NAME_SHIFT,
            xkb::MOD_NAME_CAPS,
            xkb::MOD_NAME_ISO_LEVEL3_SHIFT,
        ] {
            assert!(!t.blocks_typing(&held(name)), "{name} should not block");
        }
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

//! The plugins' check marks on their own menu items, for a backend that
//! rebuilds its Plugins menu rather than keeping one — GTK and Cocoa.
//!
//! A Notepad++ plugin ticks its items by command id whenever it likes —
//! from `NPPN_TBMODIFICATION` or `NPPN_READY`, from one of its commands,
//! from a `DMN_CLOSE` about its panel — and a click never ticks or
//! unticks an item by itself: the mark changes only when the plugin says
//! so (`NPPM_SETMENUITEMCHECK`), or at load (`FuncItem._init2Check`).
//! A backend that rebuilds the menu every time it opens cannot keep a
//! mark on a menu item, so it keeps it here and paints every build from
//! it. That also makes the order of loading and menu-building
//! unobservable: a tick set before the menu has ever been built is simply
//! waiting here for it. Win32 keeps its Plugins menu for the session and
//! checks the native item instead.
//!
//! One type for both backends, for the reason `crate::docking` gives:
//! two copies of one rule is how two hosts drift.

use std::collections::{HashMap, HashSet};

use crate::ffi::FuncItem;

/// The recorded marks, keyed by command id. See the module docs.
#[derive(Debug, Default)]
pub struct PluginMenuChecks {
    /// Every command a loaded plugin's `FuncItem` array publishes — the
    /// only ids a mark is recorded for, which bounds the map by what the
    /// plugins published rather than by what a buggy one sends.
    commands: HashSet<i32>,
    /// The last mark recorded for each command. A command with no entry
    /// has never been ticked or unticked, and a backend that can tell
    /// the two apart draws its item as a plain one — no empty check box
    /// beside an action that is not a toggle, which is what an unchecked
    /// item looks like on Win32.
    marks: HashMap<i32, bool>,
}

impl PluginMenuChecks {
    /// Take in the commands a load pass's plugins publish: every
    /// `FuncItem` that runs something — a null `pFunc` is a separator,
    /// not a command. `_init2Check` ticks a command that has no mark yet;
    /// a mark already recorded is the plugin's later word and is kept.
    pub fn absorb<'a>(&mut self, funcs: impl IntoIterator<Item = &'a FuncItem>) {
        for f in funcs {
            if f.p_func.is_none() {
                continue;
            }
            self.commands.insert(f.cmd_id);
            if f.init2_check != 0 {
                self.marks.entry(f.cmd_id).or_insert(true);
            }
        }
    }

    /// Record a plugin's mark for `cmd_id`. `false` — nothing recorded —
    /// for an id no loaded plugin published as a command: a built-in
    /// `IDM_*` (the backends that keep this map none), a separator, or a
    /// stray value.
    #[must_use]
    pub fn set(&mut self, cmd_id: i32, checked: bool) -> bool {
        if !self.commands.contains(&cmd_id) {
            return false;
        }
        self.marks.insert(cmd_id, checked);
        true
    }

    /// The mark recorded for `cmd_id`, if the plugin has ever set one.
    #[must_use]
    pub fn get(&self, cmd_id: i32) -> Option<bool> {
        self.marks.get(&cmd_id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::PluginMenuChecks;
    use crate::ffi::{FuncItem, MENU_TITLE_LENGTH};

    unsafe extern "C" fn run() {}

    /// A `FuncItem` as a plugin publishes it: a command when it runs
    /// something, a separator when it does not.
    fn item(cmd_id: i32, is_command: bool, init_checked: bool) -> FuncItem {
        FuncItem {
            item_name: [0; MENU_TITLE_LENGTH],
            p_func: is_command.then_some(run as unsafe extern "C" fn()),
            cmd_id,
            init2_check: i32::from(init_checked),
            p_sh_key: std::ptr::null_mut(),
        }
    }

    /// Only a loaded plugin's own commands take a mark — not a built-in
    /// id, not a separator's slot, not a stray value — so the table is
    /// bounded by what the plugins published.
    #[test]
    fn a_mark_is_recorded_only_for_a_published_command() {
        let mut checks = PluginMenuChecks::default();
        checks.absorb(&[item(50_000, true, false), item(50_001, false, false)]);
        assert!(checks.set(50_000, true));
        assert_eq!(checks.get(50_000), Some(true));
        assert!(!checks.set(50_001, true), "a separator is no command");
        assert!(
            !checks.set(42_001, true),
            "a built-in IDM_* id is not mapped"
        );
        assert!(!checks.set(-1, false));
        assert_eq!(checks.get(50_001), None);
        assert_eq!(checks.get(42_001), None);
    }

    /// `_init2Check` ticks a command that has no mark yet and yields to
    /// one the plugin set since — a later load pass re-absorbing the same
    /// commands must not undo the plugin's own untick.
    #[test]
    fn init2check_seeds_a_mark_without_overriding_one() {
        let mut checks = PluginMenuChecks::default();
        checks.absorb(&[item(50_010, true, true), item(50_011, true, false)]);
        assert_eq!(checks.get(50_010), Some(true), "_init2Check ticks it");
        assert_eq!(checks.get(50_011), None, "no mark: drawn as a plain item");
        assert!(checks.set(50_010, false));
        checks.absorb(&[item(50_010, true, true)]);
        assert_eq!(
            checks.get(50_010),
            Some(false),
            "the plugin's untick survives"
        );
    }

    /// An untick is a mark too: an item the plugin has unticked is a
    /// toggle the user should see as one, empty, rather than a plain
    /// action.
    #[test]
    fn an_untick_is_recorded_as_a_mark() {
        let mut checks = PluginMenuChecks::default();
        checks.absorb(&[item(50_020, true, false)]);
        assert!(checks.set(50_020, false));
        assert_eq!(checks.get(50_020), Some(false));
    }
}

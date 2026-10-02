//! Limits on what a plugin can ask the host to make for it and keep.
//!
//! On the backends that make these things for a plugin and hold them —
//! Cocoa today — `NPPM_CREATESCINTILLAHANDLE` makes a Scintilla view the
//! host then keeps for the rest of the process: a plugin may hold the
//! view's direct-call pair (`SCI_GETDIRECTFUNCTION`), which nothing could
//! invalidate safely, and Notepad++ keeps every Scintilla it makes for
//! plugins until it exits too. `NPPM_ADDTOOLBARICON` adds a toolbar button
//! that stays for the session. What is kept needs a cap, or a plugin that
//! asks once per file it processes grows it without end. Win32 caps
//! neither: its Scintillas are the plugin's to destroy, so nothing is kept
//! there to bound, while its toolbar adds a button and an image per call,
//! uncapped.
//!
//! In this crate rather than in a backend so that every backend applying
//! them applies the same rule — the reason `crate::docking` gives: two
//! copies of one rule is how two hosts drift. A backend adopting them owes
//! two things the types cannot carry: it keeps the views' handles in a
//! table with [`MAX_PLUGIN_SCINTILLAS`] slots, reached with `get` rather
//! than indexing, and it reads [`PluginToolbarButtonSlot::Existing`]'s
//! index in the same list of commands it passed in.

/// The most Scintilla views the host makes for plugins, all told. On a
/// backend that keeps them, each is kept for the rest of the process, so
/// an unbounded number would be an unbounded leak — and a fixed number is
/// what lets such a backend keep the views' handles in a fixed table of
/// atomics, which any thread can read without a lock.
pub const MAX_PLUGIN_SCINTILLAS: usize = 64;

/// The most the host makes for any one plugin, so a plugin that asks for
/// a view per file it processes runs out of views of its own rather than
/// of everyone's — the same reasoning as the per-plugin quota on dock
/// panels ([`codepp_core::dock::MAX_PLUGIN_PANELS_PER_MODULE`]). Views
/// asked for from outside any host call, where the host cannot tell which
/// plugin asked, share one allowance of this size.
pub const MAX_PLUGIN_SCINTILLAS_PER_PLUGIN: usize = 16;

// A per-plugin allowance as large as the whole table would cap nothing.
const _: () = assert!(MAX_PLUGIN_SCINTILLAS_PER_PLUGIN < MAX_PLUGIN_SCINTILLAS);

/// Whether one more view may be made for `owner`, given the owners of
/// every view made so far. `owner` is the plugin the host was calling when
/// the view was asked for (`crate::calling_plugin`), or `None` when it was
/// asked for from outside any host call.
///
/// # Errors
///
/// A message for the log naming the limit that was reached:
/// [`MAX_PLUGIN_SCINTILLAS`] views in all, or
/// [`MAX_PLUGIN_SCINTILLAS_PER_PLUGIN`] for `owner`.
pub fn may_make_plugin_scintilla(
    made: &[Option<usize>],
    owner: Option<usize>,
) -> Result<(), &'static str> {
    if made.len() >= MAX_PLUGIN_SCINTILLAS {
        return Err("the host has made as many Scintilla views for plugins as it makes");
    }
    if made.iter().filter(|&&by| by == owner).count() >= MAX_PLUGIN_SCINTILLAS_PER_PLUGIN {
        return Err(
            "the host has made as many Scintilla views for this plugin as it makes for one",
        );
    }
    Ok(())
}

/// The most toolbar buttons plugins may add, all told. A button stays for
/// the session, so the cap bounds what a plugin adding one per call could
/// otherwise grow without end; asking again for the same command only
/// replaces its image ([`plugin_toolbar_button_slot`]).
pub const MAX_PLUGIN_TOOLBAR_BUTTONS: usize = 64;

/// Where a plugin's toolbar button for a command goes, given the commands
/// that already have one.
#[derive(Debug, PartialEq, Eq)]
pub enum PluginToolbarButtonSlot {
    /// The command's own button, at this index in the list of commands
    /// asked about: its image is replaced.
    Existing(usize),
    /// A new button — the first plugin button of all when `first`, which
    /// a separator then precedes.
    New { first: bool },
    /// None: the bar has [`MAX_PLUGIN_TOOLBAR_BUTTONS`] already.
    Full,
}

/// Where a plugin button for `cmd_id` goes, given `commands`, the
/// commands with a button, in order. A command keeps its one button
/// however often it asks, so asking again is never refused, not even at
/// the cap.
#[must_use]
pub fn plugin_toolbar_button_slot(commands: &[i32], cmd_id: i32) -> PluginToolbarButtonSlot {
    if let Some(index) = commands.iter().position(|&id| id == cmd_id) {
        PluginToolbarButtonSlot::Existing(index)
    } else if commands.len() >= MAX_PLUGIN_TOOLBAR_BUTTONS {
        PluginToolbarButtonSlot::Full
    } else {
        PluginToolbarButtonSlot::New {
            first: commands.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        may_make_plugin_scintilla, plugin_toolbar_button_slot, PluginToolbarButtonSlot,
        MAX_PLUGIN_SCINTILLAS, MAX_PLUGIN_SCINTILLAS_PER_PLUGIN, MAX_PLUGIN_TOOLBAR_BUTTONS,
    };

    /// The numbers the plugin headers and the coverage matrix publish —
    /// at most 16 views per plugin and 64 in all, 64 toolbar buttons —
    /// which the tests below are written in terms of, so a changed cap
    /// would otherwise pass them silently.
    #[test]
    fn the_caps_are_the_published_ones() {
        assert_eq!(MAX_PLUGIN_SCINTILLAS, 64);
        assert_eq!(MAX_PLUGIN_SCINTILLAS_PER_PLUGIN, 16);
        assert_eq!(MAX_PLUGIN_TOOLBAR_BUTTONS, 64);
    }

    /// A plugin that has had its allowance is refused, and that costs
    /// every other plugin nothing — the reason there is a per-plugin cap
    /// at all.
    #[test]
    fn one_plugin_spends_its_own_allowance_and_nobody_elses() {
        let mut made = Vec::new();
        for _ in 0..MAX_PLUGIN_SCINTILLAS_PER_PLUGIN {
            assert!(may_make_plugin_scintilla(&made, Some(3)).is_ok());
            made.push(Some(3));
        }
        assert!(may_make_plugin_scintilla(&made, Some(3)).is_err());
        assert!(may_make_plugin_scintilla(&made, Some(4)).is_ok());
        // Views asked for from outside any host call share an allowance
        // of their own, apart from every plugin's.
        assert!(may_make_plugin_scintilla(&made, None).is_ok());
    }

    /// Views asked for from outside any host call are one allowance
    /// between them.
    #[test]
    fn views_nobody_can_be_charged_for_share_one_allowance() {
        let made = vec![None; MAX_PLUGIN_SCINTILLAS_PER_PLUGIN];
        assert!(may_make_plugin_scintilla(&made, None).is_err());
        assert!(may_make_plugin_scintilla(&made, Some(0)).is_ok());
    }

    /// The table is full once every slot is taken, whoever took them.
    #[test]
    fn the_table_holds_no_more_than_its_slots() {
        let made: Vec<Option<usize>> = (0..MAX_PLUGIN_SCINTILLAS).map(Some).collect();
        assert!(may_make_plugin_scintilla(&made, Some(MAX_PLUGIN_SCINTILLAS)).is_err());
        assert!(may_make_plugin_scintilla(&made[1..], Some(MAX_PLUGIN_SCINTILLAS)).is_ok());
    }

    /// The first plugin button is the one a separator precedes; later
    /// ones are not.
    #[test]
    fn the_first_plugin_button_comes_after_a_separator() {
        assert_eq!(
            plugin_toolbar_button_slot(&[], 7),
            PluginToolbarButtonSlot::New { first: true }
        );
        assert_eq!(
            plugin_toolbar_button_slot(&[7], 8),
            PluginToolbarButtonSlot::New { first: false }
        );
    }

    /// A command that has a button keeps it: asking again replaces the
    /// image, even once the bar is full.
    #[test]
    fn a_command_keeps_its_one_button() {
        assert_eq!(
            plugin_toolbar_button_slot(&[5, 7, 9], 7),
            PluginToolbarButtonSlot::Existing(1)
        );
        let full: Vec<i32> = (0..).take(MAX_PLUGIN_TOOLBAR_BUTTONS).collect();
        assert_eq!(
            plugin_toolbar_button_slot(&full, 3),
            PluginToolbarButtonSlot::Existing(3)
        );
    }

    /// Past the cap a new command gets no button.
    #[test]
    fn the_bar_takes_no_more_than_the_cap() {
        let full: Vec<i32> = (0..).take(MAX_PLUGIN_TOOLBAR_BUTTONS).collect();
        assert_eq!(
            plugin_toolbar_button_slot(&full, -1),
            PluginToolbarButtonSlot::Full
        );
        assert_eq!(
            plugin_toolbar_button_slot(&full[1..], -1),
            PluginToolbarButtonSlot::New { first: false }
        );
    }
}

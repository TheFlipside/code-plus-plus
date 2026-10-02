//! The toolbar: 32 buttons in 10 separator-delimited groups, mirroring
//! `ui_win32`'s layout button-for-button.
//!
//! Buttons carry no command-dispatch logic of their own — each functional
//! button calls the exact same `menu`/`search` handler its menu item does,
//! so a toolbar click and a menu click share one code path. Buttons whose
//! underlying feature is not wired on GTK yet (the panel toggles, macros,
//! monitoring, sync-scroll, Define Language) are present but **greyed**, so
//! the bar matches Win32's layout exactly while never offering a dead click.
//!
//! # Icons
//!
//! The 24 px (`@1x`) and 48 px (`@2x`) PNGs under `assets/icons/` are
//! embedded with `include_bytes!` — the binary stays self-contained
//! (DESIGN.md §9), the same approach `ui_win32` and the tab strip take.
//! The `@2x` set is picked on a `HiDPI` display and drawn at scale 2
//! ([`crate::image_at_scale`]), so both fill the same 24-pixel cell.
//!
//! # Toggle state
//!
//! Word Wrap, Show All Characters and Show Indent Guide are toggle buttons
//! whose pressed state must track the live editor. They register into
//! [`crate::menu`]'s view-indicator registry so one `refresh_view_indicators`
//! keeps the toolbar toggles and the View-menu check items in agreement,
//! from either surface, guarded against re-entrancy. (Show Indent Guide is
//! toolbar-only — it has no View-menu check, matching Win32.)
//!
//! # Plugin buttons
//!
//! A plugin adds a button for one of its commands with
//! `NPPM_ADDTOOLBARICON` ([`add_plugin_button`]). They go after the
//! built-in groups, divided from them by one more separator, in the order
//! added — so the bar's overflow arrow reaches them first when the window
//! is narrow, and lists each under its command's label. A plugin button
//! shows its command's check mark as pressed, as the Plugins menu shows
//! it ticked, and Notepad++'s toolbar does the same.

use std::cell::{Cell, RefCell};
use std::io::Cursor;

use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::prelude::*;

use codepp_plugin_host::{plugin_toolbar_button_slot, PluginToolbarButtonSlot};

use crate::menu;
use crate::search;

/// Logical edge of a toolbar icon, in pixels. The `@2x` asset is twice
/// this, drawn at scale 2 so that it fills the same cell.
const ICON_LOGICAL_PX: i32 = 24;

/// `(png_1x, png_2x)` for one icon, embedded from `assets/icons/`.
macro_rules! icon {
    ($name:literal) => {
        (
            include_bytes!(concat!("../../../assets/icons/", $name, ".png")).as_slice(),
            include_bytes!(concat!("../../../assets/icons/", $name, "@2x.png")).as_slice(),
        )
    };
}

type IconPair = (&'static [u8], &'static [u8]);

/// Decode the scale-appropriate PNG into a `gtk::Image`, `None` on a
/// decode failure (a missing icon is cosmetic — never fatal, matching the
/// tab strip and the Win32 side).
fn icon_image(icons: IconPair, scale: i32) -> Option<gtk::Image> {
    let (bytes, asset_scale) = if scale >= 2 {
        (icons.1, 2)
    } else {
        (icons.0, 1)
    };
    match Pixbuf::from_read(Cursor::new(bytes)) {
        Ok(pixbuf) => crate::image_at_scale(&pixbuf, asset_scale),
        Err(err) => {
            tracing::warn!(
                ?err,
                "toolbar icon decode failed; button renders without it"
            );
            None
        }
    }
}

/// Append a push button bound to `action`, or greyed if `action` is
/// `None` (its feature is not wired on GTK yet).
///
/// `tip` is `'static` so it can double as the boundary's entry name: a
/// panic then names the button ("Save", "Undo"…) rather than the helper.
fn push(
    toolbar: &gtk::Toolbar,
    icons: IconPair,
    tip: &'static str,
    scale: i32,
    action: Option<fn()>,
) {
    let button = gtk::ToolButton::new(icon_image(icons, scale).as_ref(), None);
    WidgetExt::set_tooltip_text(&button, Some(tip));
    match action {
        Some(f) => {
            button.connect_clicked(move |_| crate::at_callback_boundary(tip, (), f));
        }
        None => button.set_sensitive(false),
    }
    toolbar.insert(&button, -1);
}

/// Append a greyed toggle button whose feature is not wired on GTK yet.
/// Kept as a toggle (not a push) so it looks identical to its Win32
/// counterpart.
fn disabled_toggle(toolbar: &gtk::Toolbar, icons: IconPair, tip: &str, scale: i32) {
    let button = gtk::ToggleToolButton::new();
    ToolButtonExt::set_icon_widget(&button, icon_image(icons, scale).as_ref());
    WidgetExt::set_tooltip_text(&button, Some(tip));
    button.set_sensitive(false);
    toolbar.insert(&button, -1);
}

/// Append a functional toggle button, returning it so the caller can
/// register it for state refresh.
fn toggle(toolbar: &gtk::Toolbar, icons: IconPair, tip: &str, scale: i32) -> gtk::ToggleToolButton {
    let button = gtk::ToggleToolButton::new();
    ToolButtonExt::set_icon_widget(&button, icon_image(icons, scale).as_ref());
    WidgetExt::set_tooltip_text(&button, Some(tip));
    toolbar.insert(&button, -1);
    button
}

/// Append a group separator.
fn separator(toolbar: &gtk::Toolbar) {
    toolbar.insert(&gtk::SeparatorToolItem::new(), -1);
}

/// Build the toolbar in Win32's exact 10-group order. `scale` is the
/// window's scale factor, choosing the `@1x`/`@2x` icon set.
pub fn build_toolbar(scale: i32) -> gtk::Toolbar {
    let toolbar = gtk::Toolbar::new();
    toolbar.set_style(gtk::ToolbarStyle::Icons);
    toolbar.set_icon_size(gtk::IconSize::LargeToolbar);
    toolbar.set_show_arrow(true);

    add_file_clipboard_history(&toolbar, scale);
    add_search_zoom_sync(&toolbar, scale);
    let (word_wrap, show_all_chars, indent_guide) = add_view_tools_macros(&toolbar, scale);

    // Hand the functional toggles to the view-indicator registry so one
    // refresh keeps them in agreement with the editor (and, for the first
    // two, the View-menu checks). Indent Guide is toolbar-only.
    menu::register_toolbar_view_toggles(word_wrap, show_all_chars, indent_guide);

    toolbar
}

/// Groups 1-3: File ops, Clipboard, History.
fn add_file_clipboard_history(toolbar: &gtk::Toolbar, scale: i32) {
    push(toolbar, icon!("new"), "New", scale, Some(menu::on_new));
    push(toolbar, icon!("open"), "Open…", scale, Some(menu::on_open));
    push(toolbar, icon!("save"), "Save", scale, Some(menu::on_save));
    push(
        toolbar,
        icon!("save-all"),
        "Save All",
        scale,
        Some(menu::on_save_all),
    );
    push(
        toolbar,
        icon!("close"),
        "Close",
        scale,
        Some(menu::on_close),
    );
    push(
        toolbar,
        icon!("close-all"),
        "Close All",
        scale,
        Some(menu::on_close_all),
    );
    push(
        toolbar,
        icon!("print"),
        "Print",
        scale,
        Some(crate::print::show),
    );
    separator(toolbar);

    push(toolbar, icon!("cut"), "Cut", scale, Some(menu::on_cut));
    push(toolbar, icon!("copy"), "Copy", scale, Some(menu::on_copy));
    push(
        toolbar,
        icon!("paste"),
        "Paste",
        scale,
        Some(menu::on_paste),
    );
    separator(toolbar);

    // History — always enabled (a no-op when there is nothing to undo/redo);
    // the dynamic grey-out Win32 does is a tracked follow-up.
    push(toolbar, icon!("undo"), "Undo", scale, Some(menu::on_undo));
    push(toolbar, icon!("redo"), "Redo", scale, Some(menu::on_redo));
    separator(toolbar);
}

/// Groups 4-6: Search, Zoom, Sync scroll (greyed — feature not on GTK).
fn add_search_zoom_sync(toolbar: &gtk::Toolbar, scale: i32) {
    push(
        toolbar,
        icon!("find"),
        "Find…",
        scale,
        Some(search::show_find),
    );
    push(
        toolbar,
        icon!("replace"),
        "Replace…",
        scale,
        Some(search::show_replace),
    );
    separator(toolbar);

    let zin = "Zoom In (Ctrl + Mouse Wheel Up)";
    let zout = "Zoom Out (Ctrl + Mouse Wheel Down)";
    push(
        toolbar,
        icon!("zoom-in"),
        zin,
        scale,
        Some(menu::on_zoom_in),
    );
    push(
        toolbar,
        icon!("zoom-out"),
        zout,
        scale,
        Some(menu::on_zoom_out),
    );
    separator(toolbar);

    let sv = icon!("sync-scroll-vertical");
    let sh = icon!("sync-scroll-horizontal");
    disabled_toggle(toolbar, sv, "Synchronize Vertical Scrolling", scale);
    disabled_toggle(toolbar, sh, "Synchronize Horizontal Scrolling", scale);
    separator(toolbar);
}

/// Groups 7-10: View toggles (Word Wrap, Show All Characters and Show
/// Indent Guide functional), Tools/panels (all greyed), Monitoring
/// (greyed), Macros (all greyed). Returns the three functional toggles.
fn add_view_tools_macros(
    toolbar: &gtk::Toolbar,
    scale: i32,
) -> (
    gtk::ToggleToolButton,
    gtk::ToggleToolButton,
    gtk::ToggleToolButton,
) {
    let word_wrap = toggle(toolbar, icon!("word-wrap"), "Word Wrap", scale);
    word_wrap.connect_toggled(|b| {
        crate::at_callback_boundary("toolbar:word_wrap:toggled", (), || {
            menu::on_word_wrap(b.is_active());
        });
    });
    let show_all_chars = toggle(
        toolbar,
        icon!("show-all-chars"),
        "Show All Characters",
        scale,
    );
    show_all_chars.connect_toggled(|b| {
        crate::at_callback_boundary("toolbar:show_all_chars:toggled", (), || {
            menu::on_show_all_chars(b.is_active());
        });
    });
    let indent_guide = toggle(
        toolbar,
        icon!("show-indent-guide"),
        "Show Indent Guide",
        scale,
    );
    indent_guide.connect_toggled(|b| {
        crate::at_callback_boundary("toolbar:indent_guide:toggled", (), || {
            menu::on_indent_guide(b.is_active());
        });
    });
    separator(toolbar);

    push(
        toolbar,
        icon!("define-language"),
        "Define your language…",
        scale,
        None,
    );
    // Document Map is wired: the toggle drives the right-side minimap
    // panel, kept in step with the View-menu check and the panel's close
    // button (guarded against the `set_active` feedback loop by
    // `docmap::syncing`), the same shape as Folder as Workspace below.
    let docmap = toggle(toolbar, icon!("document-map"), "Document Map", scale);
    docmap.connect_toggled(|b| {
        crate::at_callback_boundary("toolbar:docmap:toggled", (), || {
            if crate::docmap::syncing() {
                return;
            }
            crate::docmap::set_visible(b.is_active());
        });
    });
    crate::docmap::register_toolbar_toggle(docmap);
    disabled_toggle(toolbar, icon!("document-list"), "Document List", scale);
    disabled_toggle(toolbar, icon!("function-list"), "Function List", scale);
    // Folder as Workspace is wired: the toggle drives the side panel, and
    // the workspace module keeps it in step with the View-menu check and
    // the panel's own close button (guarded against the `set_active`
    // feedback loop by `workspace::syncing`).
    let workspace = toggle(
        toolbar,
        icon!("folder-workspace"),
        "Folder as Workspace",
        scale,
    );
    workspace.connect_toggled(|b| {
        crate::at_callback_boundary("toolbar:workspace:toggled", (), || {
            if crate::workspace::syncing() {
                return;
            }
            crate::workspace::set_visible(b.is_active());
        });
    });
    crate::workspace::register_toolbar_toggle(workspace);
    separator(toolbar);

    push(
        toolbar,
        icon!("monitoring"),
        "Monitoring (tail -f)",
        scale,
        None,
    );
    separator(toolbar);

    push(
        toolbar,
        icon!("macro-record"),
        "Start Recording",
        scale,
        None,
    );
    push(toolbar, icon!("macro-stop"), "Stop Recording", scale, None);
    push(toolbar, icon!("macro-play"), "Playback", scale, None);
    push(
        toolbar,
        icon!("run"),
        "Run a Macro Multiple Times…",
        scale,
        None,
    );
    push(
        toolbar,
        icon!("save-macro"),
        "Save Current Recorded Macro…",
        scale,
        None,
    );

    (word_wrap, show_all_chars, indent_guide)
}

thread_local! {
    /// The buttons plugins added, each with the command it runs, in the
    /// order added. See [`add_plugin_button`].
    static PLUGIN_BUTTONS: RefCell<Vec<(i32, gtk::ToggleToolButton)>> =
        const { RefCell::new(Vec::new()) };
    /// Set while the host itself changes a plugin button's state. GTK 3's
    /// `gtk_toggle_tool_button_set_active` clicks the button to change it,
    /// so `toggled` runs again, and without this that would run the
    /// plugin's command as if the user had clicked.
    static SYNCING_PLUGIN_BUTTONS: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    /// How many times a plugin button has run its command — what the
    /// display scenario counts, since it loads no plugin for a command to
    /// reach, and a command run twice is invisible otherwise.
    pub(crate) static COMMANDS_RUN: Cell<usize> = const { Cell::new(0) };
}

/// Add a button that runs plugin command `cmd_id`, showing `icon` —
/// `NPPM_ADDTOOLBARICON` — or give the button that command already has
/// this icon, keeping the one it has if this one cannot be drawn.
/// `label` is its tooltip, and its name in the bar's overflow menu, shown
/// as given: the caller passes the command's menu label, which
/// `crate::plugin` sanitized for display when it recorded it. `checked`
/// is whether it starts pressed. Refused once
/// [`MAX_PLUGIN_TOOLBAR_BUTTONS`](codepp_plugin_host::MAX_PLUGIN_TOOLBAR_BUTTONS)
/// are in place — see [`plugin_toolbar_button_slot`].
///
/// The icon is drawn at its own size, scaled down if it is larger than
/// the built-in icons' cell ([`plugin_icon_size`]): a plugin's icon is
/// whatever size the plugin made it, where the built-in ones are all the
/// same.
///
/// Called from inside the NPPM dispatch's state borrow. Nothing here
/// runs a plugin command: a button's state is changed quietly, and a new
/// one is given its state before its `toggled` is connected.
pub(crate) fn add_plugin_button(
    toolbar: &gtk::Toolbar,
    cmd_id: i32,
    icon: &Pixbuf,
    label: &str,
    checked: bool,
) -> Result<(), &'static str> {
    let Some(commands) = PLUGIN_BUTTONS.with(|buttons| {
        buttons
            .try_borrow()
            .ok()
            .map(|buttons| buttons.iter().map(|(id, _)| *id).collect::<Vec<i32>>())
    }) else {
        return Err("the toolbar's plugin buttons are being changed already");
    };
    match plugin_toolbar_button_slot(&commands, cmd_id) {
        PluginToolbarButtonSlot::Existing(index) => {
            // The same list the slot was asked about, and nothing has been
            // added to it since.
            let Some(button) = PLUGIN_BUTTONS.with(|buttons| {
                buttons
                    .try_borrow()
                    .ok()
                    .and_then(|buttons| buttons.get(index).map(|(_, b)| b.clone()))
            }) else {
                return Err("the toolbar's plugin buttons are being changed already");
            };
            if let Some(image) = plugin_icon_image(icon, toolbar.scale_factor()) {
                ToolButtonExt::set_icon_widget(&button, Some(&image));
                image.show();
            }
            WidgetExt::set_tooltip_text(&button, Some(label));
            ToolButtonExt::set_label(&button, Some(label));
            set_state_quietly(&button, checked);
            Ok(())
        }
        PluginToolbarButtonSlot::Full => Err("the toolbar has as many plugin buttons as it takes"),
        PluginToolbarButtonSlot::New { first } => {
            let button = gtk::ToggleToolButton::new();
            let image = plugin_icon_image(icon, toolbar.scale_factor());
            ToolButtonExt::set_icon_widget(&button, image.as_ref());
            WidgetExt::set_tooltip_text(&button, Some(label));
            ToolButtonExt::set_label(&button, Some(label));
            // Seeded before `toggled` is connected, so it runs nothing.
            button.set_active(checked);
            button.connect_toggled(move |b| {
                crate::at_callback_boundary("toolbar:plugin:toggled", (), || {
                    on_plugin_button_toggled(cmd_id, b);
                });
            });
            let recorded = PLUGIN_BUTTONS.with(|buttons| {
                let mut buttons = buttons.try_borrow_mut().ok()?;
                buttons.push((cmd_id, button.clone()));
                Some(())
            });
            if recorded.is_none() {
                return Err("the toolbar's plugin buttons are being changed already");
            }
            // The bar was shown at startup, so what joins it later is
            // shown here.
            if first {
                let line = gtk::SeparatorToolItem::new();
                toolbar.insert(&line, -1);
                line.show();
            }
            toolbar.insert(&button, -1);
            button.show_all();
            Ok(())
        }
    }
}

/// Show plugin command `cmd_id`'s check mark on its button, if it has
/// one — `NPPM_SETMENUITEMCHECK` marks both, as in Notepad++.
pub(crate) fn set_plugin_button_state(cmd_id: i32, checked: bool) {
    let button = PLUGIN_BUTTONS.with(|buttons| {
        buttons.try_borrow().ok().and_then(|buttons| {
            buttons
                .iter()
                .find(|(id, _)| *id == cmd_id)
                .map(|(_, b)| b.clone())
        })
    });
    if let Some(button) = button {
        set_state_quietly(&button, checked);
    }
}

/// A click on a plugin's toolbar button: show the plugin's own mark
/// again, then run the command.
///
/// A toggle tool button changes its state in its class handler, before
/// `toggled` reaches here; a Notepad++ button shows only what the plugin
/// says. So whatever the click did to the state is put back from the
/// plugin's record at once — before the command runs, so a command that
/// opens a modal dialog does not leave the button looking pressed behind
/// it. A mark the command sets, it sets through `NPPM_SETMENUITEMCHECK`,
/// which reaches the button itself ([`set_plugin_button_state`]). The
/// same rule the plugin's menu item keeps.
fn on_plugin_button_toggled(cmd_id: i32, button: &gtk::ToggleToolButton) {
    if SYNCING_PLUGIN_BUTTONS.with(Cell::get) {
        // The host changing the state itself, not a click.
        return;
    }
    set_state_quietly(button, crate::plugin::menu_mark(cmd_id));
    #[cfg(test)]
    COMMANDS_RUN.with(|runs| runs.set(runs.get() + 1));
    crate::plugin::on_plugin_command(cmd_id);
}

/// Set a plugin button's state without its `toggled` running a command.
/// The flag is held only around `set_active`, and the one handler that
/// runs inside it returns at once, so it is never held already here.
fn set_state_quietly(button: &gtk::ToggleToolButton, checked: bool) {
    if button.is_active() == checked {
        return;
    }
    let _syncing = crate::FlagGuard::set(&SYNCING_PLUGIN_BUTTONS);
    button.set_active(checked);
}

/// A plugin's toolbar icon as an image for the bar: [`plugin_icon_size`],
/// rendered for the bar's scale factor so it stays sharp on a high-DPI
/// screen ([`crate::image_at_scale`]). `None` if it cannot be drawn at
/// that scale — a new button then shows its label instead, which is
/// cosmetic.
pub(crate) fn plugin_icon_image(icon: &Pixbuf, scale: i32) -> Option<gtk::Image> {
    let scale = scale.max(1);
    let (width, height) = plugin_icon_size(icon.width(), icon.height());
    let scaled = icon.scale_simple(
        width.checked_mul(scale)?,
        height.checked_mul(scale)?,
        InterpType::Bilinear,
    )?;
    crate::image_at_scale(&scaled, scale)
}

/// The logical size a plugin icon of `width` × `height` pixels is drawn
/// at: its own, or scaled down to fit [`ICON_LOGICAL_PX`] square with its
/// aspect kept. Never scaled up, and never below one pixel a side.
fn plugin_icon_size(width: i32, height: i32) -> (i32, i32) {
    let (width, height) = (width.max(1), height.max(1));
    let longest = width.max(height);
    if longest <= ICON_LOGICAL_PX {
        return (width, height);
    }
    // In `i64`, so no pixbuf is large enough to overflow it; the result
    // is at most `ICON_LOGICAL_PX`.
    let fit = |side: i32| {
        let scaled = i64::from(side) * i64::from(ICON_LOGICAL_PX) / i64::from(longest);
        i32::try_from(scaled).unwrap_or(ICON_LOGICAL_PX).max(1)
    };
    (fit(width), fit(height))
}

/// The cap on plugin toolbar buttons is applied in [`add_plugin_button`],
/// unconditionally, before a button is made — the twin of `ui_cocoa`'s
/// guard of the same name, whose comment gives the reason: the rule lives
/// in `codepp_plugin_host`, where nothing warns if its use is neutered,
/// and each button stays for the session. A button is made and recorded
/// only where the cap says a new one goes, a full bar is refused, and
/// nothing else adds a plugin button.
#[cfg(test)]
mod plugin_cap_guard {
    use crate::source_scan::{
        block_after, code_only, fn_body, occurs_at_depth_one, strip_test_modules,
    };

    #[test]
    fn the_plugin_toolbar_cap_is_applied_before_a_button_is_made() {
        let toolbar = strip_test_modules(&code_only(include_str!("toolbar.rs")));
        let add = fn_body(&toolbar, "add_plugin_button");
        let counted = "buttons.iter().map(|(id, _)| *id).collect::<Vec<i32>>()";
        let asked = "match plugin_toolbar_button_slot(&commands, cmd_id) {";
        let commands = add
            .find(counted)
            .expect("the cap no longer counts every plugin button")
            + counted.len();
        let slot = add
            .find(asked)
            .expect("`add_plugin_button` no longer asks the cap where a button goes");
        assert!(
            commands <= slot,
            "the cap must count every button before it is asked"
        );
        assert!(
            !add[commands..slot].trim_start().starts_with('.')
                && !add[commands..slot].contains("let "),
            "the list of buttons is cut short or rebound before the cap is asked"
        );
        assert!(
            occurs_at_depth_one(&add, asked),
            "the cap is asked only under a condition, so some buttons go uncounted"
        );
        // Made and recorded only in the arm for a new button: the one
        // answer the cap gives below its limit. Made unconditionally there;
        // recorded inside the table's `with`, so a level deeper.
        let new = block_after(&add, "PluginToolbarButtonSlot::New { first } => {");
        let made = "gtk::ToggleToolButton::new()";
        assert!(
            add.matches(made).count() == 1 && occurs_at_depth_one(&new, made),
            "a plugin button is made somewhere besides where the cap says a new one goes"
        );
        let recorded = ".push((cmd_id,";
        assert!(
            add.matches(recorded).count() == 1 && new.contains(recorded),
            "a plugin button is recorded somewhere besides where the cap says a new one goes"
        );
        // And nowhere else in the module: the table's one mutable borrow
        // is that one, so no other function can add to it past the cap,
        // whatever it names its variables.
        for (what, needle) in [
            ("borrows the table mutably", "borrow_mut("),
            ("pushes a pair", ".push(("),
        ] {
            assert_eq!(
                toolbar.matches(needle).count(),
                1,
                "toolbar.rs {what} somewhere besides `add_plugin_button`"
            );
        }
        assert!(
            add.contains("PluginToolbarButtonSlot::Full => Err("),
            "a full bar no longer refuses the button"
        );
    }
}

/// A click puts the plugin's mark back before the command runs, so a
/// command that opens a modal dialog does not leave its button looking
/// pressed — an order the display scenario cannot see, since it loads no
/// plugin whose command could take its time.
#[cfg(test)]
mod click_order_guard {
    use crate::source_scan::{code_only, fn_body, occurs_at_depth_one, strip_test_modules};

    #[test]
    fn a_click_restores_the_mark_before_the_command_runs() {
        let toolbar = strip_test_modules(&code_only(include_str!("toolbar.rs")));
        let toggled = fn_body(&toolbar, "on_plugin_button_toggled");
        let restore = "set_state_quietly(button, crate::plugin::menu_mark(cmd_id));";
        let run = "crate::plugin::on_plugin_command(cmd_id);";
        assert!(
            occurs_at_depth_one(&toggled, restore) && occurs_at_depth_one(&toggled, run),
            "a click no longer both restores the mark and runs the command, unconditionally"
        );
        assert_eq!(
            toggled.matches("on_plugin_command(").count(),
            1,
            "a click runs its command some other number of times than once"
        );
        assert!(
            toggled.find(restore) < toggled.find(run),
            "the mark must be restored before the command runs"
        );
    }
}

/// The built-in icons drawn for real, at both scales. Display-gated,
/// driven by `crate::display_tests`.
#[cfg(test)]
pub(crate) mod icon_display_tests {
    use gtk::prelude::*;

    use super::icon_image;

    /// A built-in icon fills its 24-pixel cell at either scale, with twice
    /// the pixels at scale 2: the cell `set_pixel_size` was once trusted
    /// to keep, which a pixbuf image ignores.
    pub(crate) fn built_in_icons_keep_their_cell() {
        gtk::init().expect("gtk::init failed — no display?");
        for (scale, pixels) in [(1, 24), (2, 48)] {
            let image = icon_image(icon!("save"), scale).expect("the icon decodes");
            // GTK 3 sizes a shown widget only.
            image.show();
            assert_eq!(
                image.preferred_width().1,
                24,
                "a built-in icon left its cell at scale {scale}"
            );
            let surface = image
                .property::<Option<gtk::cairo::Surface>>("surface")
                .expect("drawn as a surface");
            let surface = gtk::cairo::ImageSurface::try_from(surface).expect("an image surface");
            assert_eq!(
                surface.width(),
                pixels,
                "a built-in icon is not drawn at the screen's pixels at scale {scale}"
            );
        }
    }
}

#[cfg(test)]
mod plugin_icon_tests {
    use super::{plugin_icon_size, ICON_LOGICAL_PX};

    /// An icon that fits is drawn at its own size — a 16 px icon is not
    /// blown up to the built-in 24.
    #[test]
    fn an_icon_that_fits_keeps_its_size() {
        assert_eq!(plugin_icon_size(16, 16), (16, 16));
        assert_eq!(
            plugin_icon_size(ICON_LOGICAL_PX, ICON_LOGICAL_PX),
            (ICON_LOGICAL_PX, ICON_LOGICAL_PX)
        );
        assert_eq!(plugin_icon_size(20, 12), (20, 12));
    }

    /// A larger one is scaled down to the cell with its aspect kept.
    #[test]
    fn a_larger_icon_is_scaled_down_to_the_cell() {
        assert_eq!(plugin_icon_size(48, 48), (24, 24));
        assert_eq!(plugin_icon_size(48, 24), (24, 12));
        assert_eq!(plugin_icon_size(30, 60), (12, 24));
    }

    /// Degenerate sizes still give a pixel a side, and a huge one does
    /// not overflow.
    #[test]
    fn degenerate_sizes_stay_drawable() {
        assert_eq!(plugin_icon_size(0, 0), (1, 1));
        assert_eq!(plugin_icon_size(1, 100), (1, 24));
        assert_eq!(plugin_icon_size(i32::MAX, i32::MAX), (24, 24));
    }
}

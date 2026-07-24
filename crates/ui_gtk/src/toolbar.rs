//! The toolbar: 32 buttons in 10 separator-delimited groups, mirroring
//! `ui_win32`'s layout button-for-button.
//!
//! Buttons carry no command-dispatch logic of their own — each functional
//! button calls the exact same `menu`/`search` handler its menu item does,
//! so a toolbar click and a menu click share one code path. Buttons whose
//! underlying feature is not wired on GTK yet (Print, the panel toggles,
//! macros, monitoring, sync-scroll, Define Language, indent guide) are
//! present but **greyed**, so the bar matches Win32's layout exactly while
//! never offering a dead click.
//!
//! # Icons
//!
//! The 24 px (`@1x`) and 48 px (`@2x`) PNGs under `assets/icons/` are
//! embedded with `include_bytes!` — the binary stays self-contained
//! (DESIGN.md §9), the same approach `ui_win32` and the tab strip take.
//! The `@2x` set is picked on a `HiDPI` display.
//!
//! # Toggle state
//!
//! Word Wrap and Show All Characters are toggle buttons whose pressed
//! state must track the live editor. They register into [`crate::menu`]'s
//! view-indicator registry so one `refresh_view_indicators` keeps the
//! toolbar toggles and the View-menu check items in agreement, from either
//! surface, guarded against re-entrancy.

use std::io::Cursor;

use gtk::gdk_pixbuf::Pixbuf;
use gtk::prelude::*;

use crate::menu;
use crate::search;

/// Logical edge of a toolbar icon, in pixels. The `@2x` asset is twice
/// this; `set_pixel_size` pins the logical square so both scales occupy
/// the same cell — the same normalisation the tab strip uses.
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
    let bytes = if scale >= 2 { icons.1 } else { icons.0 };
    match Pixbuf::from_read(Cursor::new(bytes)) {
        Ok(pixbuf) => {
            let image = gtk::Image::from_pixbuf(Some(&pixbuf));
            image.set_pixel_size(ICON_LOGICAL_PX);
            Some(image)
        }
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
fn push(toolbar: &gtk::Toolbar, icons: IconPair, tip: &str, scale: i32, action: Option<fn()>) {
    let button = gtk::ToolButton::new(icon_image(icons, scale).as_ref(), None);
    WidgetExt::set_tooltip_text(&button, Some(tip));
    match action {
        Some(f) => {
            button.connect_clicked(move |_| f());
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
    let (word_wrap, show_all_chars) = add_view_tools_macros(&toolbar, scale);

    // Hand the two functional toggles to the view-indicator registry so
    // one refresh keeps them and the View-menu checks in agreement.
    menu::register_toolbar_view_toggles(word_wrap, show_all_chars);

    toolbar
}

/// Groups 1-3: File ops (Print greyed), Clipboard, History.
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
    push(toolbar, icon!("print"), "Print", scale, None);
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

/// Groups 7-10: View toggles (Word Wrap + Show All Characters functional,
/// Show Indent Guide greyed), Tools/panels (all greyed), Monitoring
/// (greyed), Macros (all greyed). Returns the two functional toggles.
fn add_view_tools_macros(
    toolbar: &gtk::Toolbar,
    scale: i32,
) -> (gtk::ToggleToolButton, gtk::ToggleToolButton) {
    let word_wrap = toggle(toolbar, icon!("word-wrap"), "Word Wrap", scale);
    word_wrap.connect_toggled(|b| menu::on_word_wrap(b.is_active()));
    let show_all_chars = toggle(
        toolbar,
        icon!("show-all-chars"),
        "Show All Characters",
        scale,
    );
    show_all_chars.connect_toggled(|b| menu::on_show_all_chars(b.is_active()));
    disabled_toggle(
        toolbar,
        icon!("show-indent-guide"),
        "Show Indent Guide",
        scale,
    );
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
        if crate::docmap::syncing() {
            return;
        }
        crate::docmap::set_visible(b.is_active());
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
        if crate::workspace::syncing() {
            return;
        }
        crate::workspace::set_visible(b.is_active());
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

    (word_wrap, show_all_chars)
}

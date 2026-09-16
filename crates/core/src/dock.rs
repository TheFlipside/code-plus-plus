//! Dockable panel layout model — the cross-platform policy layer of
//! the plugin-panel docking subsystem.
//!
//! Phase 4.6/5 shipped "Folder as Workspace" and "Document Map" as
//! fixed, per-backend side columns. This module replaces that shape
//! with the general model both panels — and future plugin panels —
//! dock through: a panel lives in a *group*, a group is docked to one
//! of the four sides of the dock area or floats above it, a group
//! holds one or more panels as tabs (active one visible), and the
//! user rearranges everything by dragging captions and tabs.
//!
//! The split follows the same policy/mechanism discipline as
//! `core::shortcuts`: **everything decidable without a window system
//! is decided here** — group/tab bookkeeping, band geometry, splitter
//! placement, drop-target resolution, drag-hint rectangles, clamps,
//! persistence — and the three UI backends contribute only mechanism
//! (moving native windows/widgets, mouse capture, painting the hint).
//! That is not stylistic: only one backend has a runner on any given
//! development host, so logic that lives here is exercised by
//! `cargo test` on every CI runner while per-backend logic is
//! exercised on exactly one. The geometry in particular is where the
//! interesting failures are off-by-ones at band boundaries, which a
//! hands-on demo is worst at catching — the same reasoning
//! `resolve_tab_arm_commit` and the Cocoa tab-strip shuffle math
//! record.
//!
//! Coordinate space: this module never converts coordinates. Every
//! rect and cursor position handed to a function here must be in
//! *one consistent space chosen by the caller* (Win32 uses screen
//! coordinates because floating groups are screen-positioned windows;
//! GTK and Cocoa make their own choice). The model only compares and
//! carves rectangles — it cannot tell spaces apart, so mixing them is
//! a caller bug this module cannot detect.

use crate::session::{DockGroupSession, DockPanelSession, DockRememberSession, DockSession};

/// Thickness of the resize splitter between a docked band and the
/// editor cell, in pixels. Matches the 4-px value the fixed
/// workspace/docmap splitters shipped with on every backend.
pub const DOCK_SPLITTER_PX: i32 = 4;

/// Minimum band thickness (width of a Left/Right band, height of a
/// Top/Bottom band). Below this the panel content is unusable; the
/// clamp in [`compute_frame`] and [`DockLayout::set_side_size`]
/// enforces it. 80 px is the old docmap floor — the tighter of the
/// two panels' historical minimums that still leaves a legible
/// miniature/tree.
pub const MIN_DOCK_BAND_PX: i32 = 80;

/// Ceiling for a persisted band thickness. `session.xml` is
/// hand-editable and a crash can truncate it mid-write, so a value
/// read from disk is bounded before it can drive layout — same
/// discipline as `Styles::clamp` and the Cocoa docmap width clamp.
pub const MAX_DOCK_BAND_PX: i32 = 4000;

/// Distance from a dock-area edge within which a drag resolves to
/// "dock to that side" rather than "join the group under the cursor"
/// or "float". Chosen wide enough to hit while moving quickly, narrow
/// enough that a drop on a docked group's body still reads as a
/// join.
pub const EDGE_DOCK_ZONE_PX: i32 = 32;

/// Default size of a newly floated group, used when a panel is
/// dropped into empty space and no better size is known (the caller
/// may substitute the group's current docked size instead).
pub const DEFAULT_FLOAT_W: i32 = 280;
/// See [`DEFAULT_FLOAT_W`].
pub const DEFAULT_FLOAT_H: i32 = 380;

/// Minimum floating-group dimensions. A floating rect read from
/// `session.xml` or produced by a user resize is clamped to this so
/// the caption (the only re-dock affordance) can never collapse away.
pub const MIN_FLOAT_W: i32 = 120;
/// See [`MIN_FLOAT_W`].
pub const MIN_FLOAT_H: i32 = 100;

/// Default band thickness per side, used the first time a side is
/// occupied. Left carries the workspace tree's historical 240-px
/// default; Right the document map's 160; Top/Bottom get the same
/// 160 (nothing has shipped there yet, and a shallow band is the
/// safer first impression for a horizontal strip).
const DEFAULT_SIDE_SIZE: [i32; 4] = [240, 160, 160, 160];

/// A dockable panel. Today the two Phase 4.6 panels; future plugin
/// panels extend this enum (or, once panels become dynamically
/// registered, replace it with an id space — the persistence layer
/// already keys on a string so the wire format survives that change).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DockPanel {
    /// "Folder as Workspace" — the lazily-populated folder tree.
    Workspace,
    /// "Document Map" — the miniature second Scintilla view.
    DocMap,
}

impl DockPanel {
    /// Every panel, for iteration. Order is presentation-neutral.
    pub const ALL: [DockPanel; 2] = [DockPanel::Workspace, DockPanel::DocMap];

    /// Human-readable title shown in the group caption and on the
    /// panel's tab when active.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            DockPanel::Workspace => "Folder as Workspace",
            DockPanel::DocMap => "Document Map",
        }
    }

    /// Stable key used in `session.xml`. Never rename a value here —
    /// the strings are the wire format.
    #[must_use]
    pub fn persist_key(self) -> &'static str {
        match self {
            DockPanel::Workspace => "workspace",
            DockPanel::DocMap => "docmap",
        }
    }

    /// Inverse of [`Self::persist_key`]. `None` for an unknown key —
    /// a session written by a future build with more panels loads
    /// with those panels silently dropped rather than erroring, the
    /// same forward-compatibility posture the rest of `session.xml`
    /// takes.
    #[must_use]
    pub fn from_persist_key(key: &str) -> Option<DockPanel> {
        match key {
            "workspace" => Some(DockPanel::Workspace),
            "docmap" => Some(DockPanel::DocMap),
            _ => None,
        }
    }

    /// The side a panel docks to the first time it is shown with no
    /// remembered location — the pre-dock fixed positions, kept so
    /// the refactor changes nothing for a user who never drags.
    #[must_use]
    pub fn default_side(self) -> DockSide {
        match self {
            DockPanel::Workspace => DockSide::Left,
            DockPanel::DocMap => DockSide::Right,
        }
    }
}

/// One of the four dockable edges of the dock area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DockSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl DockSide {
    /// All four sides in the order [`compute_frame`] carves them.
    pub const ALL: [DockSide; 4] = [
        DockSide::Left,
        DockSide::Right,
        DockSide::Top,
        DockSide::Bottom,
    ];

    fn index(self) -> usize {
        match self {
            DockSide::Left => 0,
            DockSide::Right => 1,
            DockSide::Top => 2,
            DockSide::Bottom => 3,
        }
    }

    /// Stable key used in `session.xml`.
    #[must_use]
    pub fn persist_key(self) -> &'static str {
        match self {
            DockSide::Left => "left",
            DockSide::Right => "right",
            DockSide::Top => "top",
            DockSide::Bottom => "bottom",
        }
    }

    /// Inverse of [`Self::persist_key`].
    #[must_use]
    pub fn from_persist_key(key: &str) -> Option<DockSide> {
        match key {
            "left" => Some(DockSide::Left),
            "right" => Some(DockSide::Right),
            "top" => Some(DockSide::Top),
            "bottom" => Some(DockSide::Bottom),
            _ => None,
        }
    }
}

/// An axis-aligned rectangle in the caller's coordinate space.
/// `w`/`h` are extents, not far edges; negative extents never appear
/// in values this module produces (carve math clamps at zero).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DockRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl DockRect {
    #[must_use]
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        DockRect { x, y, w, h }
    }

    /// True iff the point is inside (right/bottom edges exclusive —
    /// the half-open convention native hit tests use).
    #[must_use]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Where a group lives: docked into a side band, or floating above
/// the main window at a caller-space rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockLocation {
    Side(DockSide),
    Floating(DockRect),
}

/// A group of one or more panels sharing one window slot. The
/// panels are tabs; `active` picks the visible one. Invariants
/// (upheld by every `DockLayout` mutation, checked by
/// `debug_assert_invariants`):
///
///   * `panels` is non-empty — an emptied group is removed, never
///     kept as a husk;
///   * no panel appears twice, in this group or any other;
///   * `active < panels.len()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockGroup {
    /// Session-stable identity. Monotonically allocated, never
    /// reused — the same "key on ids, not indices or pointers"
    /// rule the tab strip's arm/commit fix established (§7.4).
    pub id: u32,
    pub location: DockLocation,
    /// Tab order, left to right.
    pub panels: Vec<DockPanel>,
    /// Index into `panels` of the visible tab.
    pub active: usize,
}

impl DockGroup {
    /// The panel whose content is currently shown.
    #[must_use]
    pub fn active_panel(&self) -> DockPanel {
        // Invariant: active < panels.len() and panels non-empty.
        self.panels[self.active.min(self.panels.len().saturating_sub(1))]
    }
}

/// What is being dragged: a single panel (grabbed by its tab) or a
/// whole group (grabbed by its caption bar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragSubject {
    Panel(DockPanel),
    Group(u32),
}

/// Where a drag would land if released now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropTarget {
    /// Dock to this side, as a new group appended to the side's
    /// stack (an already-docked band gains a stacked neighbour, not
    /// a tab — dropping *onto a group's body* is what tabs).
    Side(DockSide),
    /// Join this group as a tab (appended, made active).
    IntoGroup(u32),
    /// Float at this rect.
    Floating(DockRect),
}

/// The hit-test inputs for [`resolve_drop`]. The caller measures
/// these from live windows each time the cursor moves; the resolver
/// is pure so the mapping from (cursor, zones) to (target, hint) is
/// unit-testable without a window system.
#[derive(Clone, Debug, Default)]
pub struct DropZones {
    /// The dock area: the region between toolbar-bottom and
    /// status-bar-top of the main window, in the caller's space.
    pub mid: DockRect,
    /// Every group's current on-screen rect, **topmost first** —
    /// floating groups before docked ones, so a float overlapping a
    /// docked band wins the hit the same way it wins the paint.
    pub groups: Vec<(u32, DockRect)>,
}

/// The computed pixel layout of every docked band, group and
/// splitter, plus what is left for the editor cell. Produced by
/// [`compute_frame`]; backends apply it verbatim with native move
/// calls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DockFrame {
    /// What remains of the dock area after every occupied band is
    /// carved out: the cell holding tab strip + editor + FIF dock.
    pub editor: DockRect,
    /// One entry per *occupied* side, in [`DockSide::ALL`] order.
    pub bands: Vec<BandFrame>,
}

/// One occupied side's carve: the band body, its resize splitter,
/// and the stacked group rects inside the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BandFrame {
    pub side: DockSide,
    /// The band body (excludes the splitter).
    pub rect: DockRect,
    /// The 4-px resize splitter between the band and the editor
    /// cell.
    pub splitter: DockRect,
    /// `(group id, rect)` for each group docked on this side, in
    /// stack order, carved from `rect` in equal shares (remainder
    /// to the last).
    pub groups: Vec<(u32, DockRect)>,
}

impl DockFrame {
    /// The rect of a docked group by id, if it is docked (floating
    /// groups are not in the frame — their rect lives on the
    /// group's `DockLocation`).
    #[must_use]
    pub fn group_rect(&self, id: u32) -> Option<DockRect> {
        self.bands
            .iter()
            .flat_map(|b| &b.groups)
            .find(|(gid, _)| *gid == id)
            .map(|(_, r)| *r)
    }
}

/// The whole docking state for one main window. Owned by each UI
/// backend's window state; synced to `session.xml` through
/// [`Self::to_session`] / [`Self::from_session`] on the same cadence
/// as the rest of the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockLayout {
    groups: Vec<DockGroup>,
    /// Band thickness per side, indexed by `DockSide::index`.
    /// Retained while a side is empty so re-docking there restores
    /// the size the user last dragged — the same "the width
    /// survives show/hide" behaviour the fixed panels had.
    side_size: [i32; 4],
    /// Last location of each currently-hidden panel, so a re-toggle
    /// reopens where the user last had it rather than at the
    /// factory default. At most one entry per panel.
    remembered: Vec<(DockPanel, DockLocation)>,
    /// Next group id. Monotonic, never reused within a session (and
    /// re-seeded past every persisted id on load).
    next_id: u32,
}

impl Default for DockLayout {
    fn default() -> Self {
        DockLayout {
            groups: Vec::new(),
            side_size: DEFAULT_SIDE_SIZE,
            remembered: Vec::new(),
            next_id: 1,
        }
    }
}

impl DockLayout {
    #[must_use]
    pub fn new() -> Self {
        DockLayout::default()
    }

    /// Every group, floating and docked, in stack order (the order
    /// bands stack groups; floats' order is z-irrelevant here).
    #[must_use]
    pub fn groups(&self) -> &[DockGroup] {
        &self.groups
    }

    #[must_use]
    pub fn group(&self, id: u32) -> Option<&DockGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// The group currently holding `panel`, if it is visible.
    #[must_use]
    pub fn group_of(&self, panel: DockPanel) -> Option<&DockGroup> {
        self.groups.iter().find(|g| g.panels.contains(&panel))
    }

    #[must_use]
    pub fn is_visible(&self, panel: DockPanel) -> bool {
        self.group_of(panel).is_some()
    }

    /// True iff `panel` is visible *and* the active tab of its
    /// group — i.e. its content is actually on screen. The View
    /// menu marks and toolbar toggles read [`Self::is_visible`]
    /// instead: a panel behind another tab is still "open".
    #[must_use]
    pub fn is_active(&self, panel: DockPanel) -> bool {
        self.group_of(panel)
            .is_some_and(|g| g.active_panel() == panel)
    }

    /// Groups docked on `side`, in stack order.
    pub fn groups_on(&self, side: DockSide) -> impl Iterator<Item = &DockGroup> {
        self.groups
            .iter()
            .filter(move |g| g.location == DockLocation::Side(side))
    }

    /// Floating groups, in creation order (backends impose their
    /// own z-order).
    pub fn floating_groups(&self) -> impl Iterator<Item = &DockGroup> {
        self.groups
            .iter()
            .filter(|g| matches!(g.location, DockLocation::Floating(_)))
    }

    /// Band thickness for `side` (width for Left/Right, height for
    /// Top/Bottom). Always within
    /// `MIN_DOCK_BAND_PX..=MAX_DOCK_BAND_PX`; the tighter
    /// window-relative clamp happens in [`compute_frame`].
    #[must_use]
    pub fn side_size(&self, side: DockSide) -> i32 {
        self.side_size[side.index()]
    }

    /// Store a new band thickness (from a splitter drag or a
    /// persisted value). Clamped to the absolute bounds here; the
    /// window-relative clamp is [`compute_frame`]'s job on every
    /// layout pass, so an over-large stored value self-corrects
    /// visually without being destructively rewritten — growing the
    /// window back restores what the user dragged (the same
    /// deliberate non-write-back as the FIF dock height clamp).
    pub fn set_side_size(&mut self, side: DockSide, px: i32) {
        self.side_size[side.index()] = px.clamp(MIN_DOCK_BAND_PX, MAX_DOCK_BAND_PX);
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn remember(&mut self, panel: DockPanel, location: DockLocation) {
        self.remembered.retain(|(p, _)| *p != panel);
        self.remembered.push((panel, location));
    }

    fn remembered_for(&self, panel: DockPanel) -> Option<DockLocation> {
        self.remembered
            .iter()
            .find(|(p, _)| *p == panel)
            .map(|(_, l)| *l)
    }

    /// Remove `panel` from whatever group holds it, dropping the
    /// group if that empties it. Returns the vacated location.
    fn remove_panel(&mut self, panel: DockPanel) -> Option<DockLocation> {
        let gi = self.groups.iter().position(|g| g.panels.contains(&panel))?;
        let group = &mut self.groups[gi];
        let location = group.location;
        let pi = group
            .panels
            .iter()
            .position(|p| *p == panel)
            .expect("position found above");
        group.panels.remove(pi);
        if group.panels.is_empty() {
            self.groups.remove(gi);
        } else {
            // Keep the active index pointing at a live tab. If the
            // removed tab *was* the active one, fall to its left
            // neighbour (matches the editor tab strip's close
            // behaviour).
            if group.active >= pi && group.active > 0 {
                group.active -= 1;
            }
            group.active = group.active.min(group.panels.len() - 1);
        }
        self.debug_assert_invariants();
        Some(location)
    }

    /// Insert `panel` as a brand-new single-panel group at
    /// `location`. Returns the new group's id.
    fn insert_panel_at(&mut self, panel: DockPanel, location: DockLocation) -> u32 {
        debug_assert!(
            !self.is_visible(panel),
            "insert of an already-visible panel"
        );
        let id = self.alloc_id();
        self.groups.push(DockGroup {
            id,
            location,
            panels: vec![panel],
            active: 0,
        });
        self.debug_assert_invariants();
        id
    }

    /// Show `panel`. Already visible → just make it the active tab
    /// of its group (a "show" on an open-but-behind panel reveals
    /// it). Hidden → reopen at its remembered location, or the
    /// panel's default side on first ever open.
    pub fn show(&mut self, panel: DockPanel) {
        if self.is_visible(panel) {
            self.activate(panel);
            return;
        }
        let location = self
            .remembered_for(panel)
            .unwrap_or(DockLocation::Side(panel.default_side()));
        self.insert_panel_at(panel, location);
    }

    /// Hide `panel`, remembering where it was so the next
    /// [`Self::show`] reopens there.
    pub fn hide(&mut self, panel: DockPanel) {
        if let Some(location) = self.remove_panel(panel) {
            self.remember(panel, location);
        }
    }

    /// Toggle visibility; returns the new visibility.
    pub fn toggle(&mut self, panel: DockPanel) -> bool {
        if self.is_visible(panel) {
            self.hide(panel);
            false
        } else {
            self.show(panel);
            true
        }
    }

    /// Make `panel` the active tab of its group. No-op if hidden.
    pub fn activate(&mut self, panel: DockPanel) {
        for group in &mut self.groups {
            if let Some(i) = group.panels.iter().position(|p| *p == panel) {
                group.active = i;
                return;
            }
        }
    }

    /// Set the active tab of group `id` by index. Out-of-range or
    /// unknown ids are ignored — the caller's index may be a click
    /// resolved against chrome that has since changed, and a stale
    /// click must not corrupt the model.
    pub fn set_active_index(&mut self, id: u32, index: usize) {
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            if index < group.panels.len() {
                group.active = index;
            }
        }
    }

    /// Move a tab within its group from `from` to `to` (both
    /// indices into the current tab order). The moved tab stays
    /// active. Out-of-range indices are ignored.
    pub fn reorder_panel(&mut self, id: u32, from: usize, to: usize) {
        let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
            return;
        };
        if from >= group.panels.len() || to >= group.panels.len() {
            return;
        }
        let panel = group.panels.remove(from);
        group.panels.insert(to, panel);
        group.active = to;
        self.debug_assert_invariants();
    }

    /// Apply a drop of a dragged panel. `IntoGroup` on the panel's
    /// own group is a no-op beyond activation (dropping a tab back
    /// where it came from is the cancel gesture).
    pub fn move_panel(&mut self, panel: DockPanel, target: DropTarget) {
        match target {
            DropTarget::IntoGroup(gid) => {
                if self.group_of(panel).is_some_and(|g| g.id == gid) {
                    self.activate(panel);
                    return;
                }
                // Validate the target BEFORE removing — the drop may
                // race a model change (the same stale-identity class
                // the tab arm/commit fix closed); an unknown gid must
                // leave the layout untouched rather than half-moved.
                if self.group(gid).is_none() {
                    return;
                }
                self.remove_panel(panel);
                // Still present: validated above, and `remove_panel`
                // can only drop the panel's OWN group, which the
                // early return above proved is not `gid`. The `else`
                // arm is unreachable; it returns rather than panics
                // so a future logic change degrades to a lost drop
                // instead of an abort (release builds are
                // `panic = "abort"`).
                let Some(group) = self.groups.iter_mut().find(|g| g.id == gid) else {
                    debug_assert!(false, "validated target group vanished during move_panel");
                    return;
                };
                group.panels.push(panel);
                group.active = group.panels.len() - 1;
            }
            DropTarget::Side(side) => {
                self.remove_panel(panel);
                self.insert_panel_at(panel, DockLocation::Side(side));
            }
            DropTarget::Floating(rect) => {
                self.remove_panel(panel);
                self.insert_panel_at(panel, DockLocation::Floating(clamp_float_size(rect)));
            }
        }
        self.debug_assert_invariants();
    }

    /// Apply a drop of a whole dragged group (caption-bar drag).
    /// `Side` re-docks it (appended to that side's stack); `IntoGroup`
    /// merges its tabs into the target (dragged group's active panel
    /// stays the visible one); `Floating` floats it. A merge into
    /// itself or an unknown target is a no-op.
    pub fn move_group(&mut self, id: u32, target: DropTarget) {
        match target {
            DropTarget::Side(side) => {
                // Remove + re-push so the group lands at the END of
                // the side's stack order (vec order is stack order).
                let Some(gi) = self.groups.iter().position(|g| g.id == id) else {
                    return;
                };
                let mut group = self.groups.remove(gi);
                group.location = DockLocation::Side(side);
                self.groups.push(group);
            }
            DropTarget::Floating(rect) => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.location = DockLocation::Floating(clamp_float_size(rect));
                }
            }
            DropTarget::IntoGroup(other) => {
                if other == id {
                    return;
                }
                let Some(gi) = self.groups.iter().position(|g| g.id == id) else {
                    return;
                };
                if !self.groups.iter().any(|g| g.id == other) {
                    return;
                }
                let dragged = self.groups.remove(gi);
                let active_panel = dragged.active_panel();
                // Same unreachable-else shape as `move_panel`'s
                // target lookup: `other != id` was checked, so the
                // remove above cannot have taken the target with it.
                let Some(target_group) = self.groups.iter_mut().find(|g| g.id == other) else {
                    debug_assert!(false, "validated merge target vanished during move_group");
                    self.groups.insert(gi, dragged);
                    return;
                };
                for p in dragged.panels {
                    if !target_group.panels.contains(&p) {
                        target_group.panels.push(p);
                    }
                }
                if let Some(i) = target_group.panels.iter().position(|p| *p == active_panel) {
                    target_group.active = i;
                }
            }
        }
        self.debug_assert_invariants();
    }

    /// Record a floating group's new rect after a user move/resize.
    /// Unknown or docked ids are ignored.
    pub fn set_floating_rect(&mut self, id: u32, rect: DockRect) {
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            if matches!(group.location, DockLocation::Floating(_)) {
                group.location = DockLocation::Floating(clamp_float_size(rect));
            }
        }
    }

    /// Clamp every floating rect (live and remembered) so its
    /// caption stays reachable inside `area` — a session restored
    /// on a smaller display, or a hand-edited float position, must
    /// never leave a group stranded where no drag can retrieve it.
    pub fn clamp_floating_to_area(&mut self, area: DockRect) {
        for group in &mut self.groups {
            if let DockLocation::Floating(r) = group.location {
                group.location = DockLocation::Floating(clamp_float_into(r, area));
            }
        }
        for (_, loc) in &mut self.remembered {
            if let DockLocation::Floating(r) = *loc {
                *loc = DockLocation::Floating(clamp_float_into(r, area));
            }
        }
    }

    /// Serialise for `session.xml`. Everything the model owns is
    /// captured: groups (with tab order + active tab + float
    /// rects), band sizes, and the remembered locations of hidden
    /// panels (so "close the panel, restart, reopen" lands where
    /// the user had it — the behaviour the fixed panels' persisted
    /// widths already promised).
    #[must_use]
    pub fn to_session(&self) -> DockSession {
        let location_attrs = |loc: DockLocation| match loc {
            DockLocation::Side(s) => (s.persist_key().to_string(), None, None, None, None),
            DockLocation::Floating(r) => (
                "float".to_string(),
                Some(r.x),
                Some(r.y),
                Some(r.w),
                Some(r.h),
            ),
        };
        DockSession {
            left: Some(self.side_size(DockSide::Left)),
            right: Some(self.side_size(DockSide::Right)),
            top: Some(self.side_size(DockSide::Top)),
            bottom: Some(self.side_size(DockSide::Bottom)),
            groups: self
                .groups
                .iter()
                .map(|g| {
                    let (side, x, y, w, h) = location_attrs(g.location);
                    DockGroupSession {
                        side,
                        x,
                        y,
                        w,
                        h,
                        active: g.active,
                        panels: g
                            .panels
                            .iter()
                            .map(|p| DockPanelSession {
                                kind: p.persist_key().to_string(),
                            })
                            .collect(),
                    }
                })
                .collect(),
            remembered: self
                .remembered
                .iter()
                .map(|(p, loc)| {
                    let (side, x, y, w, h) = location_attrs(*loc);
                    DockRememberSession {
                        kind: p.persist_key().to_string(),
                        side,
                        x,
                        y,
                        w,
                        h,
                    }
                })
                .collect(),
        }
    }

    /// Rebuild from a persisted [`DockSession`], validating every
    /// field — `session.xml` is hand-editable, so nothing read from
    /// it may drive layout unclamped:
    ///
    ///   * unknown panel kinds are dropped (forward compat with
    ///     future panels);
    ///   * a panel claimed by two groups keeps its first claim;
    ///   * groups left empty by the above are dropped;
    ///   * `active` is clamped into range;
    ///   * float rects get defaulted/clamped dimensions;
    ///   * band sizes are clamped to the absolute bounds;
    ///   * unknown side strings drop the group (there is no safe
    ///     guess for where the user wanted it).
    #[must_use]
    pub fn from_session(session: &DockSession) -> DockLayout {
        let mut layout = DockLayout::new();
        for (side, saved) in [
            (DockSide::Left, session.left),
            (DockSide::Right, session.right),
            (DockSide::Top, session.top),
            (DockSide::Bottom, session.bottom),
        ] {
            if let Some(px) = saved {
                layout.set_side_size(side, px);
            }
        }
        let parse_location =
            |side: &str, x: Option<i32>, y: Option<i32>, w: Option<i32>, h: Option<i32>| {
                if side == "float" {
                    Some(DockLocation::Floating(clamp_float_size(DockRect::new(
                        x.unwrap_or(0),
                        y.unwrap_or(0),
                        w.unwrap_or(DEFAULT_FLOAT_W),
                        h.unwrap_or(DEFAULT_FLOAT_H),
                    ))))
                } else {
                    DockSide::from_persist_key(side).map(DockLocation::Side)
                }
            };
        let mut seen: Vec<DockPanel> = Vec::new();
        for g in &session.groups {
            let Some(location) = parse_location(&g.side, g.x, g.y, g.w, g.h) else {
                continue;
            };
            let panels: Vec<DockPanel> = g
                .panels
                .iter()
                .filter_map(|p| DockPanel::from_persist_key(&p.kind))
                .filter(|p| {
                    if seen.contains(p) {
                        false
                    } else {
                        seen.push(*p);
                        true
                    }
                })
                .collect();
            if panels.is_empty() {
                continue;
            }
            let active = g.active.min(panels.len() - 1);
            let id = layout.alloc_id();
            layout.groups.push(DockGroup {
                id,
                location,
                panels,
                active,
            });
        }
        for r in &session.remembered {
            let Some(panel) = DockPanel::from_persist_key(&r.kind) else {
                continue;
            };
            if layout.is_visible(panel) {
                // A remembered spot for a *visible* panel is
                // contradictory; the live group wins.
                continue;
            }
            if let Some(location) = parse_location(&r.side, r.x, r.y, r.w, r.h) {
                layout.remember(panel, location);
            }
        }
        layout.debug_assert_invariants();
        layout
    }

    /// Migrate the pre-dock `<workspace>` / `<docmap>` session
    /// fields into a dock layout: each visible panel becomes a
    /// single-panel group on its historical side, each persisted
    /// width seeds that side's band size, and a hidden-but-known
    /// panel gets a remembered spot so its first toggle reopens on
    /// the side it always used. Called only when the session has no
    /// `<dock>` element — a session written by a dock-aware build
    /// never routes through here.
    #[must_use]
    pub fn from_legacy(
        workspace: Option<(bool, Option<i32>)>,
        docmap: Option<(bool, Option<i32>)>,
    ) -> DockLayout {
        let mut layout = DockLayout::new();
        for (panel, legacy) in [
            (DockPanel::Workspace, workspace),
            (DockPanel::DocMap, docmap),
        ] {
            let Some((visible, width)) = legacy else {
                continue;
            };
            let side = panel.default_side();
            if let Some(px) = width {
                layout.set_side_size(side, px);
            }
            if visible {
                layout.insert_panel_at(panel, DockLocation::Side(side));
            } else {
                layout.remember(panel, DockLocation::Side(side));
            }
        }
        layout
    }

    /// Debug-build structural check. The invariants are what every
    /// mutation above claims to preserve; a violation is a bug in
    /// this module, never in a caller, which is why this asserts
    /// rather than repairs.
    fn debug_assert_invariants(&self) {
        #[cfg(debug_assertions)]
        {
            let mut seen: Vec<DockPanel> = Vec::new();
            for g in &self.groups {
                assert!(!g.panels.is_empty(), "empty group {} survived", g.id);
                assert!(g.active < g.panels.len(), "active out of range in {}", g.id);
                for p in &g.panels {
                    assert!(!seen.contains(p), "panel {p:?} in two groups");
                    seen.push(*p);
                }
            }
        }
    }
}

/// Clamp a floating rect's *size* to the floors (position is
/// unconstrained here — [`DockLayout::clamp_floating_to_area`] is
/// the position authority, because only the caller knows the
/// display).
fn clamp_float_size(r: DockRect) -> DockRect {
    DockRect {
        x: r.x,
        y: r.y,
        w: r.w.clamp(MIN_FLOAT_W, MAX_DOCK_BAND_PX),
        h: r.h.clamp(MIN_FLOAT_H, MAX_DOCK_BAND_PX),
    }
}

/// Move `r` so a draggable margin of it stays inside `area`; also
/// clamps size. The tolerance keeps at least a 40-px corner of the
/// caption reachable rather than forcing the whole rect inside —
/// half-off-screen floats are legitimate working positions.
fn clamp_float_into(r: DockRect, area: DockRect) -> DockRect {
    const REACH: i32 = 40;
    let r = clamp_float_size(r);
    let min_x = area.x - r.w + REACH;
    let max_x = (area.x + area.w - REACH).max(min_x);
    let min_y = area.y;
    let max_y = (area.y + area.h - REACH).max(min_y);
    DockRect {
        x: r.x.clamp(min_x, max_x),
        y: r.y.clamp(min_y, max_y),
        w: r.w,
        h: r.h,
    }
}

/// Clamp one band's thickness against the space still available,
/// reserving `reserve` (the editor minimum) beyond the splitter.
/// `clamp(MIN, MIN)` collapses to exactly `MIN_DOCK_BAND_PX` on a
/// window crushed below everyone's minimums rather than panicking on
/// an inverted range — the same shape `clamp_workspace_width` used.
fn clamp_band(requested: i32, available: i32, reserve: i32) -> i32 {
    let upper = (available - reserve - DOCK_SPLITTER_PX).max(MIN_DOCK_BAND_PX);
    requested.clamp(MIN_DOCK_BAND_PX, upper)
}

/// Carve the dock area into band/group/splitter rects and the
/// remaining editor cell. Pure; called on every layout pass, so all
/// clamping is re-applied idempotently — a window resize that pushes
/// past a persisted band size is corrected here without rewriting
/// the stored size (growing the window back restores it).
///
/// Carve order is fixed and documented: **Left, then Right (both
/// full-height), then Top, then Bottom (spanning between the
/// vertical bands)** — the same corner ownership the fixed panels
/// had (side columns ran toolbar-to-statusbar; the FIF dock sat
/// inside the editor column). Later carves clamp against what the
/// earlier ones left, so two fat opposing bands squeeze each other
/// before they squeeze the editor below its minimums.
#[must_use]
pub fn compute_frame(
    mid: DockRect,
    layout: &DockLayout,
    min_editor_w: i32,
    min_editor_h: i32,
) -> DockFrame {
    let mut inner = mid;
    let mut bands: Vec<BandFrame> = Vec::new();

    let occupied = |side: DockSide| layout.groups_on(side).next().is_some();

    // Left/Right reserve enough width for the editor minimum plus,
    // if the opposite band is also occupied, ITS minimum — so the
    // first carve cannot starve the second below MIN_DOCK_BAND_PX.
    if occupied(DockSide::Left) {
        let opposite = if occupied(DockSide::Right) {
            MIN_DOCK_BAND_PX + DOCK_SPLITTER_PX
        } else {
            0
        };
        let size = clamp_band(
            layout.side_size(DockSide::Left),
            inner.w,
            min_editor_w + opposite,
        );
        let rect = DockRect::new(inner.x, inner.y, size, inner.h);
        let splitter = DockRect::new(inner.x + size, inner.y, DOCK_SPLITTER_PX, inner.h);
        bands.push(BandFrame {
            side: DockSide::Left,
            rect,
            splitter,
            groups: stack_groups(layout, DockSide::Left, rect),
        });
        let eaten = size + DOCK_SPLITTER_PX;
        inner.x += eaten;
        inner.w = (inner.w - eaten).max(0);
    }
    if occupied(DockSide::Right) {
        let size = clamp_band(layout.side_size(DockSide::Right), inner.w, min_editor_w);
        let rect = DockRect::new(inner.x + inner.w - size, inner.y, size, inner.h);
        let splitter = DockRect::new(
            rect.x - DOCK_SPLITTER_PX,
            inner.y,
            DOCK_SPLITTER_PX,
            inner.h,
        );
        bands.push(BandFrame {
            side: DockSide::Right,
            rect,
            splitter,
            groups: stack_groups(layout, DockSide::Right, rect),
        });
        inner.w = (inner.w - size - DOCK_SPLITTER_PX).max(0);
    }
    if occupied(DockSide::Top) {
        let opposite = if occupied(DockSide::Bottom) {
            MIN_DOCK_BAND_PX + DOCK_SPLITTER_PX
        } else {
            0
        };
        let size = clamp_band(
            layout.side_size(DockSide::Top),
            inner.h,
            min_editor_h + opposite,
        );
        let rect = DockRect::new(inner.x, inner.y, inner.w, size);
        let splitter = DockRect::new(inner.x, inner.y + size, inner.w, DOCK_SPLITTER_PX);
        bands.push(BandFrame {
            side: DockSide::Top,
            rect,
            splitter,
            groups: stack_groups(layout, DockSide::Top, rect),
        });
        let eaten = size + DOCK_SPLITTER_PX;
        inner.y += eaten;
        inner.h = (inner.h - eaten).max(0);
    }
    if occupied(DockSide::Bottom) {
        let size = clamp_band(layout.side_size(DockSide::Bottom), inner.h, min_editor_h);
        let rect = DockRect::new(inner.x, inner.y + inner.h - size, inner.w, size);
        let splitter = DockRect::new(
            inner.x,
            rect.y - DOCK_SPLITTER_PX,
            inner.w,
            DOCK_SPLITTER_PX,
        );
        bands.push(BandFrame {
            side: DockSide::Bottom,
            rect,
            splitter,
            groups: stack_groups(layout, DockSide::Bottom, rect),
        });
        inner.h = (inner.h - size - DOCK_SPLITTER_PX).max(0);
    }

    DockFrame {
        editor: inner,
        bands,
    }
}

/// Stack a side's groups inside its band rect: equal shares along
/// the band's long axis (vertical for Left/Right, horizontal for
/// Top/Bottom), integer remainder to the last so the shares tile the
/// band exactly.
fn stack_groups(layout: &DockLayout, side: DockSide, band: DockRect) -> Vec<(u32, DockRect)> {
    let ids: Vec<u32> = layout.groups_on(side).map(|g| g.id).collect();
    let n = i32::try_from(ids.len()).unwrap_or(i32::MAX).max(1);
    let vertical = matches!(side, DockSide::Left | DockSide::Right);
    let total = if vertical { band.h } else { band.w };
    let share = total / n;
    let mut out = Vec::with_capacity(ids.len());
    let mut offset = 0;
    for (i, id) in ids.iter().enumerate() {
        let is_last = i + 1 == ids.len();
        let extent = if is_last { total - offset } else { share };
        let rect = if vertical {
            DockRect::new(band.x, band.y + offset, band.w, extent.max(0))
        } else {
            DockRect::new(band.x + offset, band.y, extent.max(0), band.h)
        };
        out.push((*id, rect));
        offset += extent;
    }
    out
}

/// Resolve where a drag would drop if released at `cursor`, and the
/// hint rect the backend outlines to preview it. Priority order,
/// deliberate and worth stating because two of the three zones
/// overlap:
///
///   1. **Edge zone** — cursor inside the dock area and within
///      [`EDGE_DOCK_ZONE_PX`] of one of its edges → dock to that
///      side. Wins over a group hit so a side that already carries a
///      band is still reachable as a *stacked* dock (the band's body
///      would otherwise swallow every drop near its edge and make
///      "dock beside it" unreachable). Nearest edge wins; the
///      corner tie breaks in [`DockSide::ALL`] order.
///   2. **Group hit** — cursor over a group's rect (`zones.groups`,
///      topmost first) → join it as a tab. The dragged group itself
///      is skipped: its own window tracks the cursor for most of a
///      caption drag, and a self-hit would make every float-move
///      resolve to a no-op merge.
///   3. **Float** — anywhere else (inside the window or out) →
///      float at `float_preview`, the caller's cursor-anchored
///      candidate rect (the caller anchors it so the grab point
///      stays under the cursor).
#[must_use]
pub fn resolve_drop(
    zones: &DropZones,
    layout: &DockLayout,
    subject: DragSubject,
    cursor: (i32, i32),
    float_preview: DockRect,
) -> (DropTarget, DockRect) {
    let (cx, cy) = cursor;
    if zones.mid.contains(cx, cy) {
        let d_left = cx - zones.mid.x;
        let d_right = zones.mid.x + zones.mid.w - 1 - cx;
        let d_top = cy - zones.mid.y;
        let d_bottom = zones.mid.y + zones.mid.h - 1 - cy;
        let mut best: Option<(DockSide, i32)> = None;
        for (side, d) in [
            (DockSide::Left, d_left),
            (DockSide::Right, d_right),
            (DockSide::Top, d_top),
            (DockSide::Bottom, d_bottom),
        ] {
            if d <= EDGE_DOCK_ZONE_PX && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((side, d));
            }
        }
        if let Some((side, _)) = best {
            return (
                DropTarget::Side(side),
                side_hint_rect(zones.mid, layout, side),
            );
        }
    }
    let self_group = match subject {
        DragSubject::Group(id) => Some(id),
        DragSubject::Panel(p) => layout
            .group_of(p)
            .filter(|g| g.panels.len() == 1)
            .map(|g| g.id),
    };
    for (id, rect) in &zones.groups {
        if Some(*id) == self_group {
            continue;
        }
        if rect.contains(cx, cy) {
            return (DropTarget::IntoGroup(*id), *rect);
        }
    }
    let preview = clamp_float_size(float_preview);
    (DropTarget::Floating(preview), preview)
}

/// The band rect `side` would occupy if the drag dropped there —
/// the "outline fills out the bottom part as a rectangle" preview.
/// Thickness is the side's stored size clamped against the dock
/// area, so the hint matches what [`compute_frame`] will actually
/// produce closely enough to be an honest preview (it ignores other
/// bands' carves — a preview, not a layout).
fn side_hint_rect(mid: DockRect, layout: &DockLayout, side: DockSide) -> DockRect {
    match side {
        DockSide::Left => {
            let t = clamp_band(layout.side_size(side), mid.w, MIN_DOCK_BAND_PX);
            DockRect::new(mid.x, mid.y, t, mid.h)
        }
        DockSide::Right => {
            let t = clamp_band(layout.side_size(side), mid.w, MIN_DOCK_BAND_PX);
            DockRect::new(mid.x + mid.w - t, mid.y, t, mid.h)
        }
        DockSide::Top => {
            let t = clamp_band(layout.side_size(side), mid.h, MIN_DOCK_BAND_PX);
            DockRect::new(mid.x, mid.y, mid.w, t)
        }
        DockSide::Bottom => {
            let t = clamp_band(layout.side_size(side), mid.h, MIN_DOCK_BAND_PX);
            DockRect::new(mid.x, mid.y + mid.h - t, mid.w, t)
        }
    }
}

/// Tab-reorder drop index for a horizontal tab bar: given each
/// tab's width in order, the tab being dragged, and the cursor's x
/// offset from the bar's left edge, the index the dragged tab
/// should land at. Half-tab boundaries decide, computed against the
/// bar *as it would look without the dragged tab* — the same
/// arithmetic shape as the editor tab strips', shared here so all
/// three backends' plugin-tab bars agree on the boundary cases.
#[must_use]
pub fn tab_drop_index(widths: &[i32], from: usize, cursor_x: i32) -> usize {
    if widths.is_empty() || from >= widths.len() {
        return from.min(widths.len().saturating_sub(1));
    }
    let mut remaining: Vec<i32> = Vec::with_capacity(widths.len() - 1);
    for (i, w) in widths.iter().enumerate() {
        if i != from {
            remaining.push(*w);
        }
    }
    let mut index = remaining.len();
    let mut edge = 0;
    for (i, w) in remaining.iter().enumerate() {
        if cursor_x < edge + w / 2 {
            index = i;
            break;
        }
        edge += w;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid() -> DockRect {
        DockRect::new(0, 0, 1000, 700)
    }

    #[test]
    fn first_show_lands_on_the_default_side() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.show(DockPanel::DocMap);
        assert_eq!(
            l.group_of(DockPanel::Workspace).unwrap().location,
            DockLocation::Side(DockSide::Left)
        );
        assert_eq!(
            l.group_of(DockPanel::DocMap).unwrap().location,
            DockLocation::Side(DockSide::Right)
        );
    }

    #[test]
    fn hide_then_show_reopens_at_the_remembered_location() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.move_panel(DockPanel::Workspace, DropTarget::Side(DockSide::Bottom));
        l.hide(DockPanel::Workspace);
        assert!(!l.is_visible(DockPanel::Workspace));
        l.show(DockPanel::Workspace);
        assert_eq!(
            l.group_of(DockPanel::Workspace).unwrap().location,
            DockLocation::Side(DockSide::Bottom)
        );
    }

    #[test]
    fn hiding_a_floating_panel_remembers_the_float_rect() {
        let mut l = DockLayout::new();
        l.show(DockPanel::DocMap);
        let rect = DockRect::new(50, 60, 300, 400);
        l.move_panel(DockPanel::DocMap, DropTarget::Floating(rect));
        l.hide(DockPanel::DocMap);
        l.show(DockPanel::DocMap);
        assert_eq!(
            l.group_of(DockPanel::DocMap).unwrap().location,
            DockLocation::Floating(rect)
        );
    }

    #[test]
    fn dropping_a_panel_onto_a_group_tabs_it_and_makes_it_active() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.show(DockPanel::DocMap);
        let target = l.group_of(DockPanel::Workspace).unwrap().id;
        l.move_panel(DockPanel::DocMap, DropTarget::IntoGroup(target));
        assert_eq!(l.groups().len(), 1);
        let g = l.group(target).unwrap();
        assert_eq!(g.panels, vec![DockPanel::Workspace, DockPanel::DocMap]);
        assert_eq!(g.active_panel(), DockPanel::DocMap);
    }

    #[test]
    fn dragging_a_tab_out_splits_it_into_its_own_group() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let target = l.group_of(DockPanel::Workspace).unwrap().id;
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::IntoGroup(target));
        l.move_panel(DockPanel::DocMap, DropTarget::Side(DockSide::Right));
        assert_eq!(l.groups().len(), 2);
        assert_eq!(l.group(target).unwrap().panels, vec![DockPanel::Workspace]);
        assert_eq!(
            l.group_of(DockPanel::DocMap).unwrap().location,
            DockLocation::Side(DockSide::Right)
        );
    }

    #[test]
    fn removing_the_active_tab_activates_its_left_neighbour() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let gid = l.group_of(DockPanel::Workspace).unwrap().id;
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::IntoGroup(gid));
        assert_eq!(l.group(gid).unwrap().active_panel(), DockPanel::DocMap);
        l.hide(DockPanel::DocMap);
        assert_eq!(l.group(gid).unwrap().active_panel(), DockPanel::Workspace);
    }

    #[test]
    fn move_panel_into_unknown_group_is_a_no_op() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let before = l.clone();
        l.move_panel(DockPanel::Workspace, DropTarget::IntoGroup(9999));
        assert_eq!(l, before, "a stale drop target must not corrupt the model");
    }

    #[test]
    fn move_group_merge_carries_all_tabs_and_keeps_the_dragged_active_panel() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let a = l.group_of(DockPanel::Workspace).unwrap().id;
        l.show(DockPanel::DocMap);
        let b = l.group_of(DockPanel::DocMap).unwrap().id;
        l.move_group(b, DropTarget::IntoGroup(a));
        assert_eq!(l.groups().len(), 1);
        let g = l.group(a).unwrap();
        assert_eq!(g.panels, vec![DockPanel::Workspace, DockPanel::DocMap]);
        assert_eq!(g.active_panel(), DockPanel::DocMap);
    }

    #[test]
    fn move_group_onto_itself_is_a_no_op() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let a = l.group_of(DockPanel::Workspace).unwrap().id;
        let before = l.clone();
        l.move_group(a, DropTarget::IntoGroup(a));
        assert_eq!(l, before);
    }

    #[test]
    fn reorder_panel_moves_the_tab_and_keeps_it_active() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let gid = l.group_of(DockPanel::Workspace).unwrap().id;
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::IntoGroup(gid));
        l.reorder_panel(gid, 1, 0);
        let g = l.group(gid).unwrap();
        assert_eq!(g.panels, vec![DockPanel::DocMap, DockPanel::Workspace]);
        assert_eq!(g.active_panel(), DockPanel::DocMap);
        // Out-of-range indices are ignored, not clamped into a move.
        let before = l.clone();
        l.reorder_panel(gid, 5, 0);
        assert_eq!(l, before);
    }

    #[test]
    fn group_ids_are_never_reused_within_a_session() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let first = l.group_of(DockPanel::Workspace).unwrap().id;
        l.hide(DockPanel::Workspace);
        l.show(DockPanel::Workspace);
        let second = l.group_of(DockPanel::Workspace).unwrap().id;
        assert_ne!(
            first, second,
            "a reused id would let a stale drag target address the wrong group"
        );
    }

    // --- compute_frame ---

    #[test]
    fn empty_layout_leaves_the_whole_mid_region_to_the_editor() {
        let frame = compute_frame(mid(), &DockLayout::new(), 200, 100);
        assert_eq!(frame.editor, mid());
        assert!(frame.bands.is_empty());
    }

    #[test]
    fn left_band_carves_width_and_full_height() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let frame = compute_frame(mid(), &l, 200, 100);
        assert_eq!(frame.bands.len(), 1);
        let band = &frame.bands[0];
        assert_eq!(band.side, DockSide::Left);
        assert_eq!(band.rect, DockRect::new(0, 0, 240, 700));
        assert_eq!(band.splitter, DockRect::new(240, 0, DOCK_SPLITTER_PX, 700));
        assert_eq!(frame.editor, DockRect::new(244, 0, 756, 700));
    }

    #[test]
    fn bottom_band_spans_between_the_vertical_bands() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace); // left, 240
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::Side(DockSide::Bottom));
        let frame = compute_frame(mid(), &l, 200, 100);
        let bottom = frame
            .bands
            .iter()
            .find(|b| b.side == DockSide::Bottom)
            .unwrap();
        // Starts after the left band + splitter, spans the rest.
        assert_eq!(bottom.rect, DockRect::new(244, 700 - 160, 756, 160));
        assert_eq!(frame.editor, DockRect::new(244, 0, 756, 700 - 164));
    }

    #[test]
    fn two_groups_on_one_side_stack_and_tile_it_exactly() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::Side(DockSide::Left));
        let frame = compute_frame(DockRect::new(0, 0, 1000, 701), &l, 200, 100);
        let band = &frame.bands[0];
        assert_eq!(band.groups.len(), 2);
        let (_, top) = band.groups[0];
        let (_, bottom) = band.groups[1];
        assert_eq!(top.y, 0);
        assert_eq!(bottom.y, top.h);
        // Integer remainder goes to the last group: shares tile the
        // full odd height with no gap.
        assert_eq!(top.h + bottom.h, 701);
    }

    #[test]
    fn opposing_bands_clamp_before_starving_each_other() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.show(DockPanel::DocMap);
        l.set_side_size(DockSide::Left, 900);
        l.set_side_size(DockSide::Right, 900);
        let frame = compute_frame(DockRect::new(0, 0, 600, 700), &l, 200, 100);
        let left = frame
            .bands
            .iter()
            .find(|b| b.side == DockSide::Left)
            .unwrap();
        let right = frame
            .bands
            .iter()
            .find(|b| b.side == DockSide::Right)
            .unwrap();
        // Left's clamp reserved the editor minimum plus Right's
        // minimum, so Right is never crushed below the floor.
        assert!(right.rect.w >= MIN_DOCK_BAND_PX, "{}", right.rect.w);
        assert!(left.rect.w >= MIN_DOCK_BAND_PX);
        assert!(frame.editor.w >= 0);
    }

    #[test]
    fn crushed_window_degrades_without_panicking() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::Side(DockSide::Bottom));
        // Smaller than any single minimum — everything clamps to
        // floors and the editor goes to zero, but nothing inverts.
        let frame = compute_frame(DockRect::new(0, 0, 50, 40), &l, 200, 100);
        assert!(frame.editor.w >= 0 && frame.editor.h >= 0);
        for band in &frame.bands {
            assert!(band.rect.w >= 0 && band.rect.h >= 0);
        }
    }

    // --- resolve_drop ---

    fn zones_with(groups: Vec<(u32, DockRect)>) -> DropZones {
        DropZones { mid: mid(), groups }
    }

    #[test]
    fn near_edge_resolves_to_that_side_with_a_band_hint() {
        let l = DockLayout::new();
        let (target, hint) = resolve_drop(
            &zones_with(vec![]),
            &l,
            DragSubject::Panel(DockPanel::Workspace),
            (500, 690),
            DockRect::new(0, 0, 280, 380),
        );
        assert_eq!(target, DropTarget::Side(DockSide::Bottom));
        // Hint fills the bottom band: full width, side_size tall.
        assert_eq!(hint, DockRect::new(0, 700 - 160, 1000, 160));
    }

    #[test]
    fn edge_zone_beats_a_group_body_at_the_same_spot() {
        // A left-docked group's body covers x=0..240; a drop at
        // x=10 must still read as "dock to the left side" so
        // stacking beside an existing band stays reachable.
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let gid = l.group_of(DockPanel::Workspace).unwrap().id;
        let (target, _) = resolve_drop(
            &zones_with(vec![(gid, DockRect::new(0, 0, 240, 700))]),
            &l,
            DragSubject::Panel(DockPanel::DocMap),
            (10, 350),
            DockRect::default(),
        );
        assert_eq!(target, DropTarget::Side(DockSide::Left));
    }

    #[test]
    fn over_a_group_body_resolves_to_join_with_the_group_rect_as_hint() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let gid = l.group_of(DockPanel::Workspace).unwrap().id;
        let rect = DockRect::new(0, 0, 240, 700);
        let (target, hint) = resolve_drop(
            &zones_with(vec![(gid, rect)]),
            &l,
            DragSubject::Panel(DockPanel::DocMap),
            (120, 350),
            DockRect::default(),
        );
        assert_eq!(target, DropTarget::IntoGroup(gid));
        assert_eq!(hint, rect);
    }

    #[test]
    fn a_dragged_group_never_hits_itself() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let gid = l.group_of(DockPanel::Workspace).unwrap().id;
        let own = DockRect::new(300, 200, 280, 380);
        let preview = DockRect::new(310, 210, 280, 380);
        let (target, _) = resolve_drop(
            &zones_with(vec![(gid, own)]),
            &l,
            DragSubject::Group(gid),
            (400, 300),
            preview,
        );
        assert_eq!(
            target,
            DropTarget::Floating(preview),
            "a caption drag over the group's own window is a move, not a merge"
        );
    }

    #[test]
    fn a_single_panel_drag_skips_its_own_group_too() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let gid = l.group_of(DockPanel::Workspace).unwrap().id;
        let own = DockRect::new(0, 0, 240, 700);
        let preview = DockRect::new(100, 300, 280, 380);
        let (target, _) = resolve_drop(
            &zones_with(vec![(gid, own)]),
            &l,
            DragSubject::Panel(DockPanel::Workspace),
            (120, 350),
            preview,
        );
        assert_eq!(
            target,
            DropTarget::Floating(preview),
            "the sole tab of a group dropping onto that same group is a float-out, not a join"
        );
    }

    #[test]
    fn outside_everything_resolves_to_floating_at_the_preview() {
        let l = DockLayout::new();
        let preview = DockRect::new(-200, -50, 280, 380);
        let (target, hint) = resolve_drop(
            &zones_with(vec![]),
            &l,
            DragSubject::Panel(DockPanel::DocMap),
            (-100, -10),
            preview,
        );
        assert_eq!(target, DropTarget::Floating(preview));
        assert_eq!(hint, preview);
    }

    #[test]
    fn corner_tie_breaks_in_side_order() {
        let l = DockLayout::new();
        // Exactly equidistant from the left and top edges.
        let (target, _) = resolve_drop(
            &zones_with(vec![]),
            &l,
            DragSubject::Panel(DockPanel::Workspace),
            (10, 10),
            DockRect::default(),
        );
        assert_eq!(target, DropTarget::Side(DockSide::Left));
    }

    #[test]
    fn topmost_group_wins_an_overlapping_hit() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let a = l.group_of(DockPanel::Workspace).unwrap().id;
        l.show(DockPanel::DocMap);
        let b = l.group_of(DockPanel::DocMap).unwrap().id;
        let under = DockRect::new(100, 100, 400, 400);
        let over = DockRect::new(200, 200, 400, 400);
        // zones.groups is topmost-first; the overlap belongs to it.
        let (target, _) = resolve_drop(
            &zones_with(vec![(b, over), (a, under)]),
            &l,
            DragSubject::Panel(DockPanel::Workspace),
            (300, 300),
            DockRect::default(),
        );
        assert_eq!(target, DropTarget::IntoGroup(b));
    }

    // --- floating clamps ---

    #[test]
    fn floating_rects_are_clamped_back_into_reach() {
        let mut l = DockLayout::new();
        l.show(DockPanel::DocMap);
        l.move_panel(
            DockPanel::DocMap,
            DropTarget::Floating(DockRect::new(-5000, -5000, 300, 400)),
        );
        l.clamp_floating_to_area(DockRect::new(0, 0, 1200, 800));
        let DockLocation::Floating(r) = l.group_of(DockPanel::DocMap).unwrap().location else {
            panic!("still floating");
        };
        assert!(r.x + r.w >= 40, "caption unreachable at x={}", r.x);
        assert!(r.y >= 0, "caption above the top edge at y={}", r.y);
    }

    #[test]
    fn floating_size_floors_apply_on_every_entry_path() {
        let mut l = DockLayout::new();
        l.show(DockPanel::DocMap);
        l.move_panel(
            DockPanel::DocMap,
            DropTarget::Floating(DockRect::new(10, 10, 1, 1)),
        );
        let DockLocation::Floating(r) = l.group_of(DockPanel::DocMap).unwrap().location else {
            panic!("still floating");
        };
        assert_eq!((r.w, r.h), (MIN_FLOAT_W, MIN_FLOAT_H));
    }

    // --- persistence ---

    #[test]
    fn session_round_trip_preserves_everything() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let gid = l.group_of(DockPanel::Workspace).unwrap().id;
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::IntoGroup(gid));
        l.reorder_panel(gid, 1, 0);
        l.set_side_size(DockSide::Left, 300);
        let session = l.to_session();
        let restored = DockLayout::from_session(&session);
        assert_eq!(restored.side_size(DockSide::Left), 300);
        let g = restored.group_of(DockPanel::DocMap).unwrap();
        assert_eq!(g.panels, vec![DockPanel::DocMap, DockPanel::Workspace]);
        assert_eq!(g.active_panel(), DockPanel::DocMap);
        assert_eq!(g.location, DockLocation::Side(DockSide::Left));
    }

    #[test]
    fn session_round_trip_preserves_floats_and_remembered_spots() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.move_panel(
            DockPanel::Workspace,
            DropTarget::Floating(DockRect::new(80, 90, 300, 420)),
        );
        l.show(DockPanel::DocMap);
        l.move_panel(DockPanel::DocMap, DropTarget::Side(DockSide::Bottom));
        l.hide(DockPanel::DocMap);
        let restored = DockLayout::from_session(&l.to_session());
        assert_eq!(
            restored.group_of(DockPanel::Workspace).unwrap().location,
            DockLocation::Floating(DockRect::new(80, 90, 300, 420))
        );
        assert!(!restored.is_visible(DockPanel::DocMap));
        let mut reopened = restored;
        reopened.show(DockPanel::DocMap);
        assert_eq!(
            reopened.group_of(DockPanel::DocMap).unwrap().location,
            DockLocation::Side(DockSide::Bottom)
        );
    }

    #[test]
    fn from_session_survives_hostile_input() {
        // Duplicate panel claims, unknown kinds, unknown sides,
        // out-of-range active, absurd sizes — all the shapes a
        // hand-edited or truncated session.xml can take.
        let session = DockSession {
            left: Some(-50),
            right: Some(99_999),
            top: None,
            bottom: None,
            groups: vec![
                DockGroupSession {
                    side: "left".into(),
                    x: None,
                    y: None,
                    w: None,
                    h: None,
                    active: 7,
                    panels: vec![
                        DockPanelSession {
                            kind: "workspace".into(),
                        },
                        DockPanelSession {
                            kind: "hologram".into(),
                        },
                    ],
                },
                DockGroupSession {
                    side: "diagonal".into(),
                    x: None,
                    y: None,
                    w: None,
                    h: None,
                    active: 0,
                    panels: vec![DockPanelSession {
                        kind: "docmap".into(),
                    }],
                },
                DockGroupSession {
                    side: "float".into(),
                    x: Some(10),
                    y: Some(10),
                    w: Some(1),
                    h: Some(1),
                    active: 0,
                    panels: vec![DockPanelSession {
                        kind: "workspace".into(),
                    }],
                },
            ],
            remembered: vec![DockRememberSession {
                kind: "workspace".into(),
                side: "left".into(),
                x: None,
                y: None,
                w: None,
                h: None,
            }],
        };
        let l = DockLayout::from_session(&session);
        assert_eq!(l.side_size(DockSide::Left), MIN_DOCK_BAND_PX);
        assert_eq!(l.side_size(DockSide::Right), MAX_DOCK_BAND_PX);
        // workspace kept its first claim (the left group); the
        // unknown-side docmap group and the duplicate float were
        // dropped; the remembered entry for a visible panel was
        // ignored.
        let g = l.group_of(DockPanel::Workspace).unwrap();
        assert_eq!(g.location, DockLocation::Side(DockSide::Left));
        assert_eq!(g.panels, vec![DockPanel::Workspace]);
        assert_eq!(g.active, 0, "active clamped into range");
        assert!(!l.is_visible(DockPanel::DocMap));
    }

    #[test]
    fn legacy_migration_reproduces_the_fixed_panel_placement() {
        let l = DockLayout::from_legacy(Some((true, Some(260))), Some((false, Some(190))));
        assert_eq!(
            l.group_of(DockPanel::Workspace).unwrap().location,
            DockLocation::Side(DockSide::Left)
        );
        assert_eq!(l.side_size(DockSide::Left), 260);
        assert_eq!(l.side_size(DockSide::Right), 190);
        assert!(!l.is_visible(DockPanel::DocMap));
        // The hidden docmap remembered its historical side, so its
        // first toggle opens on the right as it always did.
        let mut l = l;
        l.show(DockPanel::DocMap);
        assert_eq!(
            l.group_of(DockPanel::DocMap).unwrap().location,
            DockLocation::Side(DockSide::Right)
        );
    }

    #[test]
    fn restored_ids_do_not_collide_with_newly_allocated_ones() {
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        let restored = DockLayout::from_session(&l.to_session());
        let existing: Vec<u32> = restored.groups().iter().map(|g| g.id).collect();
        let mut l2 = restored;
        l2.show(DockPanel::DocMap);
        let new_id = l2.group_of(DockPanel::DocMap).unwrap().id;
        assert!(!existing.contains(&new_id));
    }

    // --- tab_drop_index ---

    #[test]
    fn tab_drop_index_half_tab_boundaries() {
        let widths = [100, 100, 100];
        // Dragging tab 0: boundaries against the remaining tabs
        // [100, 100] sit at 50 and 150.
        assert_eq!(tab_drop_index(&widths, 0, 10), 0);
        assert_eq!(tab_drop_index(&widths, 0, 49), 0);
        assert_eq!(tab_drop_index(&widths, 0, 50), 1);
        assert_eq!(tab_drop_index(&widths, 0, 149), 1);
        assert_eq!(tab_drop_index(&widths, 0, 150), 2);
        assert_eq!(tab_drop_index(&widths, 0, 900), 2);
    }

    #[test]
    fn tab_drop_index_degenerate_inputs() {
        assert_eq!(tab_drop_index(&[], 0, 10), 0);
        assert_eq!(tab_drop_index(&[100], 0, 10), 0);
        assert_eq!(
            tab_drop_index(&[100, 100], 5, 10),
            1,
            "stale from-index clamps"
        );
    }
}

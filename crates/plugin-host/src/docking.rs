//! Docking-manager ABI arithmetic, shared by every backend that hosts
//! plugin panels.
//!
//! Two numberings cross the plugin boundary, one in each direction: a
//! plugin names the container it wants through `tTbData.uMask`'s
//! `DWS_DF_CONT_*` nibble, and is told the container it is in through
//! the high word of `DMN_DOCK` / `DMN_FLOAT`. They are one numbering,
//! Notepad++'s `CONT_*`, and a backend that decoded one while encoding
//! the other differently would tell a plugin asking for the bottom that
//! it is docked on the right. So both live here, once, with a test
//! pinning each against the other. The Win32 backend carried its own
//! copies until the GTK backend needed the same arithmetic — two
//! copies of an ABI numbering is how the two hosts would drift.
//!
//! The same reasoning puts the *when* of the other docking
//! notifications here too: [`PanelTold`] decides, from the model alone,
//! which of `DMN_DOCK` / `DMN_FLOAT`, `DMN_SWITCHIN` / `DMN_SWITCHOFF`
//! and `DMN_FLOATDROPPED` a reconcile owes a plugin, and
//! [`delivery_order`] the order they go out in. A backend supplies only
//! the mechanism — whom to send them to and when it is safe to.

use codepp_core::dock::{
    DockContainer, DockFrame, DockLayout, DockLocation, DockPanel, DockRect, DockSide,
};

use crate::ffi::{
    CONT_BOTTOM, CONT_LEFT, CONT_RIGHT, CONT_TOP, DMN_CLOSE, DMN_DOCK, DMN_FLOAT, DMN_FLOATDROPPED,
    DMN_SWITCHIN, DMN_SWITCHOFF, DOCKCONT_MAX, DWS_DF_CONT_BOTTOM, DWS_DF_CONT_LEFT,
    DWS_DF_CONT_RIGHT, DWS_DF_CONT_TOP, DWS_DF_FLOATING,
};

/// The side a plugin asked its panel to open on, from
/// `tTbData.uMask`'s default-container nibble.
///
/// The container preference lives in the top nibble: `DWS_DF_FLOATING`
/// in bit 31 means "open floating", and bits 28..30 carry a
/// `CONT_LEFT`/`RIGHT`/`TOP`/`BOTTOM` id otherwise. `None` for a plugin
/// that asked to float or named no container — the panel then takes
/// `DockPanel::default_side`.
///
/// Floating is answered with `None` rather than a side because a plugin
/// panel that opens floating is exactly the pre-dock behaviour the
/// docking subsystem replaced; honouring it would put the panel back in
/// a window of its own. A plugin that wants to float can be dragged
/// out, which is the same affordance the host's own panels have.
#[must_use]
pub fn dock_side_from_u_mask(u_mask: u32) -> Option<DockSide> {
    // The nibble is a value, not a bitmask: CONT_LEFT is 0, so a
    // mask-and-test would read "left" out of every u_mask that happens
    // to carry none of the other three.
    const CONT_MASK: u32 = 0x7000_0000;
    if u_mask & DWS_DF_FLOATING != 0 {
        return None;
    }
    match u_mask & CONT_MASK {
        v if v == DWS_DF_CONT_LEFT & CONT_MASK => Some(DockSide::Left),
        v if v == DWS_DF_CONT_RIGHT & CONT_MASK => Some(DockSide::Right),
        v if v == DWS_DF_CONT_TOP & CONT_MASK => Some(DockSide::Top),
        v if v == DWS_DF_CONT_BOTTOM & CONT_MASK => Some(DockSide::Bottom),
        _ => None,
    }
}

/// Notepad++'s number for a docked container: its `CONT_*` value.
///
/// Written out rather than derived from any backend's own side index,
/// which indexes a splitter array and is free to be reordered for that;
/// this one is ABI. A test pins it against [`dock_side_from_u_mask`],
/// which is the same numbering arriving from the other direction.
#[must_use]
pub fn npp_container_index(side: DockSide) -> u32 {
    match side {
        DockSide::Left => CONT_LEFT,
        DockSide::Right => CONT_RIGHT,
        DockSide::Top => CONT_TOP,
        DockSide::Bottom => CONT_BOTTOM,
    }
}

/// The `nmhdr.code` telling a plugin its panel is now in `container`:
/// `MAKELONG(DMN_DOCK or DMN_FLOAT, container number)`.
///
/// The container number rides in the high word because that is where
/// upstream puts it, and where a plugin built from Notepad++'s
/// docking-dialog template reads it — `HIWORD(code)` on `DMN_DOCK` is
/// how that template learns which side it is docked to, and it switches
/// on `LOWORD(code)`, which is why the two halves must not be swapped or
/// merged. Floating containers are numbered from [`DOCKCONT_MAX`] in the
/// order the model lists floating groups; a hidden panel whose
/// remembered spot is floating is reported as the container a new
/// floating group would get. That number carries less than the docked
/// one — nothing in the template reads it — and is reported because the
/// code has to carry *something* there.
#[must_use]
pub fn dock_container_code(layout: &DockLayout, container: DockContainer) -> u32 {
    let (dmn, index) = match container {
        DockContainer::Docked(side) => (DMN_DOCK, npp_container_index(side)),
        DockContainer::Floating(group) => {
            let ordinal = group
                .and_then(|id| layout.floating_ordinal(id))
                .unwrap_or_else(|| layout.floating_groups().count());
            let ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);
            (DMN_FLOAT, DOCKCONT_MAX.saturating_add(ordinal).min(0xFFFF))
        }
    };
    (index << 16) | (dmn & 0xFFFF)
}

// --- what a reconcile owes a plugin ------------------------------------------------
//
// Notepad++ sends `DMN_SWITCHIN`, `DMN_SWITCHOFF` and `DMN_FLOATDROPPED`
// from inside its container code (read from its source, not measured):
// selecting a tab tells the panel coming in, then the one going out, and
// every relayout of a container — which a tab switch, a show, a hide, a
// resize and the end of a drag all cause — tells *every* panel in it
// `DMN_FLOATDROPPED`, despite the name, docked containers included. What
// its own panels do with them says what they mean: the Document Map shows
// its viewport overlay on `DMN_SWITCHIN`, hides it on `DMN_SWITCHOFF` and
// moves it on `DMN_FLOATDROPPED`, and the Function List reloads on
// `DMN_SWITCHIN`. Code++ sends each on the edge that meaning describes,
// and skips the repeats Notepad++'s layout code produces on the way.

/// Where a plugin panel's content sits, in the terms `DMN_FLOATDROPPED`
/// is owed on: its group's rectangle, and whether the group shows a tab
/// bar.
///
/// A docked group's rectangle is its carve of the dock area and a
/// floating group's is its own window's; the two are in different
/// coordinate spaces, which is why the kind is part of the value. The tab
/// bar counts because it takes its height from every panel in the group.
/// A panel's place among the group's tabs does not: every tab of a group
/// is laid out in the one content slot. So two placements are equal
/// exactly when the host lays the panel's content out in the same place
/// at the same size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PanelPlacement {
    floating: bool,
    rect: DockRect,
    tabbed: bool,
}

impl PanelPlacement {
    /// Where `panel` sits in `layout`. `frame` is the backend's own
    /// [`compute_frame`] of its dock area as it is laid out now, or `None`
    /// while the area has no size.
    ///
    /// `None` for a panel in no group — hidden, parked, never shown — and
    /// for a docked one while there is no frame to place it in: nothing
    /// has been laid out yet, so there is nothing to tell its plugin.
    ///
    /// [`compute_frame`]: codepp_core::dock::compute_frame
    #[must_use]
    fn of(layout: &DockLayout, frame: Option<&DockFrame>, panel: DockPanel) -> Option<Self> {
        let group = layout.group_of(panel)?;
        let (floating, rect) = match group.location {
            DockLocation::Floating(rect) => (true, rect),
            DockLocation::Side(_) => (false, frame?.group_rect(group.id)?),
        };
        // Every backend shows a group's tab bar from its second panel on.
        let tabbed = group.panels.len() >= 2;
        Some(PanelPlacement {
            floating,
            rect,
            tabbed,
        })
    }
}

/// Which way a panel's tab switched: `DMN_SWITCHIN` or `DMN_SWITCHOFF`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabSwitch {
    /// The panel came on screen: it is the tab its group shows, and it
    /// had been hidden, parked or behind another tab.
    In,
    /// The panel went behind another tab of its group, where it is still
    /// open.
    Off,
}

/// What one reconcile owes a plugin about one of its panels. See
/// [`PanelTold::update`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Owed {
    /// A `DMN_DOCK` / `DMN_FLOAT` code ([`dock_container_code`]): the
    /// panel is in a container its plugin has not been told of.
    pub container: Option<u32>,
    /// A `DMN_SWITCHIN` or a `DMN_SWITCHOFF`.
    pub switch: Option<TabSwitch>,
    /// A `DMN_FLOATDROPPED`: the panel has been laid out somewhere new.
    pub relaid: bool,
}

/// What a plugin has been told about one of its panels by the `DMN_*` a
/// dock reconcile sends. One per registration, kept by the backend beside
/// it, starting from `default()` — told nothing — so that the first
/// update after a registration owes whatever the panel's state calls for.
///
/// [`Self::update`] writes the record *before* anything is sent, and that
/// order is what stops a change being told twice: a plugin's handler may
/// show, hide or move a panel, which reconciles again from inside the
/// delivery, and that nested pass must find the change being delivered
/// already recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelTold {
    /// The container last named by `DMN_DOCK` / `DMN_FLOAT`.
    container: Option<DockContainer>,
    /// Whether, at the last update, the panel was the tab its group
    /// shows.
    front: bool,
    /// The panel's placement at the last update — `None` while it had
    /// none.
    placement: Option<PanelPlacement>,
}

impl PanelTold {
    /// Bring the record up to date with `panel` as `layout` has it and
    /// return what its plugin is owed for the change. `frame` is the
    /// backend's own `compute_frame` of its dock area as laid out now, or
    /// `None` while the area has no size — when no docked panel has been
    /// laid out anywhere yet. Owed:
    ///
    /// - **`DMN_DOCK` / `DMN_FLOAT`** when the panel's container is not
    ///   the one its plugin was last told of, which the first update after
    ///   a registration always finds ([`DockContainer::is_same`] decides).
    /// - **`DMN_SWITCHIN`** when the panel has come on screen: it is the
    ///   tab its group shows, and was not at the last update.
    ///   **`DMN_SWITCHOFF`** when it has gone behind another tab of its
    ///   group. A panel that leaves the screen by being hidden or parked
    ///   is owed neither: Notepad++ sends a closed panel no
    ///   `DMN_SWITCHOFF`, and its plugin learns of the close from
    ///   `DMN_CLOSE`, or from having asked for it.
    /// - **`DMN_FLOATDROPPED`** when the panel has a placement and it is
    ///   not the last one: the panel was shown, or its group was moved,
    ///   resized, floated or docked, or gained or lost its tab bar.
    ///
    /// Each is owed on the edge. A show of the panel already in front, a
    /// click on the tab already selected and a move that leaves the panel
    /// in front owe no `DMN_SWITCHIN`, and a tab switch that changes no
    /// geometry owes no `DMN_FLOATDROPPED` — the repeats Notepad++ sends
    /// as a side effect of how it lays its containers out.
    pub fn update(
        &mut self,
        layout: &DockLayout,
        frame: Option<&DockFrame>,
        panel: DockPanel,
    ) -> Owed {
        let container = layout.container_of(panel);
        let container_told = self.container.is_some_and(|last| last.is_same(container));
        self.container = Some(container);

        let front = layout.is_active(panel);
        let switch = match (self.front, front) {
            (false, true) => Some(TabSwitch::In),
            (true, false) if layout.is_visible(panel) => Some(TabSwitch::Off),
            _ => None,
        };
        self.front = front;

        let placement = PanelPlacement::of(layout, frame, panel);
        let relaid = placement.is_some() && placement != self.placement;
        self.placement = placement;

        Owed {
            container: (!container_told).then(|| dock_container_code(layout, container)),
            switch,
            relaid,
        }
    }
}

/// The `DMN_*` codes one reconcile sends, in the order they go out: every
/// `DMN_DOCK` / `DMN_FLOAT`, then every `DMN_SWITCHIN`, then every
/// `DMN_SWITCHOFF`, then every `DMN_FLOATDROPPED` — each kind in the order
/// `owed` lists the panels. `K` is whatever the backend addresses a
/// plugin's panel by.
///
/// By kind rather than by panel so that a tab switch reads as it does in
/// Notepad++, which tells the panel coming to the front of a container
/// before the one it replaced, and relays the container out — the
/// `DMN_FLOATDROPPED` — after both; and in the order Notepad++ tells a
/// panel it moves between containers: where it now is first. The switch
/// and relayout codes carry nothing in their high word, as upstream's do:
/// its own panels compare the whole code.
#[must_use]
pub fn delivery_order<K: Copy>(owed: &[(K, Owed)]) -> Vec<(K, u32)> {
    let container = owed
        .iter()
        .filter_map(|&(key, o)| o.container.map(|code| (key, code)));
    let switch = |to: TabSwitch, code: u32| {
        owed.iter()
            .filter(move |(_, o)| o.switch == Some(to))
            .map(move |&(key, _)| (key, code))
    };
    let relaid = owed
        .iter()
        .filter(|(_, o)| o.relaid)
        .map(|&(key, _)| (key, DMN_FLOATDROPPED));
    container
        .chain(switch(TabSwitch::In, DMN_SWITCHIN))
        .chain(switch(TabSwitch::Off, DMN_SWITCHOFF))
        .chain(relaid)
        .collect()
}

/// A `DMN_*` code's name, from its low word — for logs, where the number
/// alone says little.
#[must_use]
pub fn dmn_name(code: u32) -> &'static str {
    match code & 0xFFFF {
        DMN_CLOSE => "DMN_CLOSE",
        DMN_DOCK => "DMN_DOCK",
        DMN_FLOAT => "DMN_FLOAT",
        DMN_SWITCHIN => "DMN_SWITCHIN",
        DMN_SWITCHOFF => "DMN_SWITCHOFF",
        DMN_FLOATDROPPED => "DMN_FLOATDROPPED",
        _ => "DMN_?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codepp_core::dock::{compute_frame, DropTarget};

    /// Each default-container value decodes to its side, and a
    /// floating request to none — whatever content flags ride along in
    /// the low bits.
    #[test]
    fn the_u_mask_nibble_decodes_to_a_side() {
        use crate::ffi::{DWS_ADDINFO, DWS_ICONTAB};
        let flags = DWS_ICONTAB | DWS_ADDINFO;
        assert_eq!(
            dock_side_from_u_mask(DWS_DF_CONT_LEFT | flags),
            Some(DockSide::Left)
        );
        assert_eq!(
            dock_side_from_u_mask(DWS_DF_CONT_RIGHT | flags),
            Some(DockSide::Right)
        );
        assert_eq!(
            dock_side_from_u_mask(DWS_DF_CONT_TOP | flags),
            Some(DockSide::Top)
        );
        assert_eq!(
            dock_side_from_u_mask(DWS_DF_CONT_BOTTOM | flags),
            Some(DockSide::Bottom)
        );
        assert_eq!(
            dock_side_from_u_mask(DWS_DF_FLOATING | DWS_DF_CONT_BOTTOM),
            None,
            "floating wins over a container"
        );
        assert_eq!(
            dock_side_from_u_mask(0x4000_0000),
            None,
            "a container number past the four sides names none of them"
        );
    }

    /// The container numbers are ABI in both directions: a plugin names
    /// a side through `DWS_DF_CONT_*` when it registers, and is told its
    /// side back through `DMN_DOCK`'s high word. The two must be one
    /// numbering, or a plugin asking for the bottom is told it is docked
    /// on the right.
    #[test]
    fn container_numbers_match_the_registration_nibble() {
        for side in DockSide::ALL {
            let n = npp_container_index(side);
            assert_eq!(
                dock_side_from_u_mask(n << 28),
                Some(side),
                "container {n} decodes to a different side than it encodes"
            );
        }
    }

    /// The container numbers against the literals Notepad++'s headers
    /// give, not against the constants: a test that compares a constant
    /// with itself cannot fail on a wrong one, which is how the `DMN_*`
    /// numbers once stayed wrong for two phases (DESIGN.md §7.4).
    #[test]
    fn container_numbers_are_notepad_plus_pluss() {
        assert_eq!(
            [
                DockSide::Left,
                DockSide::Right,
                DockSide::Top,
                DockSide::Bottom
            ]
            .map(npp_container_index),
            [0, 1, 2, 3],
            "CONT_LEFT, CONT_RIGHT, CONT_TOP, CONT_BOTTOM"
        );
        assert_eq!(DOCKCONT_MAX, 4, "the first floating container");
    }

    #[test]
    fn container_code_packs_the_notification_low_and_the_container_high() {
        let layout = DockLayout::new();
        let docked = dock_container_code(&layout, DockContainer::Docked(DockSide::Bottom));
        assert_eq!(docked & 0xFFFF, DMN_DOCK);
        assert_eq!(docked >> 16, 3, "CONT_BOTTOM");
        let left = dock_container_code(&layout, DockContainer::Docked(DockSide::Left));
        assert_eq!(left, DMN_DOCK, "CONT_LEFT is 0: the bare code");
        let floating = dock_container_code(&layout, DockContainer::Floating(None));
        assert_eq!(floating & 0xFFFF, DMN_FLOAT);
        assert_eq!(floating >> 16, 4, "first floating container");
    }

    #[test]
    fn container_code_numbers_floating_groups_after_the_docked_four() {
        let a = codepp_core::dock::intern_plugin_panel("cc-a.dll", "CC A").expect("intern");
        let b = codepp_core::dock::intern_plugin_panel("cc-b.dll", "CC B").expect("intern");
        let mut l = DockLayout::new();
        l.show(a);
        l.show(b);
        l.move_panel(a, DropTarget::Floating(DockRect::new(0, 0, 300, 200)));
        l.move_panel(b, DropTarget::Floating(DockRect::new(40, 40, 300, 200)));
        let code_of = |p| dock_container_code(&l, l.container_of(p)) >> 16;
        assert_eq!(code_of(a), DOCKCONT_MAX);
        assert_eq!(code_of(b), DOCKCONT_MAX + 1);
    }

    // --- what a reconcile owes -------------------------------------------------
    //
    // The two built-in panels stand in for plugin panels here: the policy
    // is the same for any panel, and they need no slot in the process-wide
    // identity table every test in this binary shares.

    use codepp_core::dock::DockPanel::{DocMap, Workspace};

    /// The carve a backend lays a 1000 × 700 dock area out with.
    fn frame_of(l: &DockLayout) -> DockFrame {
        compute_frame(DockRect::new(0, 0, 1000, 700), l, 200, 60)
    }

    /// One reconcile's update of `told`, the dock area laid out.
    fn update(told: &mut PanelTold, l: &DockLayout, panel: DockPanel) -> Owed {
        told.update(l, Some(&frame_of(l)), panel)
    }

    fn owed(switch: Option<TabSwitch>, relaid: bool) -> Owed {
        Owed {
            container: None,
            switch,
            relaid,
        }
    }

    /// `Workspace` and `DocMap` as two tabs of one left-hand group,
    /// `DocMap` in front — the way two plugin panels asking for the same
    /// container end up — with both records brought up to date.
    fn two_tabs() -> (DockLayout, PanelTold, PanelTold) {
        let mut l = DockLayout::new();
        l.set_initial_side(DocMap, DockSide::Left);
        l.show(Workspace);
        l.show(DocMap);
        assert_eq!(
            l.groups().len(),
            1,
            "the fixture wants one group of two tabs"
        );
        let (mut ws, mut map) = (PanelTold::default(), PanelTold::default());
        update(&mut ws, &l, Workspace);
        update(&mut map, &l, DocMap);
        (l, ws, map)
    }

    /// A registration is owed the container its panel will open in and
    /// nothing else — nothing is on screen or laid out yet — and an update
    /// that finds nothing changed owes nothing: the record is what makes a
    /// reconcile idempotent.
    #[test]
    fn a_hidden_panel_is_told_its_container_and_nothing_else() {
        let l = DockLayout::new();
        let mut told = PanelTold::default();
        assert_eq!(
            update(&mut told, &l, Workspace),
            Owed {
                container: Some(DMN_DOCK),
                switch: None,
                relaid: false
            },
            "Workspace opens on the left: CONT_LEFT, the bare code"
        );
        assert_eq!(update(&mut told, &l, Workspace), Owed::default());
    }

    /// Shown, a panel comes on screen where it is laid out; shown again
    /// while in front, it is owed nothing — Notepad++ repeats its
    /// `DMN_SWITCHIN` there, as a side effect of re-selecting the tab.
    #[test]
    fn a_shown_panel_is_switched_in_where_it_is_laid_out() {
        let mut l = DockLayout::new();
        let mut told = PanelTold::default();
        update(&mut told, &l, Workspace);
        l.show(Workspace);
        assert_eq!(
            update(&mut told, &l, Workspace),
            owed(Some(TabSwitch::In), true)
        );
        l.show(Workspace);
        assert_eq!(update(&mut told, &l, Workspace), Owed::default());
    }

    /// A tab switch tells the panel coming in and the one going out, and
    /// relays nothing out: every tab of a group shares one content slot.
    #[test]
    fn a_tab_switch_tells_the_panel_coming_in_and_the_one_going_out() {
        let (mut l, mut ws, mut map) = two_tabs();
        l.activate(Workspace);
        assert_eq!(
            update(&mut ws, &l, Workspace),
            owed(Some(TabSwitch::In), false)
        );
        assert_eq!(
            update(&mut map, &l, DocMap),
            owed(Some(TabSwitch::Off), false)
        );
    }

    /// A panel shown into its container's group comes in front of the tab
    /// that was there, and both are relaid out: the second tab brings the
    /// tab bar, which takes its height from both.
    #[test]
    fn a_second_tab_switches_the_first_off_and_relays_both_out() {
        let mut l = DockLayout::new();
        l.set_initial_side(DocMap, DockSide::Left);
        l.show(Workspace);
        let (mut ws, mut map) = (PanelTold::default(), PanelTold::default());
        update(&mut ws, &l, Workspace);
        update(&mut map, &l, DocMap);
        l.show(DocMap);
        assert_eq!(
            update(&mut map, &l, DocMap),
            owed(Some(TabSwitch::In), true)
        );
        assert_eq!(
            update(&mut ws, &l, Workspace),
            owed(Some(TabSwitch::Off), true)
        );
    }

    /// Closing the front tab brings the other one in — relaid out, since
    /// the tab bar goes with the second tab — and tells the closed panel
    /// nothing: Notepad++ sends a closed panel no `DMN_SWITCHOFF`.
    /// Reopened, it comes back on screen and is laid out again.
    #[test]
    fn closing_the_front_tab_switches_the_next_in_and_the_closed_one_is_told_nothing() {
        let (mut l, mut ws, mut map) = two_tabs();
        l.hide(DocMap);
        assert_eq!(
            update(&mut ws, &l, Workspace),
            owed(Some(TabSwitch::In), true)
        );
        assert_eq!(update(&mut map, &l, DocMap), Owed::default());
        l.show(DocMap);
        assert_eq!(
            update(&mut map, &l, DocMap),
            owed(Some(TabSwitch::In), true)
        );
    }

    /// A group dragged onto another keeps its front tab in front: that
    /// panel changes container and is relaid out, but is not switched —
    /// it never left the screen. The target's front tab goes behind it.
    #[test]
    fn a_group_dragged_onto_another_keeps_its_front_tab_in_front() {
        let mut l = DockLayout::new();
        l.show(Workspace);
        l.show(DocMap);
        let (mut ws, mut map) = (PanelTold::default(), PanelTold::default());
        update(&mut ws, &l, Workspace);
        update(&mut map, &l, DocMap);
        let target = l.group_of(Workspace).expect("shown").id;
        let dragged = l.group_of(DocMap).expect("shown").id;
        l.move_group(dragged, DropTarget::IntoGroup(target));
        assert_eq!(
            update(&mut map, &l, DocMap),
            Owed {
                container: Some(DMN_DOCK),
                switch: None,
                relaid: true
            },
            "from the right (CONT_RIGHT) into the left group (CONT_LEFT)"
        );
        assert_eq!(
            update(&mut ws, &l, Workspace),
            owed(Some(TabSwitch::Off), true)
        );
    }

    /// Floating a group moves its panel to a new container and lays it out
    /// anew without switching it; moving the float again only relays it
    /// out, and setting the same rect again is no move at all.
    #[test]
    fn a_floated_or_moved_group_is_relaid_out_without_a_switch() {
        let mut l = DockLayout::new();
        l.show(Workspace);
        let mut ws = PanelTold::default();
        update(&mut ws, &l, Workspace);
        let group = l.group_of(Workspace).expect("shown").id;
        l.move_group(
            group,
            DropTarget::Floating(DockRect::new(100, 100, 300, 400)),
        );
        let floated = update(&mut ws, &l, Workspace);
        assert_eq!(floated.container.map(|c| c & 0xFFFF), Some(DMN_FLOAT));
        assert_eq!(floated.switch, None, "still in front: nothing switched");
        assert!(floated.relaid);
        l.set_floating_rect(group, DockRect::new(150, 120, 300, 400));
        assert_eq!(update(&mut ws, &l, Workspace), owed(None, true));
        l.set_floating_rect(group, DockRect::new(150, 120, 300, 400));
        assert_eq!(update(&mut ws, &l, Workspace), Owed::default());
    }

    /// A docked panel is relaid out when its own rectangle changes and not
    /// otherwise: a left band keeps its rect when the area narrows, and
    /// changes it when the area gets shorter or the band is dragged wider.
    #[test]
    fn a_docked_panel_is_relaid_out_only_when_its_band_changes() {
        let mut l = DockLayout::new();
        l.show(Workspace);
        let mut ws = PanelTold::default();
        update(&mut ws, &l, Workspace);
        let narrower = compute_frame(DockRect::new(0, 0, 900, 700), &l, 200, 60);
        assert_eq!(ws.update(&l, Some(&narrower), Workspace), Owed::default());
        let shorter = compute_frame(DockRect::new(0, 0, 900, 600), &l, 200, 60);
        assert_eq!(ws.update(&l, Some(&shorter), Workspace), owed(None, true));
        l.set_side_size(DockSide::Left, l.side_size(DockSide::Left) + 60);
        let wider_band = compute_frame(DockRect::new(0, 0, 900, 600), &l, 200, 60);
        assert_eq!(
            ws.update(&l, Some(&wider_band), Workspace),
            owed(None, true)
        );
    }

    /// With no frame — the dock area not laid out yet — a docked panel has
    /// nowhere to have been laid out, so it is owed its `DMN_FLOATDROPPED`
    /// once there is one. A floating panel's rectangle is its own window's
    /// and needs no frame.
    #[test]
    fn nothing_docked_is_relaid_out_before_the_area_has_a_size() {
        let mut l = DockLayout::new();
        l.show(Workspace);
        let mut ws = PanelTold::default();
        let unplaced = ws.update(&l, None, Workspace);
        assert_eq!(unplaced.switch, Some(TabSwitch::In));
        assert!(!unplaced.relaid);
        assert!(update(&mut ws, &l, Workspace).relaid);

        let group = l.group_of(Workspace).expect("shown").id;
        l.move_group(group, DropTarget::Floating(DockRect::new(0, 0, 300, 200)));
        assert!(ws.update(&l, None, Workspace).relaid);
    }

    /// The first update for a panel already on screen owes all three at
    /// once — where it is, that it is in front, where it is laid out. That
    /// is a panel a restored session showed before its plugin loaded and
    /// registered it.
    #[test]
    fn a_panel_found_on_screen_at_registration_is_owed_all_three() {
        let mut l = DockLayout::new();
        l.show(Workspace);
        let mut told = PanelTold::default();
        assert_eq!(
            update(&mut told, &l, Workspace),
            Owed {
                container: Some(DMN_DOCK),
                switch: Some(TabSwitch::In),
                relaid: true
            }
        );
    }

    /// A parked panel — open, but out of every group because no plugin
    /// can supply it this session — is off screen like a closed one and is
    /// owed nothing for leaving. Back, it comes on screen where it is laid
    /// out. Parking takes plugin panels only, so this one is interned.
    #[test]
    fn a_parked_panel_is_owed_nothing_until_it_comes_back() {
        let p = codepp_core::dock::intern_plugin_panel("dmn-park.dll", "Park P").expect("intern");
        let mut l = DockLayout::new();
        l.show(p);
        let mut told = PanelTold::default();
        update(&mut told, &l, p);
        assert!(l.park(&[p]));
        assert_eq!(update(&mut told, &l, p), Owed::default());
        assert!(l.unpark(&[p]));
        assert_eq!(update(&mut told, &l, p), owed(Some(TabSwitch::In), true));
    }

    /// A background tab dragged into another group comes to the front
    /// there: new container, on screen, laid out anew. The target's front
    /// tab goes behind it and gains the tab bar; the front tab of the group
    /// it left stays in front and loses the tab bar.
    #[test]
    fn a_background_tab_dragged_into_another_group_comes_on_screen() {
        let (mut l, mut ws, mut map) = two_tabs();
        let p = codepp_core::dock::intern_plugin_panel("dmn-move.dll", "Move P").expect("intern");
        l.show(p);
        let mut pt = PanelTold::default();
        update(&mut pt, &l, p);
        let target = l.group_of(p).expect("shown").id;
        l.move_panel(Workspace, DropTarget::IntoGroup(target));
        assert_eq!(
            update(&mut ws, &l, Workspace),
            Owed {
                container: Some((CONT_BOTTOM << 16) | DMN_DOCK),
                switch: Some(TabSwitch::In),
                relaid: true
            },
            "from behind a tab on the left to the front of the bottom group"
        );
        assert_eq!(update(&mut pt, &l, p), owed(Some(TabSwitch::Off), true));
        assert_eq!(update(&mut map, &l, DocMap), owed(None, true));
    }

    /// A docked panel whose dock area loses its size — the window squeezed
    /// until nothing is left for it — has no placement and is owed nothing
    /// for losing it; once the area has a size again, it is laid out anew.
    #[test]
    fn a_docked_panel_is_relaid_out_once_its_area_has_a_size_again() {
        let mut l = DockLayout::new();
        l.show(Workspace);
        let mut ws = PanelTold::default();
        update(&mut ws, &l, Workspace);
        assert_eq!(ws.update(&l, None, Workspace), Owed::default());
        assert_eq!(update(&mut ws, &l, Workspace), owed(None, true));
    }

    /// The order a reconcile sends in: every container first, then every
    /// `DMN_SWITCHIN`, every `DMN_SWITCHOFF`, every `DMN_FLOATDROPPED` —
    /// so a tab switch tells the panel coming in before the one going out,
    /// as Notepad++ does, whichever of the two registered first.
    #[test]
    fn notices_go_out_by_kind_container_first() {
        let docked_bottom = (CONT_BOTTOM << 16) | DMN_DOCK;
        let owed = [
            ("a", owed(Some(TabSwitch::Off), true)),
            (
                "b",
                Owed {
                    container: Some(docked_bottom),
                    switch: Some(TabSwitch::In),
                    relaid: true,
                },
            ),
            ("c", Owed::default()),
        ];
        assert_eq!(
            delivery_order(&owed),
            [
                ("b", docked_bottom),
                ("b", DMN_SWITCHIN),
                ("a", DMN_SWITCHOFF),
                ("a", DMN_FLOATDROPPED),
                ("b", DMN_FLOATDROPPED),
            ]
        );
    }

    /// Against Notepad++'s literals, not the constants — a test comparing
    /// a constant with itself cannot fail on a wrong one — and with the
    /// high word empty: Notepad++'s own panels compare the whole code for
    /// these three, so a container number there would hide them.
    #[test]
    fn switch_and_relayout_codes_are_notepad_plus_pluss_bare_numbers() {
        let owed = [
            ((), owed(Some(TabSwitch::In), true)),
            ((), owed(Some(TabSwitch::Off), false)),
        ];
        let codes: Vec<u32> = delivery_order(&owed)
            .into_iter()
            .map(|((), code)| code)
            .collect();
        assert_eq!(codes, [1054, 1055, 1056]);
    }

    #[test]
    fn dmn_names_read_the_low_word() {
        assert_eq!(dmn_name((CONT_BOTTOM << 16) | DMN_DOCK), "DMN_DOCK");
        assert_eq!(dmn_name(DMN_SWITCHIN), "DMN_SWITCHIN");
        assert_eq!(dmn_name(DMN_SWITCHOFF), "DMN_SWITCHOFF");
        assert_eq!(dmn_name(DMN_FLOATDROPPED), "DMN_FLOATDROPPED");
        assert_eq!(dmn_name(DMN_CLOSE), "DMN_CLOSE");
        assert_eq!(dmn_name(0), "DMN_?");
    }
}

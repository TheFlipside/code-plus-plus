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

use codepp_core::dock::{DockContainer, DockLayout, DockSide};

use crate::ffi::{
    CONT_BOTTOM, CONT_LEFT, CONT_RIGHT, CONT_TOP, DMN_DOCK, DMN_FLOAT, DOCKCONT_MAX,
    DWS_DF_CONT_BOTTOM, DWS_DF_CONT_LEFT, DWS_DF_CONT_RIGHT, DWS_DF_CONT_TOP, DWS_DF_FLOATING,
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

#[cfg(test)]
mod tests {
    use super::*;
    use codepp_core::dock::{DockRect, DropTarget};

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
}

//! The per-module allowance on plugin-panel identities, through the real
//! process-wide interning table.
//!
//! An integration test because it runs in a process of its own. The
//! table never shrinks and is shared by every test in a binary, so the
//! unit tests in `dock.rs` cannot fill a module's allowance without
//! starving their siblings (see `MAX_PLUGIN_PANELS`). This binary has one
//! test, and nothing else interns in it.

use codepp_core::dock::{intern_plugin_panel, DockLayout, MAX_PLUGIN_PANELS_PER_MODULE};
use codepp_core::session::{DockGroupSession, DockPanelSession, DockSession};

#[test]
fn one_plugin_cannot_register_away_the_table() {
    // A plugin that registers a fresh name each time runs out of names of
    // its own…
    for i in 0..MAX_PLUGIN_PANELS_PER_MODULE {
        assert!(
            intern_plugin_panel("greedy.dll", &format!("Panel {i}")).is_some(),
            "name {i} is within the allowance"
        );
    }
    assert!(
        intern_plugin_panel("greedy.dll", "One Too Many").is_none(),
        "a name past the allowance is refused"
    );
    // …while a name it already has still resolves, taking no slot…
    assert!(intern_plugin_panel("greedy.dll", "Panel 0").is_some());
    // …and every other plugin goes on registering.
    assert!(intern_plugin_panel("polite.dll", "Console").is_some());

    // Names a restored session creates are not charged to the plugin they
    // name, so a `session.xml` naming many of a real plugin's panels —
    // crafted, or grown over years — leaves its allowance whole.
    let session = DockSession {
        groups: vec![DockGroupSession {
            side: "bottom".into(),
            panels: (0..MAX_PLUGIN_PANELS_PER_MODULE)
                .map(|i| DockPanelSession {
                    kind: format!("plugin:real.dll|Restored {i}"),
                    ..DockPanelSession::default()
                })
                .collect(),
            ..DockGroupSession::default()
        }],
        ..DockSession::default()
    };
    let restored = DockLayout::from_session(&session);
    assert_eq!(
        restored.open_plugin_panels().len(),
        MAX_PLUGIN_PANELS_PER_MODULE,
        "the session's panels were restored"
    );
    assert!(
        intern_plugin_panel("real.dll", "Live Console").is_some(),
        "the plugin's own registrations are unaffected"
    );
}

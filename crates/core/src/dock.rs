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
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// Identity of a plugin-registered dock panel.
///
/// Held by [`DockPanel::Plugin`] as a `&'static`, which is what lets
/// `DockPanel` stay `Copy` — and `Copy` is not a nicety here: the
/// type is a `HashMap` key, sits in const arrays, and is copied
/// through every layout computation and every backend's reconciler.
/// A `String` in the enum would have rewritten all of that.
///
/// See [`intern_plugin_panel`] for how these come to be `'static`.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct PluginPanelIdent {
    /// The plugin's module name (`tTbData.pszModuleName`), which is
    /// what disambiguates two plugins offering the same panel name.
    pub module: String,
    /// The panel's display name (`tTbData.pszName`), shown on the
    /// caption and the tab, and the key `NPPM_DMMVIEWOTHERTAB` and
    /// `NPPM_DMMGETPLUGINHWNDBYNAME` look up.
    pub name: String,
    /// [`DockPanel::persist_key`]'s answer, precomputed so it can be
    /// returned as `&'static str` like the built-in panels'.
    key: String,
    /// Whether persisted text — a restored `session.xml` — created this
    /// identity, rather than a plugin's registration. Only identities a
    /// registration created count against
    /// [`MAX_PLUGIN_PANELS_PER_MODULE`].
    restored: bool,
}

/// Registered plugin panels, in interning order.
///
/// Leaked on purpose. A plugin panel is registered once and never
/// unregistered — `PluginHost` never unloads a plugin, and a panel
/// outlives every layout that mentions it — so the alternative to
/// leaking is a registry that every `title()` call site would have to
/// be handed. Bounded by [`MAX_PLUGIN_PANELS`]; a process at the cap
/// refuses further registrations rather than growing.
static PLUGIN_PANELS: std::sync::Mutex<Vec<&'static PluginPanelIdent>> =
    std::sync::Mutex::new(Vec::new());

/// Ceiling on interned plugin panels, mirroring the Win32 host's own
/// registration cap. The point is that the leak above is bounded by a
/// constant rather than by whatever a plugin — or a hand-edited
/// `session.xml` — asks for.
///
/// Deliberately not covered by a test that reaches it. The table is
/// process-global and never shrinks, so a test that interns 64
/// identities would starve every sibling test in the same binary of
/// the ability to intern one — an order-dependent suite, traded for a
/// property the cap check makes structurally: it is the one early
/// return inside the single mutex-guarded critical section, ahead of
/// every allocation, on the only path that can add an entry. What
/// *is* tested is the boundary that a caller can actually cross,
/// [`MAX_PLUGIN_PANEL_FIELD_LEN`].
pub const MAX_PLUGIN_PANELS: usize = 64;

/// How many panel identities one module's registrations may create in a
/// process.
///
/// The table above never gives a slot back, so without this one plugin
/// could fill it: register a panel under a fresh name each time — title
/// it after the current file, say, and register anew on every switch —
/// and from the 64th name on every plugin's registration is refused
/// until the process ends. With it, such a plugin runs out of names of
/// its own and nobody else's. Counted per module name over identities a
/// registration created; one a restored session created is not charged,
/// so a crafted `session.xml` cannot spend a real plugin's allowance
/// either — persisted text has [`MAX_RESTORED_PLUGIN_PANELS`] for that.
/// Generous against real plugins, which register one or two panels.
///
/// The module name is what the registering plugin *declares*
/// (`tTbData.pszModuleName`); nothing ties it to the plugin that sent
/// the message. So a plugin that varies its module name gets round the
/// allowance, and one that declares another plugin's name can spend
/// that plugin's. Neither is a mistake a plugin makes by accident — a
/// module name is a file name, not text a plugin composes — and a
/// plugin set on doing it runs in this process and needs no help from
/// the host to do worse (DESIGN.md §6.5), the same reasoning the
/// command signature's docs give for the same declared name.
pub const MAX_PLUGIN_PANELS_PER_MODULE: usize = 8;

/// Ceiling on the bytes of either half of a plugin panel's identity.
///
/// [`MAX_PLUGIN_PANELS`] bounds how many identities can be leaked;
/// this bounds how large one can be, which matters because
/// `DockPanel::from_persist_key` interns straight from
/// `session.xml` — a file the user can edit and a crash can
/// truncate. Generous against any real module filename or panel
/// title, so it never rejects something legitimate.
pub const MAX_PLUGIN_PANEL_FIELD_LEN: usize = 256;

/// How many distinct plugin panels one restored session may name: half
/// of [`MAX_PLUGIN_PANELS`].
///
/// Every plugin panel a session names is interned when the session is
/// read, and the interning table is process-wide and never shrinks.
/// Unbounded, a hand-edited or damaged `session.xml` naming 64 made-up
/// panels would fill it, and every real plugin's `NPPM_DMMREGASDCKDLG`
/// would be refused for the rest of the process. Half leaves room for
/// registrations that no session can take, and a real session names a
/// handful. Panels in groups are admitted before remembered ones, so
/// what an unusually long history loses past the bound is a remembered
/// position; only a session naming more open plugin panels than the
/// bound itself would lose an open one, and then the ones it names last.
///
/// The bound is on the process, not on one read: every identity created
/// from persisted text is charged to `RESTORED_PLUGIN_PANELS`, under
/// the table's own lock, so no number of reads — of the same session or
/// another — takes more. An identity already in the table (registered by
/// its plugin, or created by an earlier read) takes no slot and is not
/// charged, which is also what keeps repeated reads of one session
/// resolving the same panels.
pub const MAX_RESTORED_PLUGIN_PANELS: usize = MAX_PLUGIN_PANELS / 2;

/// How many plugin-panel identities persisted text has created in this
/// process — the budget [`MAX_RESTORED_PLUGIN_PANELS`] bounds.
///
/// Process-wide and never reset, so it is shared by every test in a
/// test binary: a test that restores new plugin panels should pass a
/// budget of its own to `DockLayout::from_session_budgeted` rather than
/// spend this one through [`DockLayout::from_session`].
static RESTORED_PLUGIN_PANELS: AtomicUsize = AtomicUsize::new(0);

/// Intern `(module, name)` into a [`DockPanel::Plugin`].
///
/// Idempotent: the same pair always yields the same panel, so a
/// plugin re-registering, and a `session.xml` naming a panel that
/// plugin also registers, converge on one identity rather than two
/// that compare unequal.
///
/// `None` for an empty or over-long `module` / `name` (see
/// [`MAX_PLUGIN_PANEL_FIELD_LEN`]), once [`MAX_PLUGIN_PANELS`]
/// distinct panels exist, and for a new name once `module`'s
/// registrations have created [`MAX_PLUGIN_PANELS_PER_MODULE`].
///
/// A `|` in either half is refused too: it is the separator
/// [`DockPanel::persist_key`] joins them with, so a name carrying one
/// would round-trip back through `DockPanel::from_persist_key` as a
/// *different* split — a panel able to collide with another plugin's
/// identity by choosing its own title.
///
/// And so is any character the display policy rejects
/// ([`crate::display::is_display_hostile`]): the name is what every
/// caption and tab label draws, so a bidi override or a control
/// character in it reaches the chrome. Registration sanitizes a
/// plugin's text before it gets here, which is why nothing legitimate
/// is lost — what this refuses is a key read back from a hand-edited
/// `session.xml`, which `DockPanel::from_persist_key` interns raw. One
/// check at the one place identities are made covers both routes.
#[must_use]
pub fn intern_plugin_panel(module: &str, name: &str) -> Option<DockPanel> {
    intern_plugin_panel_with(module, name, false, || true)
}

/// [`intern_plugin_panel`], asking `may_create` — under the table's lock
/// — before it creates a *new* identity. An identity already in the
/// table resolves without asking: it takes no slot.
///
/// `restored` says where the name comes from: `true` for persisted
/// text, which a restore budgets through `may_create`, and `false` for a
/// registration, which is held to [`MAX_PLUGIN_PANELS_PER_MODULE`]
/// instead.
///
/// The lock is not reentrant, so `may_create` must not intern anything
/// itself: a closure that reached back into the table would deadlock
/// every dock-panel operation on the thread, live registrations
/// included. Counting against a budget is what it is for.
fn intern_plugin_panel_with(
    module: &str,
    name: &str,
    restored: bool,
    may_create: impl FnOnce() -> bool,
) -> Option<DockPanel> {
    let usable = |s: &str| {
        !s.is_empty()
            && s.len() <= MAX_PLUGIN_PANEL_FIELD_LEN
            && !s.contains(PLUGIN_PANEL_SEPARATOR)
            && !s.chars().any(crate::display::is_display_hostile)
    };
    if !usable(module) || !usable(name) {
        return None;
    }
    let mut panels = PLUGIN_PANELS.lock().ok()?;
    if let Some(found) = panels.iter().find(|p| p.module == module && p.name == name) {
        return Some(DockPanel::Plugin(found));
    }
    // `may_create` goes last: it charges a budget, which a refusal for
    // any other reason must not spend.
    if panels.len() >= MAX_PLUGIN_PANELS
        || (!restored && !registrations_may_create(&panels, module))
        || !may_create()
    {
        return None;
    }
    let ident: &'static PluginPanelIdent = Box::leak(Box::new(PluginPanelIdent {
        module: module.to_string(),
        name: name.to_string(),
        key: format!("{PLUGIN_PANEL_KEY_PREFIX}{module}{PLUGIN_PANEL_SEPARATOR}{name}"),
        restored,
    }));
    panels.push(ident);
    Some(DockPanel::Plugin(ident))
}

/// Whether `module`'s registrations may create another identity in
/// `panels`: fewer than [`MAX_PLUGIN_PANELS_PER_MODULE`] of the ones a
/// registration created carry that module name. Identities a restore
/// created are not counted.
fn registrations_may_create(panels: &[&PluginPanelIdent], module: &str) -> bool {
    panels
        .iter()
        .filter(|p| !p.restored && p.module == module)
        .count()
        < MAX_PLUGIN_PANELS_PER_MODULE
}

/// A plugin panel's persisted key split into its module and name, with
/// nothing interned. `None` for any other key.
fn plugin_key_parts(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix(PLUGIN_PANEL_KEY_PREFIX)?
        .split_once(PLUGIN_PANEL_SEPARATOR)
}

/// Prefix distinguishing a plugin panel's `session.xml` key from the
/// built-in panels' bare keys. The separator inside is `|`, which
/// cannot appear in a Win32 module filename.
const PLUGIN_PANEL_KEY_PREFIX: &str = "plugin:";

/// Separator between the two halves of a plugin panel's key. Refused
/// inside either half by [`intern_plugin_panel`], so the split is
/// unambiguous in both directions.
const PLUGIN_PANEL_SEPARATOR: char = '|';

/// A dockable panel: the two built-in ones, or a plugin-registered
/// panel identified by an interned [`PluginPanelIdent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DockPanel {
    /// "Folder as Workspace" — the lazily-populated folder tree.
    Workspace,
    /// "Document Map" — the miniature second Scintilla view.
    DocMap,
    /// A panel a plugin registered through `NPPM_DMMREGASDCKDLG`.
    /// The Win32 and GTK backends create these — the message carries
    /// the plugin's own window, an `HWND` there and a `GtkWidget*`
    /// here — and Cocoa declines it.
    Plugin(&'static PluginPanelIdent),
}

impl DockPanel {
    /// The panels that exist in every process, for iteration.
    ///
    /// **Not every panel**: plugin panels are interned at
    /// registration and are not in here, so a caller that means
    /// "every panel this layout might mention" must not iterate
    /// this. There is deliberately no such iterator — the places that
    /// need the full set are the Win32 and GTK dock reconcilers, which
    /// build it from the live registrations they already hold, so
    /// that a panel whose plugin has not loaded yet is absent rather
    /// than present with no content window.
    pub const BUILT_IN: [DockPanel; 2] = [DockPanel::Workspace, DockPanel::DocMap];

    /// Human-readable title shown in the group caption and on the
    /// panel's tab when active.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            DockPanel::Workspace => "Folder as Workspace",
            DockPanel::DocMap => "Document Map",
            DockPanel::Plugin(ident) => &ident.name,
        }
    }

    /// The plugin module that registered this panel, or `None` for a
    /// built-in one.
    #[must_use]
    pub fn plugin_module(self) -> Option<&'static str> {
        match self {
            DockPanel::Plugin(ident) => Some(&ident.module),
            _ => None,
        }
    }

    /// Stable key used in `session.xml`. Never rename a value here —
    /// the strings are the wire format.
    #[must_use]
    pub fn persist_key(self) -> &'static str {
        match self {
            DockPanel::Workspace => "workspace",
            DockPanel::DocMap => "docmap",
            DockPanel::Plugin(ident) => &ident.key,
        }
    }

    /// Inverse of [`Self::persist_key`]. `None` for an unknown key —
    /// a session written by a future build with more panels loads
    /// with those panels silently dropped rather than erroring, the
    /// same forward-compatibility posture the rest of `session.xml`
    /// takes.
    ///
    /// A `plugin:` key interns, so a restored layout can place a
    /// panel *before* the plugin that owns it has been lazily loaded
    /// — which is the normal case, since plugins load on first touch
    /// and the layout is restored at startup. The panel simply has no
    /// content window until its plugin registers.
    ///
    /// It interns *without* charging [`MAX_RESTORED_PLUGIN_PANELS`]'s
    /// budget, so persisted text must not come through here: a restore
    /// goes through [`DockLayout::from_session`], which routes every
    /// plugin key through the budget and hands this function only the
    /// host's own keys. Crate-private so no other crate can bypass that.
    /// A plugin key that did reach here would be interned as a
    /// registration, charged to its module's
    /// [`MAX_PLUGIN_PANELS_PER_MODULE`] — the other reason no restore
    /// may use it. Only tests pass it one.
    #[must_use]
    pub(crate) fn from_persist_key(key: &str) -> Option<DockPanel> {
        match key {
            "workspace" => Some(DockPanel::Workspace),
            "docmap" => Some(DockPanel::DocMap),
            other => {
                let (module, name) = plugin_key_parts(other)?;
                intern_plugin_panel(module, name)
            }
        }
    }

    /// The side a panel docks to the first time it is shown with no
    /// remembered location — the pre-dock fixed positions, kept so
    /// the refactor changes nothing for a user who never drags.
    ///
    /// Plugin panels default to the bottom, which is where Notepad++
    /// puts a console and where a panel that expresses no preference
    /// is least in the way. A plugin that *does* express one, through
    /// `tTbData.u_mask`'s `DWS_DF_CONT_*` bits, is placed there
    /// instead by the caller — this is only the fallback.
    #[must_use]
    pub fn default_side(self) -> DockSide {
        match self {
            DockPanel::Workspace => DockSide::Left,
            DockPanel::DocMap => DockSide::Right,
            DockPanel::Plugin(_) => DockSide::Bottom,
        }
    }
}

/// A signature over one plugin panel's startup command: the
/// HMAC-SHA256 of [`restore_command_message`] under a key only the
/// user's own account can read.
///
/// Code++ restores a plugin panel by running the plugin's own command
/// for it — see [`DockLayout::set_open_command`] — and that command's
/// index is saved in `session.xml`, a file anyone who can write the
/// user's profile can change. Code++ signs a command only when the
/// plugin the panel is named for is the plugin that registered it, and
/// with Preferences → Security's guard on it runs a saved command only
/// when the signature checks out. So neither an edited or copied
/// session file nor a plugin registering a panel under another plugin's
/// name chooses what runs at startup.
///
/// This module neither computes nor checks one. The key and the MAC
/// are platform code, and the policy is the shell's. What lives here is
/// the value, its wire form and the exact message it signs, so the
/// persisted layout carries it like any other field.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CommandSeal([u8; 32]);

impl CommandSeal {
    /// Wrap a MAC.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Whether `other` is the same signature. Every byte is compared
    /// whatever the first difference, which costs nothing and leaves no
    /// timing to measure.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }

    /// The `session.xml` spelling: 64 lowercase hexadecimal digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;
        self.0.iter().fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    /// Inverse of [`Self::to_hex`]. `None` for anything else — the wrong
    /// length or alphabet, uppercase included — which the restore then
    /// treats as no signature at all.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let digits = text.as_bytes();
        if digits.len() != 64 {
            return None;
        }
        let nibble = |d: u8| match d {
            b'0'..=b'9' => Some(d - b'0'),
            b'a'..=b'f' => Some(d - b'a' + 10),
            _ => None,
        };
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
            *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(bytes))
    }
}

impl std::fmt::Debug for CommandSeal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CommandSeal({})", self.to_hex())
    }
}

/// What a [`CommandSeal`] signs, first: the purpose and its version.
///
/// A signature made for this cannot be taken for anything else a later
/// version signs with the same key. Bump the version if the fields
/// change. Every saved signature then stops checking out, and each
/// panel needs opening by hand once — the same as when the account can
/// no longer read the key.
const RESTORE_COMMAND_LABEL: &[u8] = b"Code++ plugin panel startup command v1\0";

/// The bytes a [`CommandSeal`] signs for `panel`'s startup command
/// `command`: [`RESTORE_COMMAND_LABEL`], the panel's persisted key, a
/// NUL, and the index in decimal. Neither half of a plugin panel's
/// identity can hold a control character ([`intern_plugin_panel`]
/// refuses them), so no two records share a message.
///
/// `None` for the host's own panels, which have no plugin command.
#[must_use]
pub fn restore_command_message(panel: DockPanel, command: i32) -> Option<Vec<u8>> {
    if !matches!(panel, DockPanel::Plugin(_)) {
        return None;
    }
    let mut message = RESTORE_COMMAND_LABEL.to_vec();
    message.extend_from_slice(panel.persist_key().as_bytes());
    message.push(0);
    message.extend_from_slice(command.to_string().as_bytes());
    Some(message)
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

/// Which *container* a panel is in, in the sense a Notepad++ plugin
/// is told about through `DMN_DOCK` / `DMN_FLOAT`.
///
/// Upstream's docking manager has exactly one container per side,
/// plus one per floating window. Code++'s model is finer than that —
/// a side can hold several stacked groups — so a docked panel's
/// container is its *side*, and moving between two groups on one side
/// is not a container change. That is also what upstream does when a
/// tab is dragged between two panels sharing its one bottom
/// container: nothing, because nothing about where the panel is
/// docked has changed.
///
/// A floating panel's container is its group, identified by the
/// group's id while the panel is on screen. A hidden panel whose
/// remembered spot is floating has no group — it gets a fresh one
/// when it is shown again — so it is `Floating(None)`, which
/// [`Self::is_same`] treats as matching any floating container. That
/// is what keeps a hide-then-show of a floating panel from reading as
/// a move: upstream hides a panel inside its container and shows it
/// back there, and tells the plugin nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockContainer {
    Docked(DockSide),
    Floating(Option<u32>),
}

impl DockContainer {
    /// Whether a panel moving from `self` to `other` has changed
    /// container — the edge `DMN_DOCK` / `DMN_FLOAT` are sent on.
    ///
    /// Plain equality except for the one case the type exists for: a
    /// floating container that has no group yet (a hidden panel)
    /// matches any floating container, because the group it will get
    /// on show is a new identity for the same place.
    #[must_use]
    pub fn is_same(self, other: DockContainer) -> bool {
        match (self, other) {
            (DockContainer::Docked(a), DockContainer::Docked(b)) => a == b,
            (DockContainer::Floating(Some(a)), DockContainer::Floating(Some(b))) => a == b,
            (DockContainer::Floating(_), DockContainer::Floating(_)) => true,
            _ => false,
        }
    }
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
    /// Session-stable identity: allocated in increasing order and never
    /// handed out while anything still names it (see
    /// `DockLayout::alloc_id`) — the same "key on ids, not indices or
    /// pointers" rule the tab strip's arm/commit fix established
    /// (§7.4). Not persisted: a restored layout numbers its groups
    /// afresh.
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

/// The command recorded for one plugin panel — see
/// [`DockLayout::set_open_command`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenCommand {
    panel: DockPanel,
    /// `tTbData.dlgID`: the index of the plugin's `FuncItem` that
    /// shows the panel.
    index: i32,
    /// Code++'s signature over `(panel, index)`, when the plugin the
    /// panel is named for is the plugin that registered it — or the
    /// signature a saved session carried, which is not checked here.
    seal: Option<CommandSeal>,
}

/// A plugin panel that is open in the layout but that no plugin can
/// supply this session, and where it was when it was set aside — see
/// [`DockLayout::park`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parked {
    panel: DockPanel,
    /// The group the panel was in. Still in the layout while that
    /// group holds other panels; gone if the panel was its last.
    group: u32,
    /// Where that group was when the panel left it, for recreating
    /// the group if it has gone.
    location: DockLocation,
    /// The group's index in the group list when the panel left it.
    group_pos: usize,
    /// The panel's tab index in the group when it left.
    index: usize,
    /// If the panel was its group's front tab and other tabs stayed
    /// behind, the one that took over the front. The panel takes the
    /// front back only while that one still has it — i.e. unless the
    /// user has since brought another tab forward. Re-pointed at the
    /// tab on screen when the rest of the layout is put back and parked
    /// again around it.
    front_after: Option<DockPanel>,
}

/// Put `parked`'s panel back into `groups`: into group `id` if it is
/// there, at its old tab index (clamped) and taking the front back if
/// it is owed it; otherwise as a new one-panel group `id` at the
/// parked location, at its old place in the list.
///
/// Shared by [`DockLayout::to_session`], which rebuilds the layout a
/// parked panel belongs to on a copy, and [`DockLayout::through_parked`],
/// which does it for real. Both put panels back in the reverse of the
/// order they were parked, the only order in which each record's
/// positions still mean what they did.
fn restore_parked(groups: &mut Vec<DockGroup>, parked: &Parked, id: u32) {
    if let Some(group) = groups.iter_mut().find(|g| g.id == id) {
        let front = group.active_panel();
        let at = parked.index.min(group.panels.len());
        group.panels.insert(at, parked.panel);
        group.active = if parked.front_after == Some(front) {
            at
        } else {
            group.panels.iter().position(|p| *p == front).unwrap_or(at)
        };
        return;
    }
    let at = parked.group_pos.min(groups.len());
    groups.insert(
        at,
        DockGroup {
            id,
            location: parked.location,
            panels: vec![parked.panel],
            active: 0,
        },
    );
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
    /// Where a panel that has *never* been placed should first
    /// appear, and the weaker of the two: [`Self::remembered`] wins,
    /// so anything the user chose survives. Separate from it rather
    /// than folded into it because the two mean different things —
    /// a remembered `Side` reopens the panel in a band of its own,
    /// whereas this says "the container on that side", which is the
    /// upstream docking manager's model and the reason two plugins
    /// asking for `DWS_DF_CONT_BOTTOM` become tabs rather than two
    /// stacked bands. Not persisted: it is re-seeded from each
    /// plugin's `tTbData.u_mask` at every registration.
    initial_side: Vec<(DockPanel, DockSide)>,
    /// The command that opens each plugin panel: `tTbData.dlgID`, the
    /// index of the plugin's own `FuncItem` that shows it. Persisted
    /// with the panel, because it is how the panel comes back — at the
    /// next start the host runs that command for every plugin panel
    /// that was open, which is what Notepad++ does (measured: it runs
    /// `FuncItem[dlgID]` between `NPPN_TBMODIFICATION` and
    /// `NPPN_READY`). Re-seeded at every registration, so the plugin's
    /// current value wins over a persisted one. Each carries the
    /// [`CommandSeal`] Code++ signed it with, if it did.
    open_commands: Vec<OpenCommand>,
    /// Open plugin panels no plugin can supply this session, in the
    /// order they were parked — see [`Self::park`]. Out of every
    /// group, so nothing presents them; written back where they were
    /// by [`Self::to_session`].
    parked: Vec<Parked>,
    /// The next group id to try. Counts up from 1 and wraps past
    /// `u32::MAX`; [`Self::alloc_id`] skips any id still in use.
    next_id: u32,
}

impl Default for DockLayout {
    fn default() -> Self {
        DockLayout {
            groups: Vec::new(),
            side_size: DEFAULT_SIDE_SIZE,
            remembered: Vec::new(),
            initial_side: Vec::new(),
            open_commands: Vec::new(),
            parked: Vec::new(),
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

    /// The container `panel` is in, or — if it is hidden — the one
    /// the next [`Self::show`] would put it in.
    ///
    /// Answering for a hidden panel is what lets a host tell a plugin
    /// where its panel lives at *registration*, before anything is on
    /// screen, the way upstream does. The precedence is exactly
    /// `show`'s: a visible panel's own group, then where a parked
    /// panel would come back, then a remembered location, then a
    /// registration-supplied side, then the panel's default side. If
    /// the two ever disagree, a plugin is told one container at
    /// registration and silently lands in another, so a test pins them
    /// against each other.
    #[must_use]
    pub fn container_of(&self, panel: DockPanel) -> DockContainer {
        let of_group = |group: &DockGroup| match group.location {
            DockLocation::Side(side) => DockContainer::Docked(side),
            DockLocation::Floating(_) => DockContainer::Floating(Some(group.id)),
        };
        if let Some(group) = self.group_of(panel) {
            return of_group(group);
        }
        if let Some(parked) = self.parked.iter().find(|p| p.panel == panel) {
            // Back into its group if that is still there, else into a
            // new group where its was — exactly what `unpark` does.
            return match self.group(parked.group) {
                Some(group) => of_group(group),
                None => match parked.location {
                    DockLocation::Side(side) => DockContainer::Docked(side),
                    DockLocation::Floating(_) => DockContainer::Floating(None),
                },
            };
        }
        if let Some(location) = self.remembered_for(panel) {
            return match location {
                DockLocation::Side(side) => DockContainer::Docked(side),
                DockLocation::Floating(_) => DockContainer::Floating(None),
            };
        }
        DockContainer::Docked(
            self.initial_side_for(panel)
                .unwrap_or_else(|| panel.default_side()),
        )
    }

    /// Position of floating group `id` among the floating groups, in
    /// creation order; `None` if it is not a floating group.
    ///
    /// Upstream numbers its floating containers after the four docked
    /// ones and reports that number to the plugin, so a host that
    /// wants to report *a* number needs an ordinal rather than a group
    /// id, which is unbounded and would not fit the 16 bits the
    /// notification carries it in.
    #[must_use]
    pub fn floating_ordinal(&self, id: u32) -> Option<usize> {
        self.floating_groups().position(|g| g.id == id)
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

    /// A group id nothing holds: not a live group's, and not the one a
    /// parked panel's record names, which is where that panel comes
    /// back.
    ///
    /// Ids count up, so in practice none is reused — but they wrap past
    /// `u32::MAX`, which a plugin showing and hiding one panel in a
    /// tight loop reaches in minutes (the security audit measured about
    /// four), and an id handed out twice lets an operation aimed at one
    /// group land on another. So an id still in use is skipped. The
    /// loop ends: one id per group and one per parked panel can be in
    /// use, a hundred-odd of four billion.
    fn alloc_id(&mut self) -> u32 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            let in_use =
                self.groups.iter().any(|g| g.id == id) || self.parked.iter().any(|p| p.group == id);
            if !in_use {
                return id;
            }
        }
    }

    fn remember(&mut self, panel: DockPanel, location: DockLocation) {
        self.remembered.retain(|(p, _)| *p != panel);
        self.remembered.push((panel, location));
    }

    /// Say which side's container `panel` should join the first
    /// time it is shown, **without** overriding a position the user
    /// already chose.
    ///
    /// For a plugin panel this carries `tTbData.u_mask`'s
    /// `DWS_DF_CONT_*` preference into the model. Upstream's docking
    /// manager has exactly one container per side, so that bit means
    /// "put me in the bottom container", not "give me a band of my
    /// own" — which is why [`Self::show`] *joins* an existing group
    /// on this side rather than inserting beside it.
    ///
    /// Precedence over a user's own arrangement falls out of where
    /// the two are stored rather than being special-cased:
    /// [`Self::show`] consults [`Self::remembered`] first, and that
    /// is what a hide (and a restored session) populates.
    pub fn set_initial_side(&mut self, panel: DockPanel, side: DockSide) {
        self.initial_side.retain(|(p, _)| *p != panel);
        self.initial_side.push((panel, side));
    }

    /// Record the command that opens plugin `panel` — its
    /// `tTbData.dlgID` — with the [`CommandSeal`] Code++ signed it with,
    /// if it signed it. Replaces whatever was recorded before, seal
    /// included: an unsigned registration leaves the panel unsigned.
    /// Ignored for the host's own panels, which have no plugin command.
    /// A negative index names no `FuncItem` and is not recorded.
    pub fn set_open_command(&mut self, panel: DockPanel, command: i32, seal: Option<CommandSeal>) {
        if !matches!(panel, DockPanel::Plugin(_)) {
            return;
        }
        self.open_commands.retain(|c| c.panel != panel);
        if command >= 0 {
            self.open_commands.push(OpenCommand {
                panel,
                index: command,
                seal,
            });
        }
    }

    /// The command recorded for plugin `panel`, if any.
    #[must_use]
    pub fn open_command_for(&self, panel: DockPanel) -> Option<i32> {
        self.open_command(panel).map(|(index, _)| index)
    }

    /// The command recorded for plugin `panel` and its seal, if any.
    #[must_use]
    pub fn open_command(&self, panel: DockPanel) -> Option<(i32, Option<CommandSeal>)> {
        self.open_commands
            .iter()
            .find(|c| c.panel == panel)
            .map(|c| (c.index, c.seal))
    }

    /// Every plugin panel the layout has open — in a group, docked or
    /// floating — in group then tab order.
    ///
    /// The set a load pass restores: a hidden panel is not brought
    /// back, just as Notepad++ restores only the panels it recorded as
    /// visible. The command for each is looked up separately, with
    /// [`Self::open_command_for`], and later — once the plugins have had
    /// `NPPN_TBMODIFICATION` — so a registration made there has already
    /// re-recorded it and the plugin's current `dlgID` wins over a
    /// persisted one.
    #[must_use]
    pub fn open_plugin_panels(&self) -> Vec<DockPanel> {
        self.groups
            .iter()
            .flat_map(|g| g.panels.iter().copied())
            .filter(|p| matches!(p, DockPanel::Plugin(_)))
            .collect()
    }

    /// The panels [`Self::park`] has set aside, in the order it did.
    #[must_use]
    pub fn parked_panels(&self) -> Vec<DockPanel> {
        self.parked.iter().map(|p| p.panel).collect()
    }

    /// Whether `panel` is parked.
    #[must_use]
    pub fn is_parked(&self, panel: DockPanel) -> bool {
        self.parked.iter().any(|p| p.panel == panel)
    }

    /// Take open plugin panels out of view for this session without
    /// closing them; returns whether anything was parked.
    ///
    /// For a panel whose plugin cannot supply it: not installed,
    /// disabled, or failed to load. Leaving it in its group shows a
    /// caption with nothing under it for the whole session, and
    /// closing it — [`Self::hide`] — records it closed, so it would not
    /// come back when the plugin does. Notepad++ does neither.
    /// Measured against 8.9.6 with its plugin removed, a panel it had
    /// saved open shows nothing, and the saved record is written back
    /// unchanged, so the panel returns the next time the plugin is
    /// installed.
    ///
    /// A parked panel is in no group, so nothing presents it and no
    /// backend needs to know it exists. [`Self::to_session`] writes it
    /// back where it was — into its group, at its tab position, in
    /// front if it was and no other tab has been brought forward since,
    /// or as its own group where its group was if that has gone — so a
    /// session in which nothing is touched saves exactly the layout it
    /// loaded. [`Self::unpark`] puts it back for real, and
    /// [`Self::show`] does so first.
    ///
    /// Only open plugin panels are parked; anything else in `panels`
    /// is ignored.
    pub fn park(&mut self, panels: &[DockPanel]) -> bool {
        let mut changed = false;
        for &panel in panels {
            if !matches!(panel, DockPanel::Plugin(_)) {
                continue;
            }
            let Some(group_pos) = self.groups.iter().position(|g| g.panels.contains(&panel)) else {
                continue;
            };
            let group = &self.groups[group_pos];
            let (group_id, location) = (group.id, group.location);
            let was_front = group.active_panel() == panel;
            let Some(index) = group.panels.iter().position(|p| *p == panel) else {
                continue;
            };
            self.remove_panel(panel);
            let front_after = if was_front {
                self.group(group_id).map(DockGroup::active_panel)
            } else {
                None
            };
            self.parked.push(Parked {
                panel,
                group: group_id,
                location,
                group_pos,
                index,
                front_after,
            });
            changed = true;
        }
        self.debug_assert_invariants();
        changed
    }

    /// Put parked panels back where they were; returns whether any came
    /// back. `panels` not parked are ignored.
    ///
    /// For a panel whose plugin has become able to supply it
    /// mid-session — re-enabled in the Plugin Manager and then loaded.
    /// Each comes back to its place in the layout [`Self::to_session`]
    /// saves: into its group if that is still there, else into a group
    /// recreated where its was, under a fresh id that every panel still
    /// parked from that group then follows.
    ///
    /// The result does not depend on the order panels come back in.
    /// Two plugins whose panels shared a group can be re-enabled on
    /// different days and their tabs still come back in their old
    /// order, with the old one in front; two groups that had gone come
    /// back in their old places among the rest. Nothing else on screen
    /// moves: every other tab keeps its place, and every group keeps
    /// the tab it has in front unless a panel coming back is owed it.
    pub fn unpark(&mut self, panels: &[DockPanel]) -> bool {
        let release: Vec<DockPanel> = panels
            .iter()
            .copied()
            .filter(|&panel| self.is_parked(panel))
            .collect();
        if release.is_empty() {
            return false;
        }
        self.through_parked(&release, |_| ());
        true
    }

    /// Run `edit` on the whole layout — every parked panel back where
    /// it belongs, as [`Self::to_session`] saves it — then park again
    /// each panel that was parked and is still open, except those in
    /// `release`. This is how a parked panel is put back, closed or
    /// moved.
    ///
    /// A parked panel's record places it relative to its group as that
    /// was when the panel left, which is only right once the panels
    /// parked after it are back: the reverse of the order they were
    /// parked, as `to_session` restores them. Put back on its own, into
    /// a group still missing panels parked before it, a panel lands in
    /// the wrong place — two parked from one group and put back in the
    /// order they were parked came back in the opposite order, three
    /// with the last in front came back with the first in front, and a
    /// group that had gone came back in another's place in the stack.
    /// Putting every one back first, then parking the rest again,
    /// leaves each record describing the layout as it now is.
    ///
    /// Parking the rest again would hand each group's front to
    /// whichever neighbour the removal rule picks, so a group on screen
    /// keeps the tab it had in front — unless a released panel is its
    /// front in the whole layout, being owed it — and the panel owed
    /// that group's front is re-pointed to take it back from that tab.
    fn through_parked<R>(&mut self, release: &[DockPanel], edit: impl FnOnce(&mut Self) -> R) -> R {
        let shown = self.fronts();
        let order = self.parked_panels();
        // The groups that had gone, by the id their panels' records still
        // name. Each comes back below under that id — which nothing else
        // holds, since `alloc_id` skips it — for the length of this call;
        // one that stays gets an id of its own at the end.
        let mut gone: Vec<u32> = self
            .parked
            .iter()
            .map(|r| r.group)
            .filter(|&id| self.group(id).is_none())
            .collect();
        gone.sort_unstable();
        gone.dedup();
        for record in std::mem::take(&mut self.parked).iter().rev() {
            restore_parked(&mut self.groups, record, record.group);
        }
        let result = edit(self);
        let whole = self.fronts();
        let again: Vec<DockPanel> = order
            .into_iter()
            .filter(|&panel| !release.contains(&panel) && self.is_visible(panel))
            .collect();
        self.park(&again);
        for (id, front) in shown {
            if whole
                .iter()
                .any(|&(group, panel)| group == id && release.contains(&panel))
            {
                continue;
            }
            let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
                continue;
            };
            let Some(i) = group.panels.iter().position(|p| *p == front) else {
                continue;
            };
            let displaced = group.active_panel();
            if displaced == front {
                continue;
            }
            group.active = i;
            // Parking the rest again handed the front down a chain of
            // panels ending at `displaced`; the last of them takes it
            // back from `front` now instead. At most one record names
            // `displaced`, and it is that one: every record here was made
            // by the single `park` call above, into a list the restore
            // had emptied, and within one call the front passes only to
            // a tab that keeps it until it is removed itself — which
            // `displaced`, still on screen, never was.
            let mut owed = self
                .parked
                .iter_mut()
                .filter(|r| r.group == id && r.front_after == Some(displaced));
            if let Some(record) = owed.next() {
                record.front_after = Some(front);
            }
            debug_assert!(
                owed.next().is_none(),
                "two parked records owe their front to {displaced:?}"
            );
        }
        // A group that had gone and now holds a released panel stays: it
        // is a new group on screen, so it takes a fresh id, and the panels
        // still parked from it follow. One that came back for the call
        // alone has gone again under the id its records name and spent
        // none, so a call costs one id per group that stays, however many
        // panels are parked.
        let staying: Vec<u32> = self
            .groups
            .iter()
            .map(|g| g.id)
            .filter(|id| gone.contains(id))
            .collect();
        for old in staying {
            let fresh = self.alloc_id();
            if let Some(group) = self.groups.iter_mut().find(|g| g.id == old) {
                group.id = fresh;
            }
            for record in &mut self.parked {
                if record.group == old {
                    record.group = fresh;
                }
            }
        }
        self.debug_assert_invariants();
        result
    }

    /// Each group's front tab, as `(group id, active panel)` in group
    /// order. Paired with [`Self::restore_fronts`].
    #[must_use]
    pub fn fronts(&self) -> Vec<(u32, DockPanel)> {
        self.groups
            .iter()
            .map(|g| (g.id, g.active_panel()))
            .collect()
    }

    /// Make each recorded panel its group's front tab again, where that
    /// group still exists and still holds it; returns whether anything
    /// changed.
    ///
    /// For putting back the tabs a user left in front after something
    /// else has brought others forward — restoring plugin panels does,
    /// because each one comes back through its plugin's "show" command
    /// and a show makes its panel the front tab. Notepad++ re-applies
    /// each container's saved front tab after those commands for the
    /// same reason. A group that has since gone, or a panel that has
    /// since moved to another, is left alone: the record describes one
    /// arrangement, and imposing it on a different one would be a
    /// guess.
    pub fn restore_fronts(&mut self, fronts: &[(u32, DockPanel)]) -> bool {
        let mut changed = false;
        for &(id, panel) in fronts {
            let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
                continue;
            };
            if let Some(i) = group.panels.iter().position(|p| *p == panel) {
                if group.active != i {
                    group.active = i;
                    changed = true;
                }
            }
        }
        changed
    }

    fn initial_side_for(&self, panel: DockPanel) -> Option<DockSide> {
        self.initial_side
            .iter()
            .find(|(p, _)| *p == panel)
            .map(|(_, s)| *s)
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
            let id = group.id;
            self.groups.remove(gi);
            // A panel parked from this group keeps the place the group
            // last had, which is where it comes back if nothing else
            // takes the group's place first.
            for parked in &mut self.parked {
                if parked.group == id {
                    parked.location = location;
                    parked.group_pos = gi;
                }
            }
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

    /// Forget every plugin panel: out of the groups, out of both
    /// placement tables.
    ///
    /// For a backend that cannot host one. A plugin registers its
    /// dock dialog through `NPPM_DMMREGASDCKDLG`, which the Cocoa
    /// host declines (DESIGN.md §7.4 — the `HWND`-shaped
    /// `UiPlatform` methods keep their trait defaults there), yet
    /// `session.xml` is portable and a layout written on Windows or
    /// Linux names panels by a key that interns on any platform. A
    /// backend that called this at restore never sees a panel it has
    /// no content window for; one that did not would render an empty
    /// group and have no way to close it. The backends that do host
    /// plugin panels park one whose plugin cannot supply it instead
    /// ([`Self::park`]), so it is saved back where it was.
    pub fn drop_plugin_panels(&mut self) {
        self.groups.retain_mut(|g| {
            g.panels.retain(|p| !matches!(p, DockPanel::Plugin(_)));
            g.active = g.active.min(g.panels.len().saturating_sub(1));
            !g.panels.is_empty()
        });
        self.remembered
            .retain(|(p, _)| !matches!(p, DockPanel::Plugin(_)));
        self.initial_side
            .retain(|(p, _)| !matches!(p, DockPanel::Plugin(_)));
        self.open_commands.clear();
        self.parked.clear();
        self.debug_assert_invariants();
    }

    /// Show `panel`. Already visible → just make it the active tab
    /// of its group (a "show" on an open-but-behind panel reveals
    /// it). Parked → back where it was, then to the front. Hidden →
    /// reopen at its remembered location, or the panel's default side
    /// on first ever open.
    pub fn show(&mut self, panel: DockPanel) {
        self.unpark(&[panel]);
        if self.is_visible(panel) {
            self.activate(panel);
            return;
        }
        if let Some(location) = self.remembered_for(panel) {
            self.insert_panel_at(panel, location);
            return;
        }
        // Never placed before. A registration-supplied side means the
        // *container* there (see `set_initial_side`), so join a group
        // that already occupies it; the panel's own default side is
        // only a fallback and opens a band of its own, which is what
        // the two built-in panels have always done.
        if let Some(side) = self.initial_side_for(panel) {
            if let Some(group) = self
                .groups
                .iter_mut()
                .find(|g| g.location == DockLocation::Side(side))
            {
                group.panels.push(panel);
                group.active = group.panels.len() - 1;
                self.debug_assert_invariants();
                return;
            }
            self.insert_panel_at(panel, DockLocation::Side(side));
            return;
        }
        self.insert_panel_at(panel, DockLocation::Side(panel.default_side()));
    }

    /// Hide `panel`, remembering where it was so the next
    /// [`Self::show`] reopens there. A parked panel is closed too: it
    /// stops being written back as open.
    pub fn hide(&mut self, panel: DockPanel) {
        if self.is_parked(panel) {
            // Closed from its place in the whole layout, like any open
            // panel, so it is remembered there and the panels parked
            // around it keep theirs.
            self.through_parked(&[], |layout| layout.hide(panel));
            return;
        }
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
        if self.is_parked(panel) {
            // Nothing on screen drags a parked panel. One moved by any
            // other route is placed explicitly, so it is no longer owed
            // its old spot: it moves from its place in the whole layout
            // and is not parked again. A drop onto a group that does
            // not exist leaves it parked, as it leaves anything else
            // where it was.
            if let DropTarget::IntoGroup(gid) = target {
                if self.group(gid).is_none() {
                    return;
                }
            }
            self.through_parked(&[panel], |layout| layout.move_panel(panel, target));
            return;
        }
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
                // A panel parked from the dragged group goes where its
                // tabs went.
                for parked in &mut self.parked {
                    if parked.group == id {
                        parked.group = other;
                    }
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
        for parked in &mut self.parked {
            if let DockLocation::Floating(r) = parked.location {
                parked.location = DockLocation::Floating(clamp_float_into(r, area));
            }
        }
    }

    /// Serialise for `session.xml`. Everything the model owns is
    /// captured: groups (with tab order + active tab + float
    /// rects), band sizes, and the remembered locations of hidden
    /// panels (so "close the panel, restart, reopen" lands where
    /// the user had it — the behaviour the fixed panels' persisted
    /// widths already promised).
    ///
    /// Parked panels are written as open, where they were — see
    /// [`Self::park`]. They are put back on a copy in the reverse of
    /// the order they were parked, which undoes the parking exactly
    /// for a group nothing else has touched.
    #[must_use]
    pub fn to_session(&self) -> DockSession {
        let mut groups = self.groups.clone();
        for parked in self.parked.iter().rev() {
            // A recreated group keeps the id it had: ids are not
            // persisted, none is reused, and it is what lets another
            // panel parked from the same group find it here.
            restore_parked(&mut groups, parked, parked.group);
        }
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
            groups: groups
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
                            .map(|p| {
                                let command = self.open_command(*p);
                                DockPanelSession {
                                    kind: p.persist_key().to_string(),
                                    cmd: command.map(|(index, _)| index),
                                    seal: command
                                        .and_then(|(_, seal)| seal)
                                        .map(|seal| seal.to_hex()),
                                }
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
        Self::from_session_budgeted(session, &RESTORED_PLUGIN_PANELS, MAX_RESTORED_PLUGIN_PANELS)
    }

    /// [`Self::from_session`], charging each plugin identity it creates
    /// to `budget` and refusing one once `cap` is reached. Parameters so
    /// a test can reach the bound with a budget of its own, without
    /// interning the dozens of identities the real one takes — the table
    /// is process-wide, and filling it would starve every sibling test
    /// of the ability to intern one.
    fn from_session_budgeted(
        session: &DockSession,
        budget: &AtomicUsize,
        cap: usize,
    ) -> DockLayout {
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
        // Naming a plugin panel interns it, so a panel not in the table
        // yet is charged to the budget and refused once the budget is
        // spent — see `MAX_RESTORED_PLUGIN_PANELS`. Groups are read before
        // remembered panels, so remembered positions are refused before
        // any open panel is; within each, it goes in document order.
        let admit = |kind: &str| -> Option<DockPanel> {
            let Some((module, name)) = plugin_key_parts(kind) else {
                return DockPanel::from_persist_key(kind);
            };
            intern_plugin_panel_with(module, name, true, || {
                // Relaxed: this runs under the table's lock, which
                // already orders it against every other charge.
                budget
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |spent| {
                        (spent < cap).then_some(spent + 1)
                    })
                    .is_ok()
            })
        };
        let mut seen: Vec<DockPanel> = Vec::new();
        for g in &session.groups {
            let Some(location) = parse_location(&g.side, g.x, g.y, g.w, g.h) else {
                continue;
            };
            let mut panels: Vec<DockPanel> = Vec::new();
            for saved in &g.panels {
                let Some(panel) = admit(&saved.kind) else {
                    continue;
                };
                if seen.contains(&panel) {
                    continue;
                }
                seen.push(panel);
                panels.push(panel);
                if let Some(command) = saved.cmd {
                    // Filtered like every other field read from a
                    // hand-editable file: `set_open_command` keeps only
                    // a plugin panel's non-negative index, and an index
                    // naming no command resolves to nothing when the
                    // host looks it up. The seal is only parsed here;
                    // whether it checks out is the shell's question,
                    // asked before the command runs.
                    let seal = saved.seal.as_deref().and_then(CommandSeal::from_hex);
                    layout.set_open_command(panel, command, seal);
                }
            }
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
            let Some(panel) = admit(&r.kind) else {
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
            let mut ids: Vec<u32> = Vec::new();
            for g in &self.groups {
                // Everything that addresses a group does it by id, so two
                // sharing one would send an operation to the wrong group —
                // what `alloc_id` exists to prevent.
                assert!(!ids.contains(&g.id), "two groups share id {}", g.id);
                ids.push(g.id);
                assert!(!g.panels.is_empty(), "empty group {} survived", g.id);
                assert!(g.active < g.panels.len(), "active out of range in {}", g.id);
                for p in &g.panels {
                    assert!(!seen.contains(p), "panel {p:?} in two groups");
                    seen.push(*p);
                }
            }
            for parked in &self.parked {
                assert!(
                    matches!(parked.panel, DockPanel::Plugin(_)),
                    "host panel {:?} parked",
                    parked.panel
                );
                assert!(
                    !seen.contains(&parked.panel),
                    "panel {:?} both parked and in a group, or parked twice",
                    parked.panel
                );
                seen.push(parked.panel);
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

    /// The per-module allowance counts what registrations created for
    /// that module, and nothing else. Checked over a table built here:
    /// no unit test fills the process-wide one (see
    /// `MAX_PLUGIN_PANELS`), which is why the check is a function of its
    /// own. `crates/core/tests/plugin_panel_quota.rs` drives the real
    /// table, in a process of its own.
    #[test]
    fn a_modules_registrations_have_an_allowance_of_their_own() {
        let ident = |module: &str, name: String, restored: bool| PluginPanelIdent {
            module: module.to_string(),
            name,
            key: String::new(),
            restored,
        };
        let registered: Vec<PluginPanelIdent> = (0..MAX_PLUGIN_PANELS_PER_MODULE)
            .map(|i| ident("a.dll", format!("A {i}"), false))
            .collect();
        let mut table: Vec<&PluginPanelIdent> = registered.iter().skip(1).collect();
        assert!(
            registrations_may_create(&table, "a.dll"),
            "one short of the allowance"
        );
        table.push(&registered[0]);
        assert!(
            !registrations_may_create(&table, "a.dll"),
            "at the allowance"
        );
        assert!(
            registrations_may_create(&table, "b.dll"),
            "another module's allowance is its own"
        );

        let restored: Vec<PluginPanelIdent> = (0..MAX_PLUGIN_PANELS_PER_MODULE)
            .map(|i| ident("c.dll", format!("C {i}"), true))
            .collect();
        let table: Vec<&PluginPanelIdent> = restored.iter().collect();
        assert!(
            registrations_may_create(&table, "c.dll"),
            "identities a restore created are not charged"
        );
    }

    fn mid() -> DockRect {
        DockRect::new(0, 0, 1000, 700)
    }

    /// The command that opens a plugin panel survives `session.xml`
    /// — it is what brings the panel back — and only a plugin panel's
    /// non-negative index is kept, whatever the file says.
    #[test]
    fn a_plugin_panels_open_command_round_trips_through_the_session() {
        let console = intern_plugin_panel("cmd-a.dll", "Console").expect("intern");
        let notes = intern_plugin_panel("cmd-b.dll", "Notes").expect("intern");
        let mut l = DockLayout::new();
        l.show(console);
        l.show(notes);
        l.set_open_command(console, 5, None);
        l.set_open_command(DockPanel::Workspace, 7, None);
        let session = l.to_session();
        let saved: Vec<(String, Option<i32>)> = session
            .groups
            .iter()
            .flat_map(|g| g.panels.iter().map(|p| (p.kind.clone(), p.cmd)))
            .collect();
        assert!(saved.contains(&(console.persist_key().to_string(), Some(5))));
        assert!(saved.contains(&(notes.persist_key().to_string(), None)));
        assert!(
            !saved.iter().any(|(k, c)| k == "workspace" && c.is_some()),
            "a host panel has no plugin command to record"
        );
        let back = DockLayout::from_session(&session);
        assert_eq!(back.open_command_for(console), Some(5));
        assert_eq!(back.open_command_for(notes), None);

        // A hand-edited negative index names no FuncItem.
        let mut edited = session.clone();
        for g in &mut edited.groups {
            for p in &mut g.panels {
                if p.cmd.is_some() {
                    p.cmd = Some(-3);
                }
            }
        }
        assert_eq!(
            DockLayout::from_session(&edited).open_command_for(console),
            None
        );
    }

    /// A command's seal survives `session.xml` beside it, and a seal the
    /// file spells any other way than 64 lowercase hex digits is read as
    /// no seal — which the restore treats as unsigned — rather than
    /// failing the session.
    #[test]
    fn a_commands_seal_round_trips_and_a_malformed_one_reads_as_none() {
        let console = intern_plugin_panel("seal-a.dll", "Console").expect("intern");
        let notes = intern_plugin_panel("seal-b.dll", "Notes").expect("intern");
        let seal = CommandSeal::from_bytes(std::array::from_fn(|i| u8::try_from(i * 7).unwrap()));
        let mut l = DockLayout::new();
        l.show(console);
        l.show(notes);
        l.set_open_command(console, 5, Some(seal));
        l.set_open_command(notes, 2, None);
        let session = l.to_session();
        let saved = |kind: &str| {
            session
                .groups
                .iter()
                .flat_map(|g| &g.panels)
                .find(|p| p.kind == kind)
                .expect("saved")
                .clone()
        };
        assert_eq!(saved(console.persist_key()).seal, Some(seal.to_hex()));
        assert_eq!(
            saved(notes.persist_key()).seal,
            None,
            "unsigned stays unsigned"
        );
        let back = DockLayout::from_session(&session);
        assert_eq!(back.open_command(console), Some((5, Some(seal))));
        assert_eq!(back.open_command(notes), Some((2, None)));

        for bad in [
            seal.to_hex().to_uppercase(),
            seal.to_hex()[..62].to_string(),
            format!("{}0", seal.to_hex()),
            format!("{}zz", &seal.to_hex()[..62]),
            String::new(),
        ] {
            let mut edited = session.clone();
            for p in edited.groups.iter_mut().flat_map(|g| &mut g.panels) {
                if p.seal.is_some() {
                    p.seal = Some(bad.clone());
                }
            }
            assert_eq!(
                DockLayout::from_session(&edited).open_command(console),
                Some((5, None)),
                "{bad:?} is not a seal"
            );
        }
        // A seal with no command beside it is ignored.
        let mut orphan = session.clone();
        for p in orphan.groups.iter_mut().flat_map(|g| &mut g.panels) {
            p.cmd = None;
        }
        assert_eq!(
            DockLayout::from_session(&orphan).open_command(console),
            None
        );
    }

    /// A seal's text is its bytes in order, and reads back to them.
    #[test]
    fn a_seal_is_its_bytes_in_lowercase_hex() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xab;
        bytes[1] = 0x01;
        bytes[31] = 0xff;
        let seal = CommandSeal::from_bytes(bytes);
        let hex = seal.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("ab01"));
        assert!(hex.ends_with("ff"));
        assert_eq!(CommandSeal::from_hex(&hex), Some(seal));
        assert!(seal.matches(&CommandSeal::from_bytes(bytes)));
        for i in 0..32 {
            let mut other = bytes;
            other[i] ^= 0x80;
            assert!(!seal.matches(&CommandSeal::from_bytes(other)), "byte {i}");
        }
    }

    /// The message a seal signs is exactly the label, the panel's key and
    /// the index — pinned, because a change to it silently invalidates
    /// every seal already saved — and no two records share one. A host
    /// panel has none.
    #[test]
    fn the_signed_message_names_the_panel_and_the_command() {
        let console = intern_plugin_panel("msg-a.dll", "Console").expect("intern");
        let other = intern_plugin_panel("msg-b.dll", "Console").expect("intern");
        assert_eq!(
            restore_command_message(console, 12).expect("a plugin panel"),
            b"Code++ plugin panel startup command v1\0plugin:msg-a.dll|Console\x0012".to_vec()
        );
        assert_ne!(
            restore_command_message(console, 1),
            restore_command_message(console, 12)
        );
        assert_ne!(
            restore_command_message(console, 1),
            restore_command_message(other, 1)
        );
        assert_eq!(restore_command_message(DockPanel::Workspace, 1), None);
        assert_eq!(restore_command_message(DockPanel::DocMap, 1), None);
    }

    /// Restoring panels shows each through its plugin's own command,
    /// which brings it to the front; the tabs the user left in front
    /// go back in front afterwards — but only where the arrangement
    /// the record describes still stands.
    #[test]
    fn restore_fronts_puts_back_each_groups_front_tab() {
        let a = intern_plugin_panel("front-a.dll", "A").expect("intern");
        let b = intern_plugin_panel("front-b.dll", "B").expect("intern");
        let c = intern_plugin_panel("front-c.dll", "C").expect("intern");
        let mut l = DockLayout::new();
        for p in [a, b] {
            l.set_initial_side(p, DockSide::Bottom);
            l.show(p);
        }
        l.set_initial_side(c, DockSide::Left);
        l.show(c);
        l.activate(a);
        let fronts = l.fronts();
        let bottom = l.group_of(a).expect("a is docked").id;
        assert!(fronts.contains(&(bottom, a)));

        // The restore's show commands bring the last-shown tab forward.
        l.show(a);
        l.show(b);
        assert_eq!(l.group_of(a).expect("docked").active_panel(), b);
        assert!(l.restore_fronts(&fronts));
        assert_eq!(l.group_of(a).expect("docked").active_panel(), a);
        assert!(!l.restore_fronts(&fronts), "nothing left to change");

        // A panel that has since moved to another group is not made
        // that group's front on the old record's say-so...
        let left = l.group_of(c).expect("c is docked").id;
        l.move_panel(a, DropTarget::IntoGroup(left));
        l.activate(c);
        assert!(!l.restore_fronts(&fronts));
        assert_eq!(l.group_of(c).expect("docked").active_panel(), c);
        assert_eq!(l.group_of(b).expect("docked").active_panel(), b);
        // ...and a group that has gone is skipped.
        l.hide(a);
        l.hide(c);
        assert!(!l.restore_fronts(&fronts));
    }

    /// What a restore brings back: the plugin panels that are open, in
    /// group then tab order — a panel with no recorded command included,
    /// since whether it has one is decided later — and never a hidden
    /// panel, which Notepad++ does not restore either, nor one of the
    /// host's own.
    #[test]
    fn open_plugin_panels_lists_only_open_plugin_panels() {
        let console = intern_plugin_panel("rst-a.dll", "A").expect("intern");
        let notes = intern_plugin_panel("rst-b.dll", "B").expect("intern");
        let closed = intern_plugin_panel("rst-c.dll", "C").expect("intern");
        let unrecorded = intern_plugin_panel("rst-d.dll", "D").expect("intern");
        let mut layout = DockLayout::new();
        for (panel, side) in [
            (console, DockSide::Bottom),
            (notes, DockSide::Bottom),
            (closed, DockSide::Left),
        ] {
            layout.set_initial_side(panel, side);
            layout.show(panel);
        }
        layout.show(unrecorded);
        layout.show(DockPanel::DocMap);
        layout.set_open_command(console, 1, None);
        layout.set_open_command(notes, 3, None);
        layout.set_open_command(closed, 0, None);
        layout.hide(closed);
        let open = layout.open_plugin_panels();
        assert!(open.contains(&unrecorded));
        assert!(!open.contains(&closed), "a hidden panel is not restored");
        assert!(
            !open.contains(&DockPanel::DocMap),
            "a host panel has no plugin command"
        );
        let recorded: Vec<DockPanel> = open.into_iter().filter(|p| *p != unrecorded).collect();
        assert_eq!(recorded, vec![console, notes], "group then tab order");

        // Dropping plugin panels (the backends that cannot host them)
        // forgets their commands too.
        layout.drop_plugin_panels();
        assert!(layout.open_plugin_panels().is_empty());
        assert_eq!(layout.open_command_for(console), None);
    }

    /// Six plugin panels, placed each way a parked panel has to be put
    /// back from: in front of a host panel, both tabs of a group of
    /// their own stacked under another, behind a host panel, alone in
    /// a docked band, and alone in a float. Each has an open command
    /// but one, so the commands' survival is checked too.
    fn parking_fixture() -> (DockSession, [DockPanel; 6]) {
        let panels = [
            intern_plugin_panel("park-1.dll", "One").expect("intern"),
            intern_plugin_panel("park-2.dll", "Two").expect("intern"),
            intern_plugin_panel("park-3.dll", "Three").expect("intern"),
            intern_plugin_panel("park-4.dll", "Four").expect("intern"),
            intern_plugin_panel("park-5.dll", "Five").expect("intern"),
            intern_plugin_panel("park-6.dll", "Six").expect("intern"),
        ];
        let entry = |kind: &str, cmd: Option<i32>| DockPanelSession {
            kind: kind.into(),
            cmd,
            seal: None,
        };
        let key = |i: usize| panels[i].persist_key();
        let group = |side: &str, active: usize, panels: Vec<DockPanelSession>| DockGroupSession {
            side: side.into(),
            active,
            panels,
            ..DockGroupSession::default()
        };
        let session = DockSession {
            left: Some(260),
            right: Some(180),
            top: Some(160),
            bottom: Some(200),
            groups: vec![
                group(
                    "left",
                    1,
                    vec![entry("workspace", None), entry(key(0), Some(1))],
                ),
                group("left", 0, vec![entry(key(1), Some(3)), entry(key(2), None)]),
                group(
                    "right",
                    0,
                    vec![entry("docmap", None), entry(key(3), Some(2))],
                ),
                group("bottom", 0, vec![entry(key(4), Some(5))]),
                DockGroupSession {
                    x: Some(40),
                    y: Some(50),
                    w: Some(300),
                    h: Some(220),
                    ..group("float", 0, vec![entry(key(5), Some(0))])
                },
            ],
            remembered: Vec::new(),
        };
        (session, panels)
    }

    /// A panel whose plugin cannot supply it is parked, and a session
    /// that touches nothing else saves exactly the layout it loaded —
    /// whichever panels were parked, in whichever order. That is what
    /// Notepad++ does with a panel it saved open whose plugin has gone
    /// (measured against 8.9.6): nothing shown, and the saved record
    /// written back as it was. Putting them all back gives the loaded
    /// layout again.
    #[test]
    fn parking_and_touching_nothing_saves_the_layout_that_was_loaded() {
        let (session, panels) = parking_fixture();
        let loaded = DockLayout::from_session(&session);
        let baseline = loaded.to_session();
        assert_eq!(
            loaded.open_plugin_panels(),
            panels,
            "precondition: all six open"
        );
        for mask in 1u32..(1 << panels.len()) {
            let subset: Vec<DockPanel> = (0..panels.len())
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| panels[i])
                .collect();
            let reversed: Vec<DockPanel> = subset.iter().rev().copied().collect();
            for order in [subset, reversed] {
                let mut layout = loaded.clone();
                assert!(layout.park(&order));
                for &panel in &order {
                    assert!(!layout.is_visible(panel), "{panel:?} still presented");
                    assert!(layout.is_parked(panel));
                }
                assert_eq!(layout.to_session(), baseline, "parked {order:?}");
                assert!(layout.unpark(&order));
                assert!(layout.parked_panels().is_empty());
                assert_eq!(
                    layout.to_session(),
                    baseline,
                    "parked, then put back {order:?}"
                );
            }
        }
    }

    /// Five plugin panels placed where the order they come back in
    /// matters: three tabs of one group with the *last* in front, then,
    /// stacked under it on the same side, a group of one, a host
    /// panel's group and another group of one — so a group that had
    /// gone has to come back to its own place in the stack.
    fn stacked_fixture() -> (DockSession, Vec<DockPanel>) {
        let panels: Vec<DockPanel> = ["One", "Two", "Three", "Four", "Five"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                intern_plugin_panel(&format!("stack-{}.dll", i + 1), name).expect("intern")
            })
            .collect();
        let entry = |kind: &str| DockPanelSession {
            kind: kind.into(),
            cmd: None,
            seal: None,
        };
        let group = |active: usize, kinds: Vec<&str>| DockGroupSession {
            side: "left".into(),
            active,
            panels: kinds.into_iter().map(entry).collect(),
            ..DockGroupSession::default()
        };
        let key = |i: usize| panels[i].persist_key();
        let session = DockSession {
            left: Some(260),
            groups: vec![
                group(2, vec![key(0), key(1), key(2)]),
                group(0, vec![key(3)]),
                group(0, vec!["workspace"]),
                group(0, vec![key(4)]),
            ],
            ..DockSession::default()
        };
        (session, panels)
    }

    /// Every ordering of `items`.
    fn permutations(items: &[DockPanel]) -> Vec<Vec<DockPanel>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut all = Vec::new();
        for (i, &first) in items.iter().enumerate() {
            let mut rest = items.to_vec();
            rest.remove(i);
            for tail in permutations(&rest) {
                let mut ordering = vec![first];
                ordering.extend(tail);
                all.push(ordering);
            }
        }
        all
    }

    /// `after` is `before` with `added` put in and nothing else moved:
    /// the same groups in the same order under the same ids, each with
    /// its tabs in the same order and the same tab in front — unless an
    /// added panel is owed the front, being its group's front in
    /// `whole`, the layout with nothing parked. Then it must have it.
    fn assert_only_added(
        before: &DockLayout,
        after: &DockLayout,
        added: &[DockPanel],
        whole: &DockLayout,
        context: &str,
    ) {
        let before_ids: Vec<u32> = before.groups().iter().map(|g| g.id).collect();
        let kept: Vec<u32> = after
            .groups()
            .iter()
            .map(|g| g.id)
            .filter(|id| before_ids.contains(id))
            .collect();
        assert_eq!(
            kept, before_ids,
            "{context}: a group on screen moved or went"
        );
        for old in before.groups() {
            let new = after.group(old.id).expect("kept, checked above");
            let tabs: Vec<DockPanel> = new
                .panels
                .iter()
                .copied()
                .filter(|p| !added.contains(p))
                .collect();
            assert_eq!(
                tabs, old.panels,
                "{context}: tabs of group {} moved",
                old.id
            );
            let owed = new
                .panels
                .iter()
                .copied()
                .find(|p| added.contains(p) && whole.is_active(*p));
            assert_eq!(
                new.active_panel(),
                owed.unwrap_or(old.active_panel()),
                "{context}: front of group {}",
                old.id
            );
        }
        for group in after.groups() {
            for panel in &group.panels {
                assert!(
                    before.is_visible(*panel) || added.contains(panel),
                    "{context}: {panel:?} appeared"
                );
            }
        }
        for panel in added {
            assert!(
                after.is_visible(*panel),
                "{context}: {panel:?} did not come back"
            );
        }
    }

    /// Panels parked together come back to their old arrangement
    /// whichever order they come back in — one at a time, as when the
    /// plugins that own them are re-enabled on different days — and
    /// each one coming back changes nothing on screen but adding it.
    /// The first version put each back relative to the group it found,
    /// still missing the panels parked before it, and the review of it
    /// reproduced the result: two tabs of one group came back reversed,
    /// and a group whose last tab was in front came back with its first
    /// in front. A group that had gone could come back in another's
    /// place in the stack the same way.
    #[test]
    fn unparking_one_at_a_time_in_any_order_puts_back_what_was_parked() {
        let (session, panels) = parking_fixture();
        for (session, panels) in [(session, panels.to_vec()), stacked_fixture()] {
            let loaded = DockLayout::from_session(&session);
            let baseline = loaded.to_session();
            for mask in 1u32..(1 << panels.len()) {
                let subset: Vec<DockPanel> = (0..panels.len())
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| panels[i])
                    .collect();
                let reversed: Vec<DockPanel> = subset.iter().rev().copied().collect();
                for parked in [subset.clone(), reversed] {
                    for back in permutations(&subset) {
                        let mut layout = loaded.clone();
                        layout.park(&parked);
                        for &panel in &back {
                            let context =
                                format!("parked {parked:?}, put back {back:?}, now {panel:?}");
                            let before = layout.clone();
                            assert!(layout.unpark(&[panel]), "{context}");
                            assert_only_added(&before, &layout, &[panel], &loaded, &context);
                            assert_eq!(layout.to_session(), baseline, "{context}");
                        }
                        assert!(layout.parked_panels().is_empty());
                    }
                }
            }
        }
    }

    /// The cases the exhaustive test above covers, spelled out: panels
    /// parked together and put back one at a time in the order they
    /// were parked.
    #[test]
    fn panels_put_back_one_at_a_time_keep_their_order_and_front() {
        // Two tabs of one group, the first in front.
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&[panels[1], panels[2]]);
        layout.unpark(&[panels[1]]);
        layout.unpark(&[panels[2]]);
        let group = layout.group_of(panels[1]).expect("back");
        assert_eq!(group.panels, vec![panels[1], panels[2]]);
        assert_eq!(group.active_panel(), panels[1]);

        // Three tabs with the last in front, and two groups of one on
        // the same side around a host panel's group.
        let (session, stacked) = stacked_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&stacked);
        for &panel in &stacked {
            layout.unpark(&[panel]);
        }
        let group = layout.group_of(stacked[0]).expect("back");
        assert_eq!(group.panels, stacked[..3]);
        assert_eq!(
            group.active_panel(),
            stacked[2],
            "the last tab in front again"
        );
        let left: Vec<Vec<DockPanel>> = layout
            .groups_on(DockSide::Left)
            .map(|g| g.panels.clone())
            .collect();
        assert_eq!(
            left,
            vec![
                stacked[..3].to_vec(),
                vec![stacked[3]],
                vec![DockPanel::Workspace],
                vec![stacked[4]],
            ]
        );
    }

    /// Closing or moving a parked panel works on it where it belongs
    /// and leaves the panels parked around it where they belong: what
    /// is saved is exactly what the same close or move would have made
    /// with nothing parked. The first version dropped the panel's
    /// record instead, which left the records parked before it
    /// describing a group that still held it.
    #[test]
    fn closing_or_moving_a_parked_panel_leaves_the_rest_where_they_belong() {
        let (session, stacked) = stacked_fixture();
        let loaded = DockLayout::from_session(&session);
        let reversed: Vec<DockPanel> = stacked.iter().rev().copied().collect();
        for parked in [stacked.clone(), reversed] {
            for &panel in &stacked {
                let mut expected = loaded.clone();
                expected.hide(panel);
                let mut layout = loaded.clone();
                layout.park(&parked);
                layout.hide(panel);
                assert!(!layout.is_parked(panel) && !layout.is_visible(panel));
                assert_eq!(
                    layout.to_session(),
                    expected.to_session(),
                    "closed {panel:?}, parked {parked:?}"
                );

                let mut expected = loaded.clone();
                expected.move_panel(panel, DropTarget::Side(DockSide::Top));
                let mut layout = loaded.clone();
                layout.park(&parked);
                layout.move_panel(panel, DropTarget::Side(DockSide::Top));
                assert!(!layout.is_parked(panel) && layout.is_visible(panel));
                assert_eq!(
                    layout.to_session(),
                    expected.to_session(),
                    "moved {panel:?}, parked {parked:?}"
                );
            }
        }
        // A drop onto a group that does not exist moves nothing, parked
        // or not: the panel stays parked and nothing else changes.
        let mut layout = loaded.clone();
        layout.park(&stacked);
        let parked = layout.clone();
        layout.move_panel(stacked[1], DropTarget::IntoGroup(u32::MAX));
        assert!(layout.is_parked(stacked[1]));
        assert_eq!(layout.to_session(), parked.to_session());
        assert_eq!(layout.parked_panels(), parked.parked_panels());
    }

    /// Nothing presents a parked panel: a band it had to itself is not
    /// carved, a float it had to itself is gone, a group it shared
    /// shows the rest, and it is not among the panels a load pass
    /// restores or the fronts it puts back.
    #[test]
    fn a_parked_panel_is_not_presented() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        let carved = |l: &DockLayout| -> Vec<DockSide> {
            compute_frame(mid(), l, 200, 100)
                .bands
                .iter()
                .map(|b| b.side)
                .collect()
        };
        assert!(carved(&layout).contains(&DockSide::Bottom));
        assert!(layout.park(&[panels[3], panels[4], panels[5]]));
        assert!(!carved(&layout).contains(&DockSide::Bottom));
        assert_eq!(layout.floating_groups().count(), 0);
        assert_eq!(
            layout.group_of(DockPanel::DocMap).map(|g| g.panels.clone()),
            Some(vec![DockPanel::DocMap])
        );
        assert_eq!(
            layout.open_plugin_panels(),
            [panels[0], panels[1], panels[2]]
        );
        assert!(!layout.fronts().iter().any(|(_, p)| layout.is_parked(*p)));
        assert_eq!(layout.parked_panels(), [panels[3], panels[4], panels[5]]);
        // Parking what is not open, or a host panel, does nothing.
        assert!(!layout.park(&[panels[3], DockPanel::DocMap]));
    }

    /// `show` brings a parked panel back where it was and to the front,
    /// which is what registering it with its plugin loaded means.
    #[test]
    fn show_brings_a_parked_panel_back_where_it_was() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&[panels[3]]);
        layout.show(panels[3]);
        assert!(!layout.is_parked(panels[3]));
        let group = layout.group_of(panels[3]).expect("back in a group");
        assert_eq!(group.panels, vec![DockPanel::DocMap, panels[3]]);
        assert_eq!(group.active_panel(), panels[3], "a show puts it in front");
        assert_eq!(group.location, DockLocation::Side(DockSide::Right));
    }

    /// Closing a parked panel closes it for good: it is remembered
    /// where it was, and no longer written back as open.
    #[test]
    fn hiding_a_parked_panel_closes_it() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&[panels[4]]);
        layout.hide(panels[4]);
        assert!(!layout.is_parked(panels[4]));
        assert!(!layout.is_visible(panels[4]));
        let saved = layout.to_session();
        assert!(!saved
            .groups
            .iter()
            .any(|g| g.panels.iter().any(|p| p.kind == panels[4].persist_key())));
        assert!(saved
            .remembered
            .iter()
            .any(|r| r.kind == panels[4].persist_key() && r.side == "bottom"));
    }

    /// The layout can change around a parked panel, and what is saved
    /// follows it: a panel parked from a group the user then moves is
    /// written into the group where it now is, and one whose front the
    /// user has since changed does not take the front back.
    #[test]
    fn a_parked_panel_is_saved_into_the_layout_as_it_now_is() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&[panels[0], panels[3]]);
        // The docmap's group moves from the right to the top.
        let docmap = layout.group_of(DockPanel::DocMap).expect("docked").id;
        layout.move_group(docmap, DropTarget::Side(DockSide::Top));
        let saved = layout.to_session();
        let top = saved
            .groups
            .iter()
            .find(|g| g.side == "top")
            .expect("a top group");
        let kinds: Vec<&str> = top.panels.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(kinds, ["docmap", panels[3].persist_key()]);

        // panels[0] was the front of the workspace group. The user
        // brings another tab forward there — the docmap, dragged in —
        // and the parked panel does not take the front back from their
        // choice. The docmap leaving the top group dissolves that
        // group, and panels[3] keeps the place it last had: the top.
        let workspace = layout.group_of(DockPanel::Workspace).expect("docked").id;
        layout.move_panel(DockPanel::DocMap, DropTarget::IntoGroup(workspace));
        let saved = layout.to_session();
        let top = saved
            .groups
            .iter()
            .find(|g| g.side == "top")
            .expect("panels[3] still saved on the top");
        let kinds: Vec<&str> = top.panels.iter().map(|p| p.kind.as_str()).collect();
        assert_eq!(kinds, [panels[3].persist_key()]);
        let left = saved
            .groups
            .iter()
            .find(|g| g.panels.iter().any(|p| p.kind == "workspace"))
            .expect("the workspace group");
        assert_eq!(left.panels[left.active].kind, "docmap");
        assert!(left
            .panels
            .iter()
            .any(|p| p.kind == panels[0].persist_key()));
    }

    /// A parked panel from a group that is dragged onto another goes
    /// with its group's tabs into the one it joined.
    #[test]
    fn a_parked_panel_follows_its_group_into_a_merge() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&[panels[3]]);
        let right = layout.group_of(DockPanel::DocMap).expect("docked").id;
        let workspace = layout.group_of(DockPanel::Workspace).expect("docked").id;
        layout.move_group(right, DropTarget::IntoGroup(workspace));
        let saved = layout.to_session();
        let joined = saved
            .groups
            .iter()
            .find(|g| g.panels.iter().any(|p| p.kind == "workspace"))
            .expect("the workspace group");
        assert!(joined
            .panels
            .iter()
            .any(|p| p.kind == panels[3].persist_key()));
        assert!(!saved.groups.iter().any(|g| g.side == "right"));
    }

    /// A group all of whose panels were parked is recreated when one
    /// comes back — under a fresh id, not the one it had — and the rest
    /// follow it into that group rather than starting others.
    #[test]
    fn unparking_recreates_a_gone_group_and_the_rest_follow_it() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        let old_id = layout.group_of(panels[1]).expect("docked").id;
        layout.park(&[panels[1], panels[2]]);
        assert_eq!(layout.groups_on(DockSide::Left).count(), 1);
        assert!(layout.unpark(&[panels[2]]));
        let new_id = layout.group_of(panels[2]).expect("back").id;
        assert!(new_id > old_id, "a fresh id, not the old one reused");
        assert!(layout.unpark(&[panels[1]]));
        let group = layout.group_of(panels[1]).expect("back");
        assert_eq!(group.id, new_id, "followed its old group-mate");
        assert_eq!(group.panels, vec![panels[1], panels[2]]);
        assert_eq!(group.active_panel(), panels[1]);
        assert_eq!(layout.groups_on(DockSide::Left).count(), 2);
        assert!(!layout.unpark(&[panels[1]]), "nothing left to put back");
    }

    /// Putting a panel back spends a group id only on a group that
    /// stays. Every other parked panel is put back for the length of
    /// the call, and a group of theirs that had gone comes back just as
    /// briefly, under the id its records name. The first version gave
    /// each such group a fresh id on every call — up to one per parked
    /// panel — which the security audit flagged as spending the id
    /// counter many times faster than the call needed.
    #[test]
    fn putting_a_panel_back_spends_ids_only_on_groups_that_stay() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&panels);
        let before = layout.next_id;
        // Back into the docmap's group, which never went: no id, though
        // three groups that had gone came back for the call.
        assert!(layout.unpark(&[panels[3]]));
        assert_eq!(layout.next_id, before);
        // Its own group had gone, and comes back to stay: one id.
        assert!(layout.unpark(&[panels[4]]));
        assert_eq!(layout.next_id, before + 1);
    }

    /// Group ids wrap past `u32::MAX`, so the counter can come round to
    /// an id still in use. It skips one: a new group never shares an id
    /// with a live group, or with the group a parked panel's record
    /// names, which is where that panel comes back — a shared id there
    /// would put the parked panel into the stranger's group.
    #[test]
    fn a_wrapped_group_id_skips_ids_still_in_use() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        let first = intern_plugin_panel("wrap.dll", "First").expect("intern");
        let second = intern_plugin_panel("wrap.dll", "Second").expect("intern");
        // panels[4] is alone at the bottom, so parking it leaves its
        // group's id named only by the parked record.
        let named = layout.group_of(panels[4]).expect("docked").id;
        layout.park(&[panels[4]]);
        let live = layout.group_of(DockPanel::Workspace).expect("docked").id;

        // The counter comes round to the id the parked record names...
        layout.next_id = named;
        layout.show(first);
        assert_ne!(layout.group_of(first).expect("shown").id, named);
        // ...and to a live group's.
        layout.next_id = live;
        layout.show(second);
        assert_ne!(layout.group_of(second).expect("shown").id, live);

        let mut ids: Vec<u32> = layout.groups().iter().map(|g| g.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two groups share an id");
        // And the parked panel still comes back alone, where it was.
        assert!(layout.unpark(&[panels[4]]));
        let back = layout.group_of(panels[4]).expect("back");
        assert_eq!(back.panels, vec![panels[4]]);
        assert_eq!(back.location, DockLocation::Side(DockSide::Bottom));
    }

    /// Everything that forgets plugin panels, or places one explicitly,
    /// also lets go of a parked record — and parked float rects are
    /// clamped back into reach like every other one.
    #[test]
    fn a_parked_record_is_dropped_moved_and_clamped_with_the_rest() {
        let (session, panels) = parking_fixture();
        let mut layout = DockLayout::from_session(&session);
        layout.park(&[panels[4], panels[5]]);
        layout.move_panel(panels[4], DropTarget::Side(DockSide::Top));
        assert!(!layout.is_parked(panels[4]));
        assert!(layout.is_visible(panels[4]));
        let area = DockRect::new(1000, 1000, 400, 300);
        layout.clamp_floating_to_area(area);
        let float = layout
            .to_session()
            .groups
            .into_iter()
            .find(|g| g.side == "float")
            .expect("the parked float is still saved");
        let rect = DockRect::new(
            float.x.unwrap_or(0),
            float.y.unwrap_or(0),
            float.w.unwrap_or(0),
            float.h.unwrap_or(0),
        );
        assert_eq!(
            rect,
            clamp_float_into(DockRect::new(40, 50, 300, 220), area),
            "clamped the way every floating rect is"
        );
        layout.drop_plugin_panels();
        assert!(layout.parked_panels().is_empty());
    }

    /// A plugin panel's name is what its caption and tab draw, so text
    /// the display policy rejects is refused where identities are made.
    /// Registration sanitizes first; a `session.xml` key does not, and
    /// this is the only thing standing between a hand-edited one and a
    /// bidi override in the chrome.
    #[test]
    fn a_display_hostile_plugin_identity_is_refused() {
        assert_eq!(intern_plugin_panel("spoof.dll", "Inv\u{202E}exe.pdf"), None);
        assert_eq!(intern_plugin_panel("spoof\u{0}.dll", "Name"), None);
        assert_eq!(intern_plugin_panel("spoof.dll", "Two\nLines"), None);
        assert_eq!(
            DockPanel::from_persist_key("plugin:spoof.dll|Inv\u{202E}exe.pdf"),
            None
        );
        // What registration substitutes for such a character passes.
        assert!(intern_plugin_panel("spoof.dll", "Inv\u{FFFD}exe.pdf").is_some());

        // A session naming one restores without it, and keeps the rest.
        let session = DockSession {
            groups: vec![DockGroupSession {
                side: "bottom".into(),
                panels: vec![
                    DockPanelSession {
                        kind: "plugin:spoof.dll|Inv\u{202E}exe.pdf".into(),
                        cmd: Some(1),
                        seal: None,
                    },
                    DockPanelSession {
                        kind: "docmap".into(),
                        cmd: None,
                        seal: None,
                    },
                ],
                ..DockGroupSession::default()
            }],
            ..DockSession::default()
        };
        let restored = DockLayout::from_session(&session);
        assert!(restored.open_plugin_panels().is_empty());
        assert!(restored.is_visible(DockPanel::DocMap));
    }

    /// Persisted text cannot fill the panel table: past the bound a key
    /// it names is not interned at all, so registration keeps room — and
    /// the bound is on the process, so a second read of another session
    /// finds it spent. Driven through a budget of its own with a bound of
    /// two, because the table is process-wide.
    #[test]
    fn a_restored_session_cannot_fill_the_panel_table() {
        let interned = |module: &str| {
            PLUGIN_PANELS
                .lock()
                .expect("table")
                .iter()
                .any(|p| p.module == module)
        };
        let group_panel = |kind: &str| DockPanelSession {
            kind: kind.into(),
            cmd: None,
            seal: None,
        };
        let session = DockSession {
            groups: vec![DockGroupSession {
                side: "bottom".into(),
                panels: vec![
                    group_panel("plugin:cap-a.dll|A"),
                    group_panel("plugin:cap-b.dll|B"),
                    group_panel("plugin:cap-c.dll|C"),
                    group_panel("docmap"),
                ],
                ..DockGroupSession::default()
            }],
            remembered: vec![
                DockRememberSession {
                    kind: "plugin:cap-a.dll|A".into(),
                    side: "left".into(),
                    ..DockRememberSession::default()
                },
                DockRememberSession {
                    kind: "plugin:cap-d.dll|D".into(),
                    side: "left".into(),
                    ..DockRememberSession::default()
                },
            ],
            ..DockSession::default()
        };
        let budget = AtomicUsize::new(0);
        let restored = DockLayout::from_session_budgeted(&session, &budget, 2);
        let names: Vec<&str> = restored
            .open_plugin_panels()
            .iter()
            .map(|p| p.title())
            .collect();
        assert_eq!(names, vec!["A", "B"], "groups are admitted first, in order");
        assert!(
            restored.is_visible(DockPanel::DocMap),
            "host panels are not counted"
        );
        assert!(
            !interned("cap-c.dll"),
            "a key past the bound must not be interned"
        );
        assert!(!interned("cap-d.dll"), "nor a remembered one");

        // A later read — of a different session — finds the budget spent:
        // a new panel is refused, while one already in the table still
        // resolves, because it takes no slot.
        let later = DockSession {
            groups: vec![DockGroupSession {
                side: "left".into(),
                panels: vec![
                    group_panel("plugin:cap-e.dll|E"),
                    group_panel("plugin:cap-a.dll|A"),
                ],
                ..DockGroupSession::default()
            }],
            ..DockSession::default()
        };
        let again = DockLayout::from_session_budgeted(&later, &budget, 2);
        let names: Vec<&str> = again
            .open_plugin_panels()
            .iter()
            .map(|p| p.title())
            .collect();
        assert_eq!(names, vec!["A"]);
        assert!(
            !interned("cap-e.dll"),
            "the bound is on the process, not one read"
        );
    }

    /// `container_of` answers for a hidden panel with where `show`
    /// will actually put it. A host tells a plugin its container at
    /// registration from this, before anything is on screen, so if the
    /// two ever disagree the plugin is told one place and lands in
    /// another. Every branch of `show`'s precedence is checked against
    /// the real `show`.
    #[test]
    fn container_of_a_hidden_panel_predicts_where_show_puts_it() {
        let fresh = intern_plugin_panel("ctr-fresh.dll", "Fresh").expect("intern");
        let sided = intern_plugin_panel("ctr-sided.dll", "Sided").expect("intern");
        let docked = intern_plugin_panel("ctr-docked.dll", "Docked").expect("intern");
        let floated = intern_plugin_panel("ctr-float.dll", "Floated").expect("intern");
        let mut l = DockLayout::new();
        l.set_initial_side(sided, DockSide::Top);
        l.show(docked);
        l.move_panel(docked, DropTarget::Side(DockSide::Right));
        l.hide(docked);
        l.show(floated);
        l.move_panel(
            floated,
            DropTarget::Floating(DockRect::new(10, 10, 300, 200)),
        );
        l.hide(floated);
        // Parked: one whose group is still on screen, beside a mate,
        // and one whose float went with it.
        let mate = intern_plugin_panel("ctr-mate.dll", "Mate").expect("intern");
        let parked_beside = intern_plugin_panel("ctr-parked.dll", "Beside").expect("intern");
        let parked_alone = intern_plugin_panel("ctr-parked.dll", "Alone").expect("intern");
        l.set_initial_side(mate, DockSide::Left);
        l.show(mate);
        l.set_initial_side(parked_beside, DockSide::Left);
        l.show(parked_beside);
        l.show(parked_alone);
        l.move_panel(
            parked_alone,
            DropTarget::Floating(DockRect::new(30, 30, 250, 180)),
        );
        l.park(&[parked_beside, parked_alone]);
        assert_eq!(
            l.container_of(parked_beside),
            DockContainer::Docked(DockSide::Left)
        );
        assert_eq!(l.container_of(parked_alone), DockContainer::Floating(None));

        for panel in [
            fresh,
            sided,
            docked,
            floated,
            parked_beside,
            parked_alone,
            DockPanel::Workspace,
            DockPanel::DocMap,
        ] {
            assert!(!l.is_visible(panel), "precondition: {panel:?} hidden");
            let predicted = l.container_of(panel);
            l.show(panel);
            let actual = l.container_of(panel);
            assert!(
                predicted.is_same(actual),
                "{panel:?}: predicted {predicted:?}, show put it in {actual:?}"
            );
        }
        // And the predictions were the specific ones each branch
        // stands for, not all "bottom" by coincidence.
        assert_eq!(
            l.container_of(fresh),
            DockContainer::Docked(DockSide::Bottom)
        );
        assert_eq!(l.container_of(sided), DockContainer::Docked(DockSide::Top));
        assert_eq!(
            l.container_of(docked),
            DockContainer::Docked(DockSide::Right)
        );
        assert!(matches!(
            l.container_of(floated),
            DockContainer::Floating(Some(_))
        ));
    }

    /// The sameness rule is the whole policy for when `DMN_DOCK` /
    /// `DMN_FLOAT` fire: a spurious "true" loses a notification, a
    /// spurious "false" sends one for a panel that did not move.
    #[test]
    fn container_sameness_is_side_for_docked_and_group_for_floating() {
        use DockContainer::{Docked, Floating};
        assert!(Docked(DockSide::Left).is_same(Docked(DockSide::Left)));
        assert!(!Docked(DockSide::Left).is_same(Docked(DockSide::Bottom)));
        assert!(!Docked(DockSide::Left).is_same(Floating(Some(3))));
        assert!(!Floating(None).is_same(Docked(DockSide::Left)));
        assert!(Floating(Some(3)).is_same(Floating(Some(3))));
        assert!(!Floating(Some(3)).is_same(Floating(Some(4))));
        // A hidden floating panel is the same place as whichever group
        // it comes back in.
        assert!(Floating(None).is_same(Floating(Some(9))));
        assert!(Floating(Some(9)).is_same(Floating(None)));
    }

    /// The moves that are, and are not, a change of container.
    #[test]
    fn container_changes_track_upstream_container_edges() {
        let a = intern_plugin_panel("edge-a.dll", "Edge A").expect("intern");
        let b = intern_plugin_panel("edge-b.dll", "Edge B").expect("intern");
        let mut l = DockLayout::new();
        l.set_initial_side(a, DockSide::Bottom);
        l.set_initial_side(b, DockSide::Bottom);
        l.show(a);
        l.show(b);

        // Tearing a tab off into a second band on the *same* side is
        // not a container change — a side is one container.
        let before = l.container_of(a);
        l.move_panel(a, DropTarget::Side(DockSide::Bottom));
        assert!(before.is_same(l.container_of(a)));

        // Docked to floating is.
        let before = l.container_of(a);
        l.move_panel(a, DropTarget::Floating(DockRect::new(0, 0, 300, 200)));
        let floating = l.container_of(a);
        assert!(!before.is_same(floating));

        // Moving the floating group around is not.
        let gid = l.group_of(a).expect("A visible").id;
        l.set_floating_rect(gid, DockRect::new(50, 50, 300, 200));
        assert!(floating.is_same(l.container_of(a)));

        // Hiding and re-showing a floating panel is not, even though
        // it comes back in a group with a new id. Compared step by
        // step, the way a host does it — against the container it
        // last recorded, which while the panel is hidden is the
        // group-less `Floating(None)`. A *direct* comparison of the
        // old group with the new one would say "moved", and that is
        // correct for the case it describes: a panel dragged from one
        // floating window into another.
        l.hide(a);
        let hidden = l.container_of(a);
        assert_eq!(hidden, DockContainer::Floating(None));
        assert!(floating.is_same(hidden));
        l.show(a);
        assert_ne!(
            l.group_of(a).expect("A visible").id,
            gid,
            "precondition: new group"
        );
        assert!(hidden.is_same(l.container_of(a)));
        assert!(
            !floating.is_same(l.container_of(a)),
            "two live floating groups differ"
        );

        // Floating back into a docked group is.
        let bgid = l.group_of(b).expect("B visible").id;
        let before = l.container_of(a);
        l.move_panel(a, DropTarget::IntoGroup(bgid));
        assert!(!before.is_same(l.container_of(a)));
        assert_eq!(l.container_of(a), DockContainer::Docked(DockSide::Bottom));
    }

    #[test]
    fn floating_ordinal_counts_floating_groups_only() {
        let a = intern_plugin_panel("ord-a.dll", "Ord A").expect("intern");
        let b = intern_plugin_panel("ord-b.dll", "Ord B").expect("intern");
        let mut l = DockLayout::new();
        l.show(DockPanel::Workspace);
        l.show(a);
        l.show(b);
        l.move_panel(a, DropTarget::Floating(DockRect::new(0, 0, 300, 200)));
        l.move_panel(b, DropTarget::Floating(DockRect::new(40, 40, 300, 200)));
        let ga = l.group_of(a).expect("A").id;
        let gb = l.group_of(b).expect("B").id;
        let gw = l.group_of(DockPanel::Workspace).expect("W").id;
        assert_eq!(l.floating_ordinal(ga), Some(0));
        assert_eq!(l.floating_ordinal(gb), Some(1));
        assert_eq!(
            l.floating_ordinal(gw),
            None,
            "a docked group has no floating ordinal"
        );
    }

    /// Two plugins asking for the same `DWS_DF_CONT_*` side become
    /// tabs in one container, which is what upstream's docking
    /// manager does — it has exactly one container per side — and is
    /// what makes `NPPM_DMMVIEWOTHERTAB` mean anything: the message
    /// switches between tabs that share a container.
    #[test]
    fn two_registration_sides_share_one_container() {
        let a = intern_plugin_panel("a.dll", "Panel A").expect("intern A");
        let b = intern_plugin_panel("b.dll", "Panel B").expect("intern B");
        let mut l = DockLayout::new();
        l.set_initial_side(a, DockSide::Bottom);
        l.set_initial_side(b, DockSide::Bottom);
        l.show(a);
        l.show(b);

        let ga = l.group_of(a).expect("A visible");
        let gb = l.group_of(b).expect("B visible");
        assert_eq!(ga.id, gb.id, "both panels should share one group");
        assert_eq!(ga.panels, vec![a, b]);
        // The panel just shown is the visible tab...
        assert_eq!(ga.panels[ga.active], b);
        // ...and `NPPM_DMMVIEWOTHERTAB` brings the other one forward.
        l.activate(a);
        assert_eq!(
            l.group_of(a).expect("A visible").panels[l.group_of(a).unwrap().active],
            a
        );
    }

    /// The precedence that lets a plugin state a preference without
    /// overriding the user: a location the user's own arrangement
    /// recorded wins, and it opens a band of its own rather than
    /// joining whatever else happens to be on that side.
    #[test]
    fn a_remembered_location_beats_a_registration_side() {
        let a = intern_plugin_panel("pref-a.dll", "Pref A").expect("intern A");
        let b = intern_plugin_panel("pref-b.dll", "Pref B").expect("intern B");
        let mut l = DockLayout::new();
        l.set_initial_side(a, DockSide::Bottom);
        l.show(a);

        // The user dragged B to the bottom and closed it again, so
        // the bottom is remembered for B — even though B's
        // registration also asked for the bottom.
        l.set_initial_side(b, DockSide::Bottom);
        l.show(b);
        l.move_panel(b, DropTarget::Side(DockSide::Bottom));
        l.hide(b);
        assert!(!l.is_visible(b));

        l.show(b);
        assert_ne!(
            l.group_of(a).expect("A visible").id,
            l.group_of(b).expect("B visible").id,
            "a remembered Side means a band of its own, not the container there"
        );
    }

    /// The identity a `session.xml` key restores has to be the same
    /// one the plugin's own registration interns, or a restored
    /// layout and a live registration describe two panels that
    /// compare unequal and the group renders empty.
    #[test]
    fn a_plugin_panel_round_trips_through_its_persist_key() {
        let panel = intern_plugin_panel("rt.dll", "Round Trip").expect("intern");
        let key = panel.persist_key();
        assert_eq!(key, "plugin:rt.dll|Round Trip");
        assert_eq!(DockPanel::from_persist_key(key), Some(panel));
        // Interning the same pair again is the same identity, not a
        // second one that merely prints the same.
        assert_eq!(intern_plugin_panel("rt.dll", "Round Trip"), Some(panel));
        // The module is part of the identity: two plugins may ship a
        // panel with the same display name.
        let other = intern_plugin_panel("other.dll", "Round Trip").expect("intern other");
        assert_ne!(other, panel);
        assert_eq!(panel.title(), "Round Trip");
    }

    /// A key that is not a plugin key, or is a malformed one, must
    /// not intern anything — `session.xml` is hand-editable, and
    /// `MAX_PLUGIN_PANELS` is the only thing bounding the leak.
    #[test]
    fn a_malformed_plugin_key_interns_nothing() {
        assert_eq!(DockPanel::from_persist_key("plugin:no-separator"), None);
        assert_eq!(DockPanel::from_persist_key("plugin:"), None);
        assert_eq!(DockPanel::from_persist_key("nonsense"), None);
        // Either half empty.
        assert_eq!(DockPanel::from_persist_key("plugin:|name"), None);
        assert_eq!(DockPanel::from_persist_key("plugin:mod|"), None);
        // Either half over the field cap. The intern table is leaked
        // for the process's life and this key comes off disk, so an
        // over-long half is refused rather than truncated — two keys
        // that truncate alike would otherwise become one panel.
        let long = "x".repeat(MAX_PLUGIN_PANEL_FIELD_LEN + 1);
        assert_eq!(
            DockPanel::from_persist_key(&format!("plugin:{long}|name")),
            None
        );
        assert_eq!(
            DockPanel::from_persist_key(&format!("plugin:mod|{long}")),
            None
        );
        // At the cap exactly, it is accepted.
        let at_cap = "y".repeat(MAX_PLUGIN_PANEL_FIELD_LEN);
        assert!(DockPanel::from_persist_key(&format!("plugin:{at_cap}|n")).is_some());
        // A separator inside a half would re-split differently on the
        // way back, letting a plugin choose a title that collides
        // with another plugin's identity.
        assert_eq!(intern_plugin_panel("mod", "a|b"), None);
        assert_eq!(intern_plugin_panel("a|b", "name"), None);
        assert_eq!(
            DockPanel::from_persist_key("workspace"),
            Some(DockPanel::Workspace)
        );
    }

    /// The backends that cannot host a plugin panel drop them at
    /// restore, and a shared group must survive losing one tab
    /// rather than taking the whole group with it.
    #[test]
    fn dropping_plugin_panels_leaves_a_valid_layout() {
        let a = intern_plugin_panel("drop.dll", "Drop A").expect("intern A");
        let b = intern_plugin_panel("drop.dll", "Drop B").expect("intern B");
        let mut l = DockLayout::new();
        l.show(DockPanel::DocMap);
        l.set_initial_side(a, DockSide::Right);
        l.show(a);
        l.set_initial_side(b, DockSide::Bottom);
        l.show(b);
        l.hide(b);
        // Preconditions: A shares the docmap's group and is its
        // active tab; B is hidden but remembered.
        assert_eq!(
            l.group_of(a).expect("A visible").panels,
            vec![DockPanel::DocMap, a]
        );
        assert_eq!(l.group_of(a).unwrap().active, 1);
        assert!(!l.is_visible(b));

        l.drop_plugin_panels();

        assert!(!l.is_visible(a));
        assert_eq!(
            l.group_of(DockPanel::DocMap)
                .expect("docmap survives")
                .panels,
            vec![DockPanel::DocMap],
            "losing a tab must not take the group with it"
        );
        assert_eq!(l.group_of(DockPanel::DocMap).unwrap().active, 0);
        // And neither plugin panel can be resurrected by a show:
        // nothing remembers them, so they are simply gone.
        assert!(l
            .groups()
            .iter()
            .all(|g| g.panels.iter().all(|p| !matches!(p, DockPanel::Plugin(_)))));
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
                            cmd: None,
                            seal: None,
                        },
                        DockPanelSession {
                            kind: "hologram".into(),
                            cmd: None,
                            seal: None,
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
                        cmd: None,
                        seal: None,
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
                        cmd: None,
                        seal: None,
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

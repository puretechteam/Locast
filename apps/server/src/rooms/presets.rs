use locast_protocol::room::cap;

#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub name: &'static str,
    pub cap_set: u32,
}

pub const VIEWER: Preset = Preset {
    name: "Viewer",
    cap_set: cap::CHAT,
};

pub const EDITOR: Preset = Preset {
    name: "Editor",
    cap_set: cap::CHAT | cap::DRAW | cap::LASER | cap::UNDO_OWN,
};

pub const COHOST: Preset = Preset {
    name: "CoHost",
    cap_set: cap::PLAYBACK_CONTROL
        | cap::DRAW
        | cap::LASER
        | cap::CHAT
        | cap::MANAGE_ROOM
        | cap::KICK
        | cap::INVITE
        | cap::MEDIA
        | cap::UNDO_OWN
        | cap::CLEAR_ALL,
};

pub const ALL_PRESETS: &[(&str, u32)] = &[
    ("Viewer", VIEWER.cap_set),
    ("Editor", EDITOR.cap_set),
    ("CoHost", COHOST.cap_set),
];

pub fn preset_by_name(name: &str) -> Option<u32> {
    ALL_PRESETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, cap)| *cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_and_clear_defaults_follow_architecture_14_7() {
        // Viewer: no drawing bits at all.
        assert_eq!(
            VIEWER.cap_set & (cap::DRAW | cap::UNDO_OWN | cap::UNDO_ANY | cap::CLEAR_ALL),
            0
        );
        // Editor: draw + undo_own, nothing broader.
        assert_ne!(EDITOR.cap_set & cap::DRAW, 0);
        assert_ne!(EDITOR.cap_set & cap::UNDO_OWN, 0);
        assert_eq!(EDITOR.cap_set & (cap::UNDO_ANY | cap::CLEAR_ALL), 0);
        // Co-host: Editor + clear_all. undo_any is a per-user grant only.
        assert_eq!(COHOST.cap_set & EDITOR.cap_set, EDITOR.cap_set);
        assert_ne!(COHOST.cap_set & cap::CLEAR_ALL, 0);
        assert_eq!(COHOST.cap_set & cap::UNDO_ANY, 0);
        assert_eq!(preset_by_name("Editor"), Some(EDITOR.cap_set));
    }
}

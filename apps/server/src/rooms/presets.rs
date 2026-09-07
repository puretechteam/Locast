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
    cap_set: cap::CHAT | cap::DRAW | cap::LASER,
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
        | cap::MEDIA,
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

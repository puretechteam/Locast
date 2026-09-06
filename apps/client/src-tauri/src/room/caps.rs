//! P6-T01: capability checking chokepoint for the client.
//!
//! Mirrors the server [`locast_server::rooms::caps`] API. The
//! server is the authoritative source of truth; this module
//! provides a local mirror so the UI can gate itself without
//! making a round-trip for every button press.

#![forbid(unsafe_code)]

pub use locast_protocol::room::cap as cap_bits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Playback,
    Drawing,
    Chat,
    Room,
    Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    IssuePlaybackCommand,
    DrawBegin,
    DrawPoint,
    DrawEnd,
    UndoStroke,
    ClearAll,
    SendChat,
    ManageRoom,
    Kick,
    PublishManifest,
    Invite,
}

impl Scope {
    pub fn action_bit(self, action: Action) -> u32 {
        match (self, action) {
            (Scope::Playback, Action::IssuePlaybackCommand) => cap_bits::PLAYBACK_CONTROL,
            (Scope::Drawing, Action::DrawBegin) => cap_bits::DRAW,
            (Scope::Drawing, Action::DrawPoint) => cap_bits::DRAW,
            (Scope::Drawing, Action::DrawEnd) => cap_bits::DRAW,
            (Scope::Drawing, Action::UndoStroke) => cap_bits::DRAW,
            (Scope::Drawing, Action::ClearAll) => cap_bits::DRAW,
            (Scope::Chat, Action::SendChat) => cap_bits::CHAT,
            (Scope::Room, Action::ManageRoom) => cap_bits::MANAGE_ROOM,
            (Scope::Room, Action::Kick) => cap_bits::KICK,
            (Scope::Manifest, Action::PublishManifest) => cap_bits::PUBLISH_MANIFEST,
            (Scope::Manifest, Action::Invite) => cap_bits::INVITE,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanParams {
    pub user_id: String,
    pub host_user_id: String,
    pub cap_set: u32,
}

pub fn can(params: &CanParams, scope: Scope, action: Action) -> bool {
    if params.user_id == params.host_user_id {
        return true;
    }
    let bit = scope.action_bit(action);
    params.cap_set & bit != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(i: u8) -> String {
        format!("00000000-0000-0000-0000-000000000{:02x}", i)
    }

    fn host_params(cap_set: u32) -> CanParams {
        CanParams {
            user_id: uid(1),
            host_user_id: uid(1),
            cap_set,
        }
    }

    fn viewer_params(cap_set: u32) -> CanParams {
        CanParams {
            user_id: uid(2),
            host_user_id: uid(1),
            cap_set,
        }
    }

    fn non_member_params() -> CanParams {
        CanParams {
            user_id: uid(99),
            host_user_id: uid(1),
            cap_set: 0,
        }
    }

    #[test]
    fn host_has_all_capabilities() {
        let p = &host_params(cap_bits::CHAT);
        assert!(can(p, Scope::Playback, Action::IssuePlaybackCommand));
        assert!(can(p, Scope::Drawing, Action::DrawBegin));
        assert!(can(p, Scope::Drawing, Action::DrawPoint));
        assert!(can(p, Scope::Drawing, Action::DrawEnd));
        assert!(can(p, Scope::Drawing, Action::UndoStroke));
        assert!(can(p, Scope::Drawing, Action::ClearAll));
        assert!(can(p, Scope::Chat, Action::SendChat));
        assert!(can(p, Scope::Room, Action::ManageRoom));
        assert!(can(p, Scope::Room, Action::Kick));
        assert!(can(p, Scope::Manifest, Action::PublishManifest));
        assert!(can(p, Scope::Manifest, Action::Invite));
    }

    #[test]
    fn viewer_with_default_caps() {
        let p = &viewer_params(cap_bits::CHAT);
        assert!(!can(p, Scope::Playback, Action::IssuePlaybackCommand));
        assert!(!can(p, Scope::Drawing, Action::DrawBegin));
        assert!(can(p, Scope::Chat, Action::SendChat));
        assert!(!can(p, Scope::Room, Action::ManageRoom));
        assert!(!can(p, Scope::Room, Action::Kick));
        assert!(!can(p, Scope::Manifest, Action::PublishManifest));
        assert!(!can(p, Scope::Manifest, Action::Invite));
    }

    #[test]
    fn viewer_with_full_caps() {
        let full = cap_bits::PLAYBACK_CONTROL
            | cap_bits::DRAW
            | cap_bits::LASER
            | cap_bits::MANAGE_ROOM
            | cap_bits::KICK
            | cap_bits::PUBLISH_MANIFEST
            | cap_bits::INVITE
            | cap_bits::CHAT;
        let p = &viewer_params(full);
        assert!(can(p, Scope::Playback, Action::IssuePlaybackCommand));
        assert!(can(p, Scope::Drawing, Action::DrawBegin));
        assert!(can(p, Scope::Chat, Action::SendChat));
        assert!(can(p, Scope::Room, Action::ManageRoom));
        assert!(can(p, Scope::Room, Action::Kick));
        assert!(can(p, Scope::Manifest, Action::PublishManifest));
        assert!(can(p, Scope::Manifest, Action::Invite));
    }

    #[test]
    fn host_ignores_cap_set() {
        let p = &host_params(0);
        assert!(can(p, Scope::Playback, Action::IssuePlaybackCommand));
        assert!(can(p, Scope::Drawing, Action::DrawBegin));
    }

    #[test]
    fn non_member_has_no_capabilities() {
        let p = &non_member_params();
        assert!(!can(p, Scope::Playback, Action::IssuePlaybackCommand));
        assert!(!can(p, Scope::Drawing, Action::DrawBegin));
        assert!(!can(p, Scope::Chat, Action::SendChat));
    }
}

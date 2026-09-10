//! Windows has no user-facing permission gates for screen capture (Windows.Graphics.Capture
//! shows no prompt when capturing a monitor from a desktop app) or for UI Automation.

use agent_snap_core::platform::{PermState, Permission, Permissions};

pub struct WinPermissions;

impl Permissions for WinPermissions {
    fn state(&self, _p: Permission) -> PermState {
        PermState::Granted
    }

    fn request(&self, _p: Permission) {}

    fn open_settings(&self, _p: Permission) {}
}

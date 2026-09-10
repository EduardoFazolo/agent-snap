//! Screen Recording / Accessibility permission state, prompts, and settings deep links.

use std::process::Command;

use agent_snap_core::platform::{PermState, Permission, Permissions};
use objc2_application_services::{kAXTrustedCheckOptionPrompt, AXIsProcessTrusted, AXIsProcessTrustedWithOptions};
use objc2_core_foundation::{kCFBooleanTrue, CFBoolean, CFDictionary, CFString};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

/// macOS TCC permissions.
#[derive(Default)]
pub struct MacPermissions;

impl MacPermissions {
    pub fn new() -> Self {
        Self
    }
}

fn settings_url(p: Permission) -> &'static str {
    match p {
        Permission::ScreenRecording => "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        Permission::Accessibility => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
    }
}

impl Permissions for MacPermissions {
    fn state(&self, p: Permission) -> PermState {
        let granted = match p {
            Permission::ScreenRecording => CGPreflightScreenCaptureAccess(),
            Permission::Accessibility => unsafe { AXIsProcessTrusted() },
        };
        if granted {
            PermState::Granted
        } else {
            PermState::Denied
        }
    }

    fn request(&self, p: Permission) {
        match p {
            Permission::ScreenRecording => {
                CGRequestScreenCaptureAccess();
            }
            Permission::Accessibility => {
                let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
                let Some(yes): Option<&CFBoolean> = (unsafe { kCFBooleanTrue }) else { return };
                let opts = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[yes]);
                unsafe { AXIsProcessTrustedWithOptions(Some(opts.as_opaque())) };
            }
        }
    }

    fn open_settings(&self, p: Permission) {
        let _ = Command::new("/usr/bin/open").arg(settings_url(p)).spawn();
    }
}

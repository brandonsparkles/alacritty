//! Root program status for macOS native tabs (OSC 7501). Child records have
//! their own lifecycle and never own the tab's attention indicator.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    Clear,
    Status(ProgramStatus),
}

impl Report {
    pub(crate) fn parse(body: &[u8]) -> Option<Self> {
        // Reserve room for OSC, its number, delimiter and ST under the 4096-byte limit.
        if body.len() > 4087 {
            return None;
        }
        let mut state = None;
        for pair in std::str::from_utf8(body).ok()?.split(':') {
            let Some((key, value)) = pair.split_once('=') else { continue };
            let (key, value) = (key.trim_ascii(), value.trim_ascii());
            if key.len() > 16 {
                return None;
            }
            // Even an invalid child ID must never address the root.
            if key == "id" {
                return None;
            }
            if !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)) {
                continue;
            }
            match key {
                "state" => state = Some(value),
                "app" if value.len() > 32 => return None,
                "msg" | "title" => {
                    let limit = if key == "title" { 192usize } else { 2048 };
                    if value.len() > 4 * limit.div_ceil(3) {
                        return None;
                    }
                    let bytes =
                        STANDARD.decode(value).or_else(|_| STANDARD_NO_PAD.decode(value)).ok()?;
                    if bytes.len() > limit
                        || std::str::from_utf8(&bytes).ok()?.chars().any(char::is_control)
                    {
                        return None;
                    }
                },
                _ => (),
            }
        }
        let status = match state? {
            "idle" => ProgramStatus::Idle,
            "working" => ProgramStatus::Working,
            "blocked" => ProgramStatus::Blocked,
            "done" => ProgramStatus::Done,
            "error" => ProgramStatus::Error,
            "clear" => return Some(Self::Clear),
            _ => return None,
        };
        Some(Self::Status(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_status_ignores_child_records_and_invalid_reports() {
        for body in [
            "state=done:id=worker/nested",
            "state=clear:id=",
            "state=unknown",
            "state=done:title=AA==",
            "state=done:msg=/w==",
            "state=done:title=a",
        ] {
            assert_eq!(Report::parse(body.as_bytes()), None, "{body}");
        }
        assert_eq!(
            Report::parse(b"state=done:state=working:title=YQ"),
            Some(Report::Status(ProgramStatus::Working))
        );
    }
}

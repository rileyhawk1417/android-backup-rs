//! Pure parsers for adb output. No I/O — everything here is a function of a string,
//! which is what makes the whole device layer trivially testable.

/// Raw state token as reported by `adb devices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawState {
    Device,
    Unauthorized,
    Offline,
    Recovery,
    Unrecognized(String),
}

impl RawState {
    pub fn from_token(token: &str) -> Self {
        match token.to_ascii_lowercase().as_str() {
            "device" => Self::Device,
            "unauthorized" => Self::Unauthorized,
            "offline" => Self::Offline,
            "recovery" => Self::Recovery,
            other => Self::Unrecognized(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Device => "device",
            Self::Unauthorized => "unauthorized",
            Self::Offline => "offline",
            Self::Recovery => "recovery",
            Self::Unrecognized(s) => s,
        }
    }
}

/// One entry from `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    pub serial: String,
    pub state: RawState,
    /// `model:` property (e.g. `SM_A536B`).
    pub model: Option<String>,
    /// `product:` property (codename, e.g. `a53x`).
    pub product: Option<String>,
    pub transport_id: Option<String>,
}

/// Structural problems with device listing output.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    #[error("could not parse device listing line: {0}")]
    Line(String),
}

/// Parse `adb devices -l`.
///
/// Lenient by design: header lines, daemon chatter (`* daemon started
/// successfully`) and blank lines are skipped; unknown state tokens are preserved
/// as [`RawState::Unrecognized`] rather than causing a failure, because a future
/// adb version inventing a state must not make the app blind.
pub fn parse_devices(output: &str) -> Result<Vec<DeviceSnapshot>, ParseError> {
    let mut devices = Vec::new();
    for raw_line in output.lines() {
        let line = raw_line.trim_end_matches(['\r', '\n']).trim_end();
        if line.is_empty() {
            continue;
        }
        let lowered = line.to_ascii_lowercase();
        if lowered.starts_with('*') || lowered.contains("list of devices attached") {
            continue;
        }
        let Some(snapshot) = parse_line(line) else {
            tracing::debug!(line, "skipping unparsable device listing line");
            continue;
        };
        devices.push(snapshot);
    }
    Ok(devices)
}

fn parse_line(line: &str) -> Option<DeviceSnapshot> {
    // Serials may contain ':' (wifi adb), so the tab is the only safe separator.
    let (serial, rest) = match line.split_once('\t') {
        Some((serial, rest)) => (serial.trim(), rest.trim()),
        None => {
            // Some adb builds/emulators emit space-separated lines.
            let mut it = line.split_whitespace();
            let serial = it.next()?;
            let rest = it.next()?;
            let remainder = it.collect::<Vec<_>>().join(" ");
            if remainder.is_empty() {
                (serial, rest)
            } else {
                (serial, &line[serial.len()..])
            }
        }
    };
    if serial.is_empty() {
        return None;
    }
    let mut fields = rest.split_whitespace();
    let state_token = fields.next()?;
    let mut snapshot = DeviceSnapshot {
        serial: serial.to_string(),
        state: RawState::from_token(state_token),
        model: None,
        product: None,
        transport_id: None,
    };
    for field in fields {
        if let Some((key, value)) = field.split_once(':') {
            match key {
                "model" => snapshot.model = Some(value.to_string()),
                "product" => snapshot.product = Some(value.to_string()),
                "transport_id" => snapshot.transport_id = Some(value.to_string()),
                _ => {}
            }
        }
    }
    Some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_output() {
        let out = "List of devices attached\nABC123\tdevice\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].serial, "ABC123");
        assert_eq!(devices[0].state, RawState::Device);
    }

    #[test]
    fn parses_verbose_output() {
        let out = "List of devices attached\n\
                   R58M12ABCDE\tdevice usb:1-2 product:a53x model:SM_A536B device:a53x transport_id:3\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices[0].model.as_deref(), Some("SM_A536B"));
        assert_eq!(devices[0].product.as_deref(), Some("a53x"));
        assert_eq!(devices[0].transport_id.as_deref(), Some("3"));
    }

    #[test]
    fn parses_unauthorized() {
        let out = "List of devices attached\nABC123\tunauthorized\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices[0].state, RawState::Unauthorized);
    }

    #[test]
    fn parses_offline() {
        let out = "List of devices attached\nABC123\toffline\n";
        assert_eq!(parse_devices(out).unwrap()[0].state, RawState::Offline);
    }

    #[test]
    fn parses_wifi_serial_with_colon() {
        let out = "List of devices attached\n192.168.1.20:5555\tdevice\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices[0].serial, "192.168.1.20:5555");
    }

    #[test]
    fn parses_emulator_and_physical_together() {
        let out = "List of devices attached\n\
                   emulator-5554\tdevice product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 transport_id:1\n\
                   R58M12ABCDE\tdevice product:a53x model:SM_A536B transport_id:2\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].serial, "emulator-5554");
        assert_eq!(devices[1].model.as_deref(), Some("SM_A536B"));
    }

    #[test]
    fn skips_daemon_chatter_and_headers() {
        let out = "* daemon not running; starting now at tcp:5037\n\
                   * daemon started successfully\n\
                   List of devices attached\n\
                   ABC123\tdevice\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices.len(), 1);
    }

    #[test]
    fn handles_crlf_and_no_trailing_newline() {
        let out = "List of devices attached\r\nABC123\tdevice\r\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].serial, "ABC123");
        assert_eq!(devices[0].state, RawState::Device);
    }

    #[test]
    fn empty_listing_yields_no_devices() {
        assert!(parse_devices("List of devices attached\n\n")
            .unwrap()
            .is_empty());
        assert!(parse_devices("").unwrap().is_empty());
    }

    #[test]
    fn unknown_state_is_preserved() {
        let out = "List of devices attached\nABC123\twho_knows\n";
        let devices = parse_devices(out).unwrap();
        assert_eq!(devices[0].state, RawState::Unrecognized("who_knows".into()));
    }

    #[test]
    fn arbitrary_spacing_does_not_break_parsing() {
        for line in [
            "ABC123\tdevice",
            "ABC123\t\tdevice",
            "ABC123 device",
            "ABC123   device   ",
        ] {
            let out = format!("List of devices attached\n{line}\n");
            let devices = parse_devices(&out).unwrap();
            assert_eq!(devices.len(), 1, "line: {line:?}");
            assert_eq!(devices[0].serial, "ABC123", "line: {line:?}");
            assert_eq!(devices[0].state, RawState::Device, "line: {line:?}");
        }
    }
}

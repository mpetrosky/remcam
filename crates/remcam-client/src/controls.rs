//! Camera controls on the client side: parsing `--ctrl NAME=VALUE` and the
//! interactive commands typed while streaming.

use anyhow::{Context, bail};
use remcam_proto::{ControlInfo, ControlKind, control_name};

pub const HELP: &str = "\
commands:
  controls | c          list camera controls and their current values
  NAME=VALUE            set a control (also: set NAME VALUE)
  reset NAME            set a control back to its default
  help | ?              show this help
  quit | q              stop streaming and exit
values: numbers, true/false/on/off for switches, a number or label for menus";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    List,
    Set { name: String, value: String },
    Reset { name: String },
    Quit,
}

/// Parse one line of interactive input. Blank lines yield `None`.
pub fn parse_command(line: &str) -> anyhow::Result<Option<Command>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    if let Some((name, value)) = line.split_once('=') {
        return Ok(Some(Command::Set {
            name: name.trim().to_string(),
            value: value.trim().to_string(),
        }));
    }
    let mut words = line.split_whitespace();
    let cmd = match (words.next(), words.next(), words.next()) {
        (Some("help" | "?"), None, _) => Command::Help,
        (Some("controls" | "c"), None, _) => Command::List,
        (Some("quit" | "q" | "exit"), None, _) => Command::Quit,
        (Some("reset"), Some(name), None) => Command::Reset { name: name.into() },
        (Some("set"), Some(name), Some(value)) => Command::Set {
            name: name.into(),
            value: words.fold(value.to_string(), |acc, w| acc + " " + w),
        },
        _ => bail!("unknown command {line:?} (try 'help')"),
    };
    Ok(Some(cmd))
}

/// Split `--ctrl NAME=VALUE`.
pub fn parse_assignment(s: &str) -> anyhow::Result<(&str, &str)> {
    s.split_once('=')
        .map(|(n, v)| (n.trim(), v.trim()))
        .with_context(|| format!("expected NAME=VALUE, got {s:?}"))
}

pub fn find<'a>(controls: &'a [ControlInfo], name: &str) -> anyhow::Result<&'a ControlInfo> {
    let wanted = control_name(name);
    controls.iter().find(|c| c.name == wanted).with_context(|| {
        let names: Vec<_> = controls.iter().map(|c| c.name.as_str()).collect();
        format!("no control {name:?}; available: {}", names.join(", "))
    })
}

/// Interpret `text` as a value for `info`, accepting menu labels and
/// boolean words as well as plain numbers.
pub fn parse_value(info: &ControlInfo, text: &str) -> anyhow::Result<i64> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("default") {
        return Ok(info.default);
    }
    match &info.kind {
        ControlKind::Integer { .. } => text
            .parse()
            .with_context(|| format!("{} takes a number, got {text:?}", info.name)),
        ControlKind::Boolean => match text.to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" => Ok(1),
            "0" | "false" | "off" | "no" => Ok(0),
            _ => bail!("{} takes true/false, got {text:?}", info.name),
        },
        ControlKind::Menu { items } => {
            if let Ok(n) = text.parse() {
                return Ok(n);
            }
            let wanted = control_name(text);
            items
                .iter()
                .find(|(_, label)| control_name(label) == wanted)
                .map(|(v, _)| *v)
                .with_context(|| {
                    let labels: Vec<_> = items.iter().map(|(v, l)| format!("{v}={l}")).collect();
                    format!("{} options: {}", info.name, labels.join(", "))
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remcam_proto::ControlState;

    fn control(name: &str, kind: ControlKind) -> ControlInfo {
        ControlInfo {
            id: 1,
            name: name.into(),
            kind,
            default: 3,
            state: ControlState {
                id: 1,
                value: 3,
                inactive: false,
            },
            read_only: false,
        }
    }

    #[test]
    fn values() {
        let int = control(
            "brightness",
            ControlKind::Integer {
                min: -64,
                max: 64,
                step: 1,
            },
        );
        assert_eq!(parse_value(&int, "-10").unwrap(), -10);
        assert_eq!(parse_value(&int, "default").unwrap(), 3);
        assert!(parse_value(&int, "bright").is_err());

        let switch = control("white_balance_automatic", ControlKind::Boolean);
        assert_eq!(parse_value(&switch, "off").unwrap(), 0);
        assert_eq!(parse_value(&switch, "TRUE").unwrap(), 1);

        let menu = control(
            "auto_exposure",
            ControlKind::Menu {
                items: vec![
                    (1, "Manual Mode".into()),
                    (3, "Aperture Priority Mode".into()),
                ],
            },
        );
        assert_eq!(parse_value(&menu, "manual mode").unwrap(), 1);
        assert_eq!(parse_value(&menu, "aperture_priority_mode").unwrap(), 3);
        assert_eq!(parse_value(&menu, "1").unwrap(), 1);
        assert!(parse_value(&menu, "shutter").is_err());
    }

    #[test]
    fn lookup_by_any_spelling() {
        let controls = [control("exposure_time_absolute", ControlKind::Boolean)];
        assert!(find(&controls, "Exposure Time, Absolute").is_ok());
        assert!(find(&controls, "exposure").is_err());
    }

    #[test]
    fn commands() {
        assert_eq!(parse_command("  ").unwrap(), None);
        assert_eq!(parse_command("c").unwrap(), Some(Command::List));
        assert_eq!(
            parse_command("brightness = 10").unwrap(),
            Some(Command::Set {
                name: "brightness".into(),
                value: "10".into()
            })
        );
        assert_eq!(
            parse_command("set auto_exposure manual mode").unwrap(),
            Some(Command::Set {
                name: "auto_exposure".into(),
                value: "manual mode".into()
            })
        );
        assert_eq!(
            parse_command("reset gamma").unwrap(),
            Some(Command::Reset {
                name: "gamma".into()
            })
        );
        assert!(parse_command("frobnicate").is_err());
    }
}

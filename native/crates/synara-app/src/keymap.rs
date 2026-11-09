//! Turns the server's resolved keybindings (`server.getConfig`) into GPUI
//! keystroke strings for the commands this client implements, so the user's
//! `keybindings.json` and the shipped defaults in `KEYBINDINGS.md` apply here
//! unchanged. GPUI-free so it stays unit-testable.

use synara_protocol::server::{KeybindingShortcut, ResolvedKeybinding};

/// Web command ids this client handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `chat.new`: a new thread in the current project.
    NewThread,
    /// `sidebar.toggle`.
    ToggleSidebar,
    /// `chat.visible.next` / `chat.visible.previous`.
    NextThread,
    PreviousThread,
    /// `thread.jump.1` … `thread.jump.9`, zero-based here.
    JumpThread(usize),
}

impl Command {
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "chat.new" => Self::NewThread,
            "sidebar.toggle" => Self::ToggleSidebar,
            // `thread.next`/`thread.previous` are the server's legacy aliases.
            "chat.visible.next" | "thread.next" => Self::NextThread,
            "chat.visible.previous" | "thread.previous" => Self::PreviousThread,
            _ => {
                let n: usize = id.strip_prefix("thread.jump.")?.parse().ok()?;
                if !(1..=9).contains(&n) {
                    return None;
                }
                Self::JumpThread(n - 1)
            }
        })
    }
}

/// `when` identifiers as this window stands. There is no terminal yet, so
/// rules that need terminal focus never fire and `!terminalFocus` always
/// holds. Unknown identifiers are false.
fn context_flag(name: &str, is_mac: bool) -> bool {
    match name {
        "isMac" => is_mac,
        _ => false,
    }
}

/// A GPUI keystroke such as `ctrl-shift-n`, or `None` for keys GPUI names
/// differently and this table does not know.
pub fn keystroke(shortcut: &KeybindingShortcut, is_mac: bool) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let ctrl = shortcut.ctrl_key || (shortcut.mod_key && !is_mac);
    let cmd = shortcut.meta_key || (shortcut.mod_key && is_mac);
    if ctrl {
        parts.push("ctrl");
    }
    if shortcut.alt_key {
        parts.push("alt");
    }
    if shortcut.shift_key {
        parts.push("shift");
    }
    if cmd {
        parts.push("cmd");
    }
    let key = match shortcut.key.as_str() {
        "arrowleft" => "left",
        "arrowright" => "right",
        "arrowup" => "up",
        "arrowdown" => "down",
        " " => "space",
        "-" => "minus",
        "=" => "equal",
        "+" => "plus",
        key if key.chars().count() == 1 => key,
        key @ ("escape" | "enter" | "tab" | "backspace" | "delete" | "home" | "end" | "pageup"
        | "pagedown" | "insert") => key,
        key if key.starts_with('f') && key[1..].parse::<u8>().is_ok() => key,
        _ => return None,
    };
    parts.push(key);
    Some(parts.join("-"))
}

/// The bindings to install, in config order (later rules win in GPUI, as
/// they do in the web client).
pub fn bindings(rules: &[ResolvedKeybinding], is_mac: bool) -> Vec<(String, Command)> {
    rules
        .iter()
        .filter(|rule| {
            rule.when_ast
                .as_ref()
                .is_none_or(|when| when.eval(&|name| context_flag(name, is_mac)))
        })
        .filter_map(|rule| {
            let command = Command::from_id(&rule.command)?;
            Some((keystroke(&rule.shortcut, is_mac)?, command))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(
        command: &str,
        key: &str,
        mods: &str,
        when: Option<serde_json::Value>,
    ) -> ResolvedKeybinding {
        serde_json::from_value(serde_json::json!({
            "command": command,
            "shortcut": {"key": key, "metaKey": mods.contains("meta"),
                         "ctrlKey": mods.contains("ctrl"), "shiftKey": mods.contains("shift"),
                         "altKey": mods.contains("alt"), "modKey": mods.contains("mod")},
            "whenAst": when,
        }))
        .unwrap()
    }

    fn not_terminal() -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "not",
            "node": {"type": "identifier", "name": "terminalFocus"}}))
    }

    #[test]
    fn maps_mod_per_platform_and_names_keys() {
        let rules = vec![
            rule("chat.new", "n", "mod", not_terminal()),
            rule("chat.visible.next", "]", "mod shift", not_terminal()),
            rule("thread.jump.3", "3", "mod", None),
            rule("space.previous", "arrowleft", "mod alt", None),
        ];
        assert_eq!(
            bindings(&rules, false),
            vec![
                ("ctrl-n".into(), Command::NewThread),
                ("ctrl-shift-]".into(), Command::NextThread),
                ("ctrl-3".into(), Command::JumpThread(2)),
            ]
        );
        assert_eq!(
            bindings(&rules, true)[0],
            ("cmd-n".into(), Command::NewThread)
        );
        assert_eq!(
            keystroke(&rules[3].shortcut, false).as_deref(),
            Some("ctrl-alt-left")
        );
    }

    #[test]
    fn drops_rules_whose_condition_cannot_hold() {
        let terminal_only =
            Some(serde_json::json!({"type": "identifier", "name": "terminalFocus"}));
        let mac_only = Some(serde_json::json!({"type": "identifier", "name": "isMac"}));
        let rules = vec![
            rule("chat.new", "t", "mod", terminal_only),
            rule("sidebar.toggle", "b", "mod", mac_only),
        ];
        assert!(bindings(&rules, false).is_empty());
        assert_eq!(
            bindings(&rules, true),
            vec![("cmd-b".into(), Command::ToggleSidebar)]
        );
        assert_eq!(Command::from_id("thread.jump.10"), None);
        assert_eq!(
            Command::from_id("thread.previous"),
            Some(Command::PreviousThread)
        );
    }
}

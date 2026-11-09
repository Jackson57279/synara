//! `server.getConfig` (`ServerConfig` in `packages/contracts/src/server.ts`),
//! reduced to what the native client reads. Everything else stays in `extra`.

use serde::{Deserialize, Serialize};

use crate::orchestration::Extra;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    /// The user's keybindings with the shipped defaults filled in, already
    /// parsed by the server (`ResolvedKeybindingsConfig`).
    #[serde(default)]
    pub keybindings: Vec<ResolvedKeybinding>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `ResolvedKeybindingRule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedKeybinding {
    /// Web command id, such as `chat.new` or `thread.jump.3`.
    pub command: String,
    pub shortcut: KeybindingShortcut,
    #[serde(default)]
    pub when_ast: Option<WhenNode>,
}

/// `KeybindingShortcut`: `key` is lowercase, `" "` for space, and named keys
/// use DOM names (`arrowleft`, `escape`). `mod` means Cmd on macOS and Ctrl
/// elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeybindingShortcut {
    pub key: String,
    pub meta_key: bool,
    pub ctrl_key: bool,
    pub shift_key: bool,
    pub alt_key: bool,
    pub mod_key: bool,
}

/// `KeybindingWhenNode`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WhenNode {
    Identifier {
        name: String,
    },
    Not {
        node: Box<WhenNode>,
    },
    And {
        left: Box<WhenNode>,
        right: Box<WhenNode>,
    },
    Or {
        left: Box<WhenNode>,
        right: Box<WhenNode>,
    },
}

impl WhenNode {
    /// Evaluates the condition with `lookup` answering each identifier.
    pub fn eval(&self, lookup: &impl Fn(&str) -> bool) -> bool {
        match self {
            Self::Identifier { name } => lookup(name),
            Self::Not { node } => !node.eval(lookup),
            Self::And { left, right } => left.eval(lookup) && right.eval(lookup),
            Self::Or { left, right } => left.eval(lookup) || right.eval(lookup),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_resolved_keybindings_and_evaluates_when() {
        let config: ServerConfig = serde_json::from_value(serde_json::json!({
            "cwd": "/w",
            "keybindings": [{
                "command": "chat.new",
                "shortcut": {"key": "n", "metaKey": false, "ctrlKey": false,
                             "shiftKey": false, "altKey": false, "modKey": true},
                "whenAst": {"type": "or",
                    "left": {"type": "not", "node": {"type": "identifier", "name": "terminalFocus"}},
                    "right": {"type": "identifier", "name": "isMac"}}
            }],
            "providers": []
        }))
        .unwrap();
        let rule = &config.keybindings[0];
        assert_eq!(rule.command, "chat.new");
        assert!(rule.shortcut.mod_key);
        let when = rule.when_ast.as_ref().unwrap();
        assert!(when.eval(&|_| false));
        assert!(!when.eval(&|name| name == "terminalFocus"));
        assert_eq!(config.extra["cwd"], "/w");
    }
}

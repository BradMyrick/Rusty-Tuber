//! Control interface: a dependency-free stdin command reader that drives the
//! avatar by translating simple text commands into [`state::StateCommand`]s.
//!
//! This is the **control seam** for headless operation. Today it reads lines
//! from stdin; a future server, hotkey daemon, or Stream Deck integration can
//! either pipe commands here, or — more directly — construct
//! [`StateCommand`]s and feed the same `mpsc` sender the binary uses (see
//! [`crate::run`]). The [`protocol`] types (serialize/observe avatar state via
//! the broadcast channel) round out the surface a custom server would want.
//!
//! ## Commands
//!
//! | command | effect |
//! |---------|--------|
//! | `emotion <name>` | Trigger an eye-expression set; auto-reverts on its timer. |
//! | `clear` | Drop the emotion override; return to the resting face. |
//! | `default <name>` | Change the resting emotion. |
//! | `mouth <closed\|partial\|medium\|open>` | Force a mouth level (ignores mic). |
//! | `mouth auto` | Resume mic-driven mouth. |
//! | `eyes <open\|closed>` | Force eyes (pauses blinking). |
//! | `eyes auto` | Resume blinking. |
//! | `help` / `?` | Show the command list. |
//! | `quit` / `exit` | Shut down. |
//!
//! Commands are case-insensitive; blank lines are ignored.

use crate::assets::AssetCatalog;
use crate::protocol::{EyeState, MouthState};
use crate::state::StateCommand;
use std::io::BufRead;
use std::sync::Arc;
use tracing::info;

/// Outcome of parsing one input line.
#[derive(Debug, PartialEq)]
enum Parsed {
    /// A recognized control command to forward to the state task.
    Command(StateCommand),
    /// Print the help text.
    Help,
    /// Shut the process down.
    Quit,
    /// A blank line or comment — silently ignored.
    Ignore,
    /// Unrecognized input; carry a short reason for the error reply.
    Unknown(String),
}

/// Parse a single trimmed input line into a [`Parsed`] action.
///
/// Pure (no I/O) so it is straightforward to unit-test. Emotion/default names
/// keep their case but are matched case-insensitively by the catalog later;
/// here we only split the verb from its argument.
fn parse(line: &str) -> Parsed {
    let mut tokens = line.split_whitespace();
    let verb = match tokens.next() {
        Some(v) => v.to_ascii_lowercase(),
        None => return Parsed::Ignore,
    };
    // The remainder, rejoined so multi-word names (e.g. `default big smile`)
    // survive. Empty if there was no argument.
    let arg: String = tokens.collect::<Vec<_>>().join(" ");

    match verb.as_str() {
        "help" | "?" => Parsed::Help,
        "quit" | "exit" => Parsed::Quit,
        "clear" => Parsed::Command(StateCommand::ClearOverride),
        "emotion" => match arg.is_empty() {
            false => Parsed::Command(StateCommand::TriggerEmotion(arg)),
            true => Parsed::Unknown("usage: emotion <name>".into()),
        },
        "default" => match arg.is_empty() {
            false => Parsed::Command(StateCommand::SetDefault(arg)),
            true => Parsed::Unknown("usage: default <name>".into()),
        },
        "mouth" => {
            if arg.is_empty() {
                return Parsed::Unknown(
                    "usage: mouth <closed|partial|medium|open|auto>".into(),
                );
            }
            if arg.eq_ignore_ascii_case("auto") {
                return Parsed::Command(StateCommand::ClearMouthOverride);
            }
            match MouthState::from_str_ci(&arg) {
                Some(m) => Parsed::Command(StateCommand::SetMouthOverride(m)),
                None => Parsed::Unknown(format!(
                    "invalid mouth: {arg} (closed|partial|medium|open|auto)"
                )),
            }
        }
        "eyes" => {
            if arg.is_empty() {
                return Parsed::Unknown(
                    "usage: eyes <open|closed|auto>".into(),
                );
            }
            if arg.eq_ignore_ascii_case("auto") {
                return Parsed::Command(StateCommand::ClearEyesOverride);
            }
            match EyeState::from_str_ci(&arg) {
                Some(e) => Parsed::Command(StateCommand::SetEyesOverride(e)),
                None => Parsed::Unknown(format!(
                    "invalid eyes: {arg} (open|closed|auto)"
                )),
            }
        }
        other => Parsed::Unknown(format!("unknown command: {other}")),
    }
}

/// If `cmd` names an emotion that isn't in the catalog, return a heads-up so
/// the operator knows it'll fall back to the default eyes (rather than silently
/// doing nothing). The command is still forwarded either way.
fn emotion_warning(
    cmd: &StateCommand,
    catalog: &AssetCatalog,
) -> Option<String> {
    let name = match cmd {
        StateCommand::TriggerEmotion(n) | StateCommand::SetDefault(n) => n,
        _ => return None,
    };
    if name.is_empty() || catalog.has_emotion(name) {
        None
    } else {
        Some(format!(
            "warning: '{name}' is not in the catalog; using default eyes"
        ))
    }
}

/// Spawn the stdin reader on a **dedicated OS thread** (not the tokio runtime).
///
/// This is deliberate: a blocking `read()` on stdin cannot be cancelled, so if
/// it ran on tokio's blocking pool the runtime's `Drop` would hang on it at
/// shutdown and `Ctrl-C` would wedge the process. A plain OS thread is killed
/// outright when the process exits (it is never joined), so shutdown stays
/// clean.
///
/// It runs until stdin closes (EOF) or a `quit`/`exit` command arrives (which
/// sends [`StateCommand::Shutdown`]). If stdin is not interactive (e.g. the
/// process is daemonised) the thread ends on the first EOF and the rest of the
/// pipeline keeps running.
pub fn spawn_stdin(
    cmd_tx: tokio::sync::mpsc::UnboundedSender<StateCommand>,
    catalog: Arc<AssetCatalog>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            info!(
                "control interface ready on stdin — type `help` for commands \
                 (Ctrl-C to quit)"
            );
            // Lock once for the lifetime of the thread.
            let mut lock = stdin.lock();
            loop {
                line.clear();
                match lock.read_line(&mut line) {
                    Ok(0) => {
                        // EOF: no more commands. Leave the avatar running.
                        info!("stdin closed; control interface idle");
                        return;
                    }
                    Ok(_) => match parse(line.trim()) {
                        Parsed::Ignore => {}
                        Parsed::Help => print!("{HELP_TEXT}"),
                        Parsed::Quit => {
                            let _ = cmd_tx.send(StateCommand::Shutdown);
                            return;
                        }
                        Parsed::Unknown(msg) => eprintln!("? {msg}"),
                        Parsed::Command(cmd) => {
                            if let Some(w) = emotion_warning(&cmd, &catalog) {
                                eprintln!("{w}");
                            }
                            let _ = cmd_tx.send(cmd);
                        }
                    },
                    Err(e) => {
                        eprintln!("stdin read error: {e}");
                        return;
                    }
                }
            }
        })
        .expect("spawn control thread")
}

/// The help text shown by the `help` / `?` command.
const HELP_TEXT: &str = "\
Rusty-Tuber commands (case-insensitive):
  emotion <name>          trigger an eye-expression set (auto-reverts on timer)
  clear                   drop the emotion override (return to resting face)
  default <name>          change the resting emotion
  mouth <closed|partial|medium|open>  force a mouth level (ignores mic)
  mouth auto              resume mic-driven mouth
  eyes <open|closed>      force eyes (pauses blinking)
  eyes auto               resume blinking
  help | ?                show this message
  quit | exit             shut down
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_emotion_and_clear() {
        assert_eq!(
            parse("emotion happy"),
            Parsed::Command(StateCommand::TriggerEmotion("happy".into()))
        );
        assert_eq!(
            parse("clear"),
            Parsed::Command(StateCommand::ClearOverride)
        );
    }

    #[test]
    fn parses_default() {
        assert_eq!(
            parse("default surprised"),
            Parsed::Command(StateCommand::SetDefault("surprised".into()))
        );
        // multi-word name is preserved (catalog matches case-insensitively)
        assert_eq!(
            parse("default Big Smile"),
            Parsed::Command(StateCommand::SetDefault("Big Smile".into()))
        );
    }

    #[test]
    fn parses_mouth_variants() {
        assert_eq!(
            parse("mouth open"),
            Parsed::Command(StateCommand::SetMouthOverride(MouthState::Open))
        );
        assert_eq!(
            parse("MOUTH Partial"),
            Parsed::Command(StateCommand::SetMouthOverride(
                MouthState::Partial
            ))
        );
        assert_eq!(
            parse("mouth auto"),
            Parsed::Command(StateCommand::ClearMouthOverride)
        );
    }

    #[test]
    fn parses_eyes_variants() {
        assert_eq!(
            parse("eyes closed"),
            Parsed::Command(StateCommand::SetEyesOverride(EyeState::Closed))
        );
        assert_eq!(
            parse("EYES AUTO"),
            Parsed::Command(StateCommand::ClearEyesOverride)
        );
    }

    #[test]
    fn parses_meta_commands() {
        assert_eq!(parse("help"), Parsed::Help);
        assert_eq!(parse("?"), Parsed::Help);
        assert_eq!(parse("quit"), Parsed::Quit);
        assert_eq!(parse("exit"), Parsed::Quit);
        assert_eq!(parse(""), Parsed::Ignore);
        assert_eq!(parse("   "), Parsed::Ignore);
    }

    #[test]
    fn rejects_unknown_and_malformed() {
        assert!(matches!(parse("garbage"), Parsed::Unknown(_)));
        assert!(matches!(parse("emotion"), Parsed::Unknown(_)));
        assert!(matches!(parse("mouth"), Parsed::Unknown(_)));
        assert!(matches!(parse("mouth grin"), Parsed::Unknown(_)));
        assert!(matches!(parse("eyes sideways"), Parsed::Unknown(_)));
    }
}

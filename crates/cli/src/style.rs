//! Terminal styling.
//!
//! One rule: ink goes where the information is. An address is an identifier you rarely read,
//! so it recedes; an unreadable cell is the thing you came for, so it shouts.
//!
//! Colour never carries meaning on its own. These readouts end up pasted into Slack and
//! incident notes where ANSI is stripped, so every distinction is also expressed in the text —
//! `!` and `none` read the same with colour off.

use std::io::IsTerminal;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColorChoice {
    /// Colour when stdout is a terminal.
    #[default]
    Auto,
    Always,
    Never,
}

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Decide once, at startup.
pub fn init(choice: ColorChoice) {
    let on = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        // NO_COLOR is honoured whatever its value, per the convention. Piping into `jq` or a
        // file must never receive escape codes — stdout is data.
        ColorChoice::Auto => {
            std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
        }
    };
    let _ = ENABLED.set(on);
}

fn on() -> bool {
    // Defaults to off, so anything running before `init` — tests included — stays plain.
    *ENABLED.get().unwrap_or(&false)
}

fn wrap(code: &str, s: &str) -> String {
    if on() && !s.is_empty() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Recedes: addresses, units, anything structural.
pub fn dim(s: &str) -> String {
    wrap("2", s)
}

/// Stands out: group headings, the network name.
pub fn bold(s: &str) -> String {
    wrap("1", s)
}

/// Something could not be read.
pub fn alarm(s: &str) -> String {
    wrap("1;31", s)
}

/// Something the tool intends to change.
pub fn action(s: &str) -> String {
    wrap("33", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styling_is_inert_until_enabled() {
        // Tests and pipes must see plain text, so the default is off rather than on.
        assert_eq!(dim("0x1234"), "0x1234");
        assert_eq!(alarm("!"), "!");
        assert_eq!(bold("oracles"), "oracles");
    }

    #[test]
    fn empty_strings_are_never_wrapped() {
        // An empty cell wrapped in escape codes would widen a column by zero visible
        // characters and confuse alignment for no benefit.
        assert_eq!(dim(""), "");
    }
}

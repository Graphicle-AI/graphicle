//! Unsupported options: the text of every refusal.
//!
//! A request that names an option, option value, setting or job kind that
//! Graphicle does not support gets an explicit error instead of being ignored
//! or misread. The memory library and the `graphicle` program build on this
//! crate, so they reuse [`message`] for their own unsupported options, values
//! and job kinds; each entry also carries a short unit name, which job status
//! reports ([`listing`]).
//!
//! The engine's own unsupported settings that a caller can still name are two
//! options of `watch` ([`check_watch`]).

/// How job status names `unit`: "`<unit>`".
pub fn listing(unit: &str) -> String {
    format!("`{unit}`")
}

/// The refusal of `name` (an unsupported option, value, setting or job kind,
/// quoted as the caller wrote it; `unit` is its entry's unit name):
/// "`--incremental` is not supported by Graphicle".
pub fn message(name: &str, _unit: &str) -> String {
    format!("{name} is not supported by Graphicle")
}

/// The unit of `watch`'s unsupported options.
pub const WATCH_UNIT: &str = "engine-watch-incremental";

/// `watch`'s unsupported options: an incremental rebuild and its poll interval
/// (`serve --watch` rebuilds on changes).
pub const WATCH_OPTIONS: &[&str] = &["incremental", "poll-ms"];

/// Refuse `watch`'s unsupported options among `args`, the words after `watch`,
/// written `--option`, `--option value` or `--option=value` before any `--`.
/// The command line gives an unknown option the next word as its value, so
/// without this `watch --incremental ./proj` would silently watch `.`.
pub fn check_watch(args: &[String]) -> crate::Result<()> {
    for arg in args.iter().take_while(|arg| arg.as_str() != "--") {
        let Some(option) = arg.strip_prefix("--") else {
            continue;
        };
        let name = option.split_once('=').map_or(option, |(name, _)| name);
        if let Some(unsupported) = WATCH_OPTIONS.iter().find(|option| **option == name) {
            return Err(message(&format!("`--{unsupported}`"), WATCH_UNIT).into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn watch_refuses_its_unsupported_options_in_every_spelling() {
        for (line, option) in [
            ("--incremental ./proj", "--incremental"),
            ("./proj --incremental", "--incremental"),
            ("--incremental=1 .", "--incremental"),
            ("--poll-ms 50 .", "--poll-ms"),
            (". --debounce 1 --poll-ms=50", "--poll-ms"),
        ] {
            let error = check_watch(&words(line)).unwrap_err().to_string();
            assert_eq!(
                error,
                format!("`{option}` is not supported by Graphicle"),
                "{line}"
            );
        }
        for line in [
            "",
            ".",
            "./proj --debounce 3",
            "--notify-only .",
            "-- --incremental",
            "--incrementals .",
        ] {
            assert!(check_watch(&words(line)).is_ok(), "{line}");
        }
    }
}

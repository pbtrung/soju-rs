//! Command-line flag parsing with the same syntax and error messages as Go's
//! flag package, used by BouncerServ commands.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A boolean flag which may be given without a value (-flag)
    Bool,
    /// A boolean flag requiring a value (-flag true)
    BoolValue,
    String,
    Int,
    /// An integer parsed like strconv.Atoi (custom Go flag values)
    Atoi,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FlagError {
    /// -h or -help was given
    Help,
    Other(String),
}

impl std::fmt::Display for FlagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlagError::Help => f.write_str("flag: help requested"),
            FlagError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for FlagError {}

#[derive(Default)]
pub struct FlagSet {
    flags: HashMap<String, Kind>,
}

/// The result of parsing: flag values in order of appearance, and the
/// remaining positional arguments.
#[derive(Debug, Default)]
pub struct Parsed {
    values: HashMap<String, Vec<String>>,
    pub args: Vec<String>,
}

fn parse_go_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

impl FlagSet {
    pub fn new() -> FlagSet {
        FlagSet::default()
    }

    /// A boolean flag, which can be set with -name or -name=value.
    pub fn bool(mut self, name: &str) -> FlagSet {
        self.flags.insert(name.to_owned(), Kind::Bool);
        self
    }

    /// A boolean flag which requires a value (-name value).
    pub fn bool_value(mut self, name: &str) -> FlagSet {
        self.flags.insert(name.to_owned(), Kind::BoolValue);
        self
    }

    pub fn string(mut self, name: &str) -> FlagSet {
        self.flags.insert(name.to_owned(), Kind::String);
        self
    }

    pub fn int(mut self, name: &str) -> FlagSet {
        self.flags.insert(name.to_owned(), Kind::Int);
        self
    }

    pub fn atoi(mut self, name: &str) -> FlagSet {
        self.flags.insert(name.to_owned(), Kind::Atoi);
        self
    }

    pub fn parse(&self, args: &[String]) -> Result<Parsed, FlagError> {
        let mut parsed = Parsed::default();
        let mut i = 0;
        while i < args.len() {
            let s = &args[i];
            if s.len() < 2 || !s.starts_with('-') {
                break;
            }
            let mut num_minuses = 1;
            if s.as_bytes()[1] == b'-' {
                num_minuses += 1;
                if s.len() == 2 {
                    // "--" terminates the flags
                    i += 1;
                    break;
                }
            }
            let name = &s[num_minuses..];
            if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
                return Err(FlagError::Other(format!("bad flag syntax: {s}")));
            }
            i += 1;

            let (name, value) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (name, None),
            };

            let Some(&kind) = self.flags.get(name) else {
                if name == "help" || name == "h" {
                    return Err(FlagError::Help);
                }
                return Err(FlagError::Other(format!(
                    "flag provided but not defined: -{name}"
                )));
            };

            let value = if kind == Kind::Bool {
                match value {
                    Some(v) => {
                        if parse_go_bool(&v).is_none() {
                            return Err(FlagError::Other(format!(
                                "invalid boolean value {v:?} for -{name}: parse error"
                            )));
                        }
                        v
                    }
                    None => "true".to_owned(),
                }
            } else {
                match value {
                    Some(v) => v,
                    None if i < args.len() => {
                        i += 1;
                        args[i - 1].clone()
                    }
                    None => {
                        return Err(FlagError::Other(format!("flag needs an argument: -{name}")));
                    }
                }
            };

            match kind {
                Kind::BoolValue if parse_go_bool(&value).is_none() => {
                    return Err(FlagError::Other(format!(
                        "invalid value {value:?} for flag -{name}: strconv.ParseBool: parsing {value:?}: invalid syntax"
                    )));
                }
                Kind::Int if value.parse::<i64>().is_err() => {
                    return Err(FlagError::Other(format!(
                        "invalid value {value:?} for flag -{name}: parse error"
                    )));
                }
                Kind::Atoi if value.parse::<i64>().is_err() => {
                    return Err(FlagError::Other(format!(
                        "invalid value {value:?} for flag -{name}: strconv.Atoi: parsing {value:?}: invalid syntax"
                    )));
                }
                _ => {}
            }

            parsed
                .values
                .entry(name.to_owned())
                .or_default()
                .push(value);
        }
        parsed.args = args[i..].to_vec();
        Ok(parsed)
    }
}

impl Parsed {
    /// The last value of a flag, if it was given.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    /// All values of a repeatable flag, if it was given.
    pub fn get_all(&self, name: &str) -> Option<&[String]> {
        self.values.get(name).map(Vec::as_slice)
    }

    pub fn get_bool(&self, name: &str) -> Option<bool> {
        self.get(name).and_then(parse_go_bool)
    }

    pub fn get_int(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(|v| v.parse().ok())
    }

    pub fn string_or(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or(default).to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse() {
        let fs = FlagSet::new()
            .string("addr")
            .bool("ignore-limit")
            .bool_value("enabled")
            .int("bits")
            .string("connect-command");
        let p = fs
            .parse(&args(&[
                "-addr",
                "irc.example.org",
                "--ignore-limit",
                "-enabled=false",
                "-bits=4096",
                "-connect-command",
                "a",
                "-connect-command",
                "b",
                "rest",
                "-addr",
            ]))
            .unwrap();
        assert_eq!(p.get("addr"), Some("irc.example.org"));
        assert_eq!(p.get_bool("ignore-limit"), Some(true));
        assert_eq!(p.get_bool("enabled"), Some(false));
        assert_eq!(p.get_int("bits"), Some(4096));
        assert_eq!(p.get_all("connect-command").unwrap(), ["a", "b"]);
        assert_eq!(p.args, args(&["rest", "-addr"]));

        let p = fs.parse(&args(&["--", "-addr"])).unwrap();
        assert_eq!(p.args, args(&["-addr"]));
        let p = fs.parse(&args(&["-", "x"])).unwrap();
        assert_eq!(p.args, args(&["-", "x"]));
    }

    #[test]
    fn errors() {
        let fs = FlagSet::new()
            .string("addr")
            .bool_value("enabled")
            .int("bits");
        let err = |a: &[&str]| fs.parse(&args(a)).unwrap_err().to_string();
        assert_eq!(err(&["-nope"]), "flag provided but not defined: -nope");
        assert_eq!(err(&["-addr"]), "flag needs an argument: -addr");
        assert_eq!(err(&["---x"]), "bad flag syntax: ---x");
        assert_eq!(
            err(&["-enabled", "maybe"]),
            "invalid value \"maybe\" for flag -enabled: strconv.ParseBool: parsing \"maybe\": invalid syntax"
        );
        assert_eq!(
            err(&["-bits", "x"]),
            "invalid value \"x\" for flag -bits: parse error"
        );
        assert_eq!(fs.parse(&args(&["-help"])).unwrap_err(), FlagError::Help);
    }
}

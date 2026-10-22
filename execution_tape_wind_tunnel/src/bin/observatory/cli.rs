// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use execution_tape::program::Program;
use std::{
    ffi::OsString,
    io::{self, Read},
    path::{Path, PathBuf},
};

pub(super) const HELP: &str = "Execution Observatory

Usage: observatory [--snapshot] [--color=always]
       observatory --program PATH [--snapshot] [--color=always]
       observatory --write-demo PATH

Without a path, capture and explore the built-in read/compute/store demo.
--program PATH   Browse a compiled tape without executing code or loading a host.
                 Rejected programs remain inspectable; undecodable files report an error.
                 Input files are limited to 64 MiB (decoded storage is additional).
--snapshot       Print the initial 120-column by 36-row view without terminal modes.
--color=always   Force terminal colors, including when NO_COLOR is set.
--write-demo     Write the demo's compiled program to a new file, then exit.
--help           Show this help.

Browser: Tab focus; [ / ] tables; / search visible labels (case sensitive);
         arrows navigate; Enter follow a call/branch; f false arm;
         Backspace back; d diagnostic; q quit.
";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
    Demo,
    Program(PathBuf),
    WriteDemo(PathBuf),
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Options {
    pub(super) action: Action,
    pub(super) snapshot: bool,
    pub(super) force_colors: bool,
}

impl Options {
    pub(super) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut action = None;
        let mut snapshot = false;
        let mut force_colors = false;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--help" {
                return Ok(Self {
                    action: Action::Help,
                    snapshot: false,
                    force_colors: false,
                });
            }
            if arg == "--snapshot" {
                snapshot = true;
            } else if arg == "--color=always" {
                force_colors = true;
            } else if arg == "--program" || arg == "--write-demo" {
                if action.is_some() {
                    return Err("choose one of --program or --write-demo".into());
                }
                let path = args
                    .next()
                    .ok_or_else(|| format!("{arg:?} requires a path"))?;
                action = Some(if arg == "--program" {
                    Action::Program(path.into())
                } else {
                    Action::WriteDemo(path.into())
                });
            } else {
                return Err(format!("unknown argument {arg:?}; use --help"));
            }
        }
        let action = action.unwrap_or(Action::Demo);
        if matches!(action, Action::WriteDemo(_)) && (snapshot || force_colors) {
            return Err("--write-demo cannot be combined with display options".into());
        }
        Ok(Self {
            action,
            snapshot,
            force_colors,
        })
    }
}

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

fn read_bounded(reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("compiled tape exceeds the {limit}-byte input limit"),
        ));
    }
    Ok(bytes)
}

pub(super) fn load_program(path: &Path) -> Result<Program, Box<dyn std::error::Error>> {
    let bytes = std::fs::File::open(path)
        .and_then(|file| read_bounded(file, MAX_FILE_BYTES))
        .map_err(|error| format!("cannot read compiled tape {path:?}: {error}"))?;
    Program::decode(&bytes)
        .map_err(|error| format!("cannot decode compiled tape {path:?}: {error}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Options, String> {
        Options::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn file_and_demo_modes_are_explicit_and_flags_compose() {
        assert_eq!(parse(&[]).unwrap().action, Action::Demo);
        let options = parse(&["--snapshot", "--program", "a tape.bin", "--color=always"]).unwrap();
        assert_eq!(options.action, Action::Program("a tape.bin".into()));
        assert!(options.snapshot && options.force_colors);
        assert!(parse(&["--program"]).is_err());
        assert!(parse(&["--program", "a", "--write-demo", "b"]).is_err());
        assert!(parse(&["--program", "a", "--program", "b"]).is_err());
        assert!(parse(&["--write-demo", "a", "--snapshot"]).is_err());
        assert!(parse(&["--unknown"]).is_err());
    }

    #[test]
    fn input_limit_is_enforced_even_for_a_reader_without_file_metadata() {
        assert_eq!(read_bounded(&b"1234"[..], 4).unwrap(), b"1234");
        assert_eq!(
            read_bounded(&b"12345"[..], 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let error = Program::decode(&read_bounded(&b"not a tape"[..], 64).unwrap()).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn paths_do_not_have_to_be_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(vec![b't', 255]);
        let options = Options::parse([OsString::from("--program"), path.clone()]).unwrap();
        assert_eq!(options.action, Action::Program(path.into()));
    }
}

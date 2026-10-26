//! `cortex-pack release ...`: the version rule, for the release scripts (`scripts/release-*.sh`,
//! `docs/RELEASES.md`).

use cc_protocol::release::Version;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum Release {
    /// Print the version after `version` (each part 0 to 99; 0.3.99 -> 0.4.0).
    Next { version: String },
    /// Succeed when `candidate` is a newer version than `current`.
    Newer { candidate: String, current: String },
    /// Print `version` normalized (`v0.3.7` -> `0.3.7`); fail when it breaks the version rule.
    Check { version: String },
}

pub fn run(command: Release) -> Result<(), String> {
    match command {
        Release::Next { version } => {
            println!("{}", parse(&version)?.next());
            Ok(())
        }
        Release::Newer { candidate, current } => {
            let (candidate, current) = (parse(&candidate)?, parse(&current)?);
            if candidate > current { Ok(()) } else { Err(format!("{candidate} is not newer than {current}")) }
        }
        Release::Check { version } => {
            println!("{}", parse(&version)?);
            Ok(())
        }
    }
}

fn parse(version: &str) -> Result<Version, String> {
    Version::parse(version).ok_or_else(|| format!("`{version}` is not a version: three numbers, each 0 to 99 (0.3.7)"))
}

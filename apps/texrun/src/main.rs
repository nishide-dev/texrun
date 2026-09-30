//! Command-line entry point for texrun.

use clap::Parser;

/// Compile LaTeX documents and report structured results.
#[derive(Debug, Parser)]
#[command(name = "texrun", version = texrun_core::VERSION, about, long_about = None)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}

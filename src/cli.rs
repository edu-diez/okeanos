use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "okeanos", version, about = "Autonomous coding agent harness", long_about = None)]
pub struct Cli {
    #[arg(index = 1, value_name = "PROMPT")]
    pub prompt: Option<String>,

    #[arg(long, conflicts_with = "headless")]
    pub interactive: bool,

    #[arg(long, conflicts_with = "interactive")]
    pub headless: bool,

    #[arg(
        long,
        value_name = "PATH",
        help = "Project root directory (defaults to CWD)"
    )]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Interactive,
    Headless,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        if self.headless {
            Mode::Headless
        } else {
            Mode::Interactive
        }
    }
}

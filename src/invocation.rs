use ahp_types::state::PendingMessageKind;
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub enum Invocation {
    Help(HelpTopic),
    Chat {
        editor: Editor,
        session: String,
        command: ChatCommand,
    },
    Assignment {
        config: PathBuf,
        receipt: PathBuf,
        command: AssignmentCommand,
    },
    Claude(ProviderArgs),
    Google(ProviderArgs),
}

#[derive(Debug, PartialEq)]
pub enum HelpTopic {
    General,
    Ahp,
    NamedAhp { assignments: bool },
}

#[derive(Debug, PartialEq)]
pub struct ProviderArgs {
    // Each runner owns its subcommand grammar.
    pub args: Vec<String>,
    pub model: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Editor {
    pub code: String,
    pub profile: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum ChatCommand {
    Models,
    Chats,
    OnChat { chat: String, action: ChatAction },
}

#[derive(Debug, PartialEq)]
pub enum ChatAction {
    Status,
    Message {
        kind: PendingMessageKind,
        model: String,
        prompt: PathBuf,
    },
}

#[derive(Debug, PartialEq)]
pub enum AssignmentCommand {
    Check,
    Inspect,
    Watch,
    Submit(PathBuf),
}

#[derive(Debug)]
pub enum UsageError {
    Arguments(String),
    Chat,
    Assignment,
}

impl From<String> for UsageError {
    fn from(message: String) -> Self {
        Self::Arguments(message)
    }
}

impl From<&str> for UsageError {
    fn from(message: &str) -> Self {
        Self::Arguments(message.into())
    }
}

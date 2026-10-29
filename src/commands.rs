pub enum Command {
    Start,
}

pub fn parse_command(text: &str) -> Option<Command> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }

    let cmd = text.split_whitespace().next()?.split('@').next()?.to_lowercase();

    match cmd.as_str() {
        "/start" => Some(Command::Start),
        _ => None,
    }
}

//! Security of what the model runs and reads, decided by the host from the parsed command and the
//! text itself, never from the model's word:
//! - [`threats`]: a command that runs a download nobody has read, decodes hidden text into a
//!   shell, opens a shell another machine controls, reads the keychain's passwords, sends a
//!   credential file away, turns a macOS protection off, or holds invisible characters;
//! - [`installs`] and [`check_packages`]: the packages an install command adds (or `npx` runs),
//!   checked before it runs: a known malicious package or a name no registry has is not installed
//!   (a small model invents package names, and names that do not exist yet are registered by
//!   attackers), a version with known vulnerabilities runs with a note;
//! - [`invisible_note`]: invisible and direction-changing characters in text the model reads or
//!   writes (code can read differently from how it looks);
//! - [`mask_secrets`]: secret values in what the model reads become their kind and first letters.
//!
//! The CortexTerm on this Mac, when it shares `terminal.guard` (docs/STANDARD-CHANGES.md), answers
//! the package and script checks with its own data (KEV, its threat rules); without it, the checks
//! here ask OSV and the registries directly. An offline workspace asks nobody.

use crate::approval::{arguments, literal, program_name, unwrap};
use crate::host::Host;
use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;
use tree_sitter::Node;

/// What the host found in a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Threat {
    pub rule: &'static str,
    /// High: it never runs without the person's yes. Otherwise it runs, with a note.
    pub high: bool,
    /// What the command does, as the end of "Not run: it ...".
    pub why: String,
    /// For a download piped into a program: the URL and the program.
    pub url: Option<String>,
    pub runner: Option<String>,
}

impl Threat {
    fn new(rule: &'static str, high: bool, why: String) -> Threat {
        Threat { rule, high, why, url: None, runner: None }
    }
}

const DOWNLOADERS: &[&str] = &["curl", "wget", "fetch", "aria2c", "http", "https", "xh"];
const INTERPRETERS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "python", "node", "perl", "ruby", "php", "osascript", "pwsh", "deno", "bun"];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];
const NETWORK: &[&str] = &["curl", "wget", "nc", "ncat", "netcat", "socat", "ssh", "scp", "sftp", "rsync", "telnet", "ftp", "http", "https", "xh"];
/// Programs that only pass a file's bytes along (so a credential file they read goes where the
/// next program sends it).
const READERS: &[&str] = &["cat", "base64", "gzip", "tar", "zip", "openssl", "xxd", "head", "tail", "od", "hexdump"];

/// Everything the host finds in a command, high ones first.
pub fn threats(command: &str) -> Vec<Threat> {
    let mut out = Vec::new();
    threats_into(command, 0, &mut out);
    out.sort_by_key(|threat| !threat.high);
    out.dedup_by(|a, b| a.rule == b.rule && a.why == b.why);
    out
}

fn threats_into(script: &str, depth: usize, out: &mut Vec<Threat>) {
    if depth > 6 {
        return;
    }
    hidden_text(script, out);
    let Some(tree) = crate::lang::parse(crate::lang::Lang::Bash, script) else { return };
    let source = script.as_bytes();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "pipeline" => pipeline(node, source, out),
            "command" => command_node(node, source, depth, out),
            "file_redirect" => redirect(node, source, out),
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// The words of a command node from its real program on (past sudo, env, timeout ...).
fn words_of(node: Node, source: &[u8]) -> Option<Vec<Option<String>>> {
    let name = node.child_by_field_name("name")?;
    let mut words = vec![literal(name, source)];
    words.extend(arguments(node, source));
    Some(real_program(words))
}

fn real_program(mut words: Vec<Option<String>>) -> Vec<Option<String>> {
    for _ in 0..4 {
        match unwrap(&words) {
            Some((index, _)) if index > 0 && index < words.len() => words = words[index..].to_vec(),
            _ => break,
        }
    }
    words
}

/// The program's name without its folder and version (`/usr/bin/python3.12` -> `python`).
fn base(program: &str) -> &str {
    let name = program_name(program);
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if trimmed.is_empty() { name } else { trimmed }
}

fn program_of(words: &[Option<String>]) -> Option<&str> {
    words.first()?.as_deref().map(base)
}

fn literal_args(words: &[Option<String>]) -> Vec<&str> {
    words.iter().skip(1).filter_map(|word| word.as_deref()).collect()
}

/// An interpreter that reads its program from standard input (`sh`, `bash -s -- x`, `python3 -`).
fn reads_stdin(words: &[Option<String>]) -> bool {
    let Some(program) = program_of(words) else { return false };
    if !INTERPRETERS.contains(&program) {
        return false;
    }
    let shell = SHELLS.contains(&program);
    let mut options = true;
    for word in &words[1..] {
        let Some(word) = word.as_deref() else { return false };
        if !options {
            // Arguments after `--` or `-s` go to the script read from stdin.
            continue;
        }
        match word {
            "-" | "--" => options = false,
            "-s" if shell => options = false,
            "--command" | "--eval" | "--print" => return false,
            w if w.starts_with("--") => {}
            // A short option cluster that takes the program on the command line: `-c` for a shell
            // (`-lc`; `-e` is errexit there), `-c`, `-m`, `-e`, `-E`, `-p` for an interpreter
            // (`perl -pe`, `ruby -ne`, `python3 -c`).
            w if w.starts_with('-') => {
                let cluster = &w[1..];
                let inline = if shell { cluster.contains('c') } else { cluster.contains(['c', 'm', 'e', 'E', 'p']) };
                if inline {
                    return false;
                }
            }
            // A script file: the program is that file, not the input.
            _ => return false,
        }
    }
    true
}

fn first_url(words: &[Option<String>]) -> Option<String> {
    literal_args(words).into_iter().find(|arg| arg.starts_with("http://") || arg.starts_with("https://")).map(str::to_owned)
}

fn host_of(url: &str) -> String {
    let host = crate::approval::url_host(url);
    if host.is_empty() { url.to_owned() } else { host }
}

/// The first simple command inside a pipeline stage.
fn stage_command(node: Node) -> Option<Node> {
    if node.kind() == "command" {
        return Some(node);
    }
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "command" {
            return Some(current);
        }
        let mut cursor = current.walk();
        let children: Vec<Node> = current.named_children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    None
}

fn decodes(words: &[Option<String>]) -> bool {
    let args = literal_args(words);
    match program_of(words) {
        Some("base64") => args.iter().any(|a| matches!(*a, "-d" | "-D" | "--decode") || (a.starts_with('-') && !a.starts_with("--") && a.contains('d'))),
        Some("openssl") => args.iter().any(|a| matches!(*a, "base64" | "enc")) && args.contains(&"-d"),
        Some("xxd") => args.iter().any(|a| a.starts_with('-') && a.contains('r')),
        Some("uudecode") => true,
        _ => false,
    }
}

fn pipeline(node: Node, source: &[u8], out: &mut Vec<Threat>) {
    let mut cursor = node.walk();
    let stages: Vec<Vec<Option<String>>> =
        node.named_children(&mut cursor).filter_map(stage_command).filter_map(|command| words_of(command, source)).collect();
    for (index, words) in stages.iter().enumerate() {
        let later = &stages[index + 1..];
        let Some(program) = program_of(words) else { continue };
        let runner = later.iter().find(|stage| reads_stdin(stage)).and_then(|stage| program_of(stage).map(str::to_owned));
        if DOWNLOADERS.contains(&program)
            && let Some(runner) = runner.clone()
        {
            let url = first_url(words);
            let from = url.as_deref().map(host_of).unwrap_or_else(|| "the network".to_owned());
            let mut threat = Threat::new("pipe_to_shell", true, format!("pipes a download from {from} straight into `{runner}`, so code nobody has read would run"));
            threat.url = url;
            threat.runner = Some(runner);
            out.push(threat);
        } else if decodes(words)
            && let Some(runner) = runner
        {
            out.push(Threat::new("decode_to_shell", true, format!("decodes hidden text and runs it with `{runner}`")));
        }
        if READERS.contains(&program)
            && let Some(file) = literal_args(words).into_iter().find(|arg| credential_file(arg))
            && later.iter().any(|stage| program_of(stage).is_some_and(|p| NETWORK.contains(&p)))
        {
            out.push(Threat::new("secrets_to_network", true, format!("sends a credential file ({file}) to another machine")));
        }
    }
}

/// A download inside `$(...)` or `<(...)` of a command node's arguments.
fn substituted_download(node: Node, source: &[u8]) -> Option<Option<String>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(current.kind(), "command_substitution" | "process_substitution") {
            let mut inner = vec![current];
            while let Some(candidate) = inner.pop() {
                if candidate.kind() == "command"
                    && let Some(words) = words_of(candidate, source)
                    && program_of(&words).is_some_and(|p| DOWNLOADERS.contains(&p))
                {
                    return Some(first_url(&words));
                }
                let mut cursor = candidate.walk();
                for child in candidate.named_children(&mut cursor) {
                    inner.push(child);
                }
            }
        }
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn command_node(node: Node, source: &[u8], depth: usize, out: &mut Vec<Threat>) {
    let Some(words) = words_of(node, source) else { return };
    let Some(program) = program_of(&words) else { return };
    let args = literal_args(&words);
    // `bash -c '<script>'`: the script is checked as a command of its own.
    if SHELLS.contains(&program)
        && let Some(position) = words.iter().skip(1).position(|w| w.as_deref().is_some_and(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c')))
        && let Some(Some(script)) = words.get(position + 2)
    {
        threats_into(script, depth + 1, out);
    }
    // `bash <(curl ...)`, `sh -c "$(curl ...)"`, `eval "$(curl ...)"`, `source <(curl ...)`.
    if (INTERPRETERS.contains(&program) || matches!(program, "eval" | "source" | "."))
        && let Some(url) = substituted_download(node, source)
    {
        let from = url.as_deref().map(host_of).unwrap_or_else(|| "the network".to_owned());
        let runner = program.to_owned();
        let mut threat = Threat::new("pipe_to_shell", true, format!("runs a download from {from} with `{runner}`, so code nobody has read would run"));
        threat.url = url;
        threat.runner = Some(runner);
        out.push(threat);
    }
    match program {
        "nc" | "ncat" | "netcat" if args.iter().any(|a| matches!(*a, "-e" | "-c" | "--exec" | "--sh-exec" | "--lua-exec")) => {
            out.push(Threat::new("reverse_shell", true, "opens a shell that another machine controls".to_owned()));
        }
        "socat" if args.iter().any(|a| {
            let lower = a.to_ascii_lowercase();
            lower.contains("exec:") || lower.contains("system:")
        }) =>
        {
            out.push(Threat::new("reverse_shell", true, "opens a shell that another machine controls".to_owned()));
        }
        "security" => keychain(&args, out),
        "spctl" if args.iter().any(|a| matches!(*a, "--master-disable" | "--global-disable" | "--disable")) => {
            out.push(Threat::new("protection_off", true, "turns off a macOS protection (Gatekeeper, which checks apps before they open)".to_owned()));
        }
        "csrutil" if args.contains(&"disable") => {
            out.push(Threat::new("protection_off", true, "turns off a macOS protection (System Integrity Protection)".to_owned()));
        }
        "socketfilterfw" if args.windows(2).any(|pair| pair[0] == "--setglobalstate" && pair[1] == "off") => {
            out.push(Threat::new("protection_off", true, "turns off a macOS protection (the firewall)".to_owned()));
        }
        "defaults" if args.contains(&"com.apple.LaunchServices") && args.contains(&"LSQuarantine") => {
            out.push(Threat::new("protection_off", true, "turns off a macOS protection (the check of downloaded files)".to_owned()));
        }
        "xattr"
            if {
                let short = |letter: char| args.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(letter));
                short('c') || (short('d') && args.contains(&"com.apple.quarantine"))
            } =>
        {
            out.push(Threat::new("quarantine_removed", false, "removes macOS's mark on a downloaded file, so it opens without the download check".to_owned()));
        }
        _ => {}
    }
    if DOWNLOADERS.contains(&program) || matches!(program, "scp" | "sftp" | "rsync") {
        uploads(program, &args, out);
    }
    for arg in &args {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            let host = host_of(arg);
            if !host.is_ascii() || host.split('.').any(|label| label.starts_with("xn--")) {
                out.push(Threat::new("lookalike_host", false, format!("talks to {host}, a name written to look like another")));
            }
        }
    }
    // `nc host 9000 < ~/.aws/credentials`
    if NETWORK.contains(&program)
        && let Some(parent) = node.parent()
        && parent.kind() == "redirected_statement"
    {
        let mut cursor = parent.walk();
        for child in parent.named_children(&mut cursor) {
            if child.kind() == "file_redirect"
                && child.utf8_text(source).is_ok_and(|text| text.trim_start().starts_with('<'))
                && let Some(target) = child.child_by_field_name("destination").and_then(|d| literal(d, source))
                && credential_file(&target)
            {
                out.push(Threat::new("secrets_to_network", true, format!("sends a credential file ({target}) to another machine")));
            }
        }
    }
}

fn keychain(args: &[&str], out: &mut Vec<Threat>) {
    let Some(sub) = args.first() else { return };
    let shows_secret = args.iter().skip(1).any(|a| a.starts_with('-') && !a.starts_with("--") && (a.contains('w') || a.contains('g')));
    match *sub {
        "dump-keychain" | "export" => out.push(Threat::new("keychain", true, "copies passwords and keys out of the keychain".to_owned())),
        "find-generic-password" | "find-internet-password" if shows_secret => {
            out.push(Threat::new("keychain", true, "reads a password out of the keychain".to_owned()));
        }
        _ => {}
    }
}

/// `curl -d @~/.ssh/id_rsa`, `curl -F f=@.env`, `curl -T creds`, `wget --post-file=...`,
/// `scp ~/.aws/credentials host:`.
fn uploads(program: &str, args: &[&str], out: &mut Vec<Threat>) {
    let mut found: Option<String> = None;
    let mut previous = "";
    let copies = matches!(program, "scp" | "sftp" | "rsync");
    for arg in args {
        // `scp -i ~/.ssh/id_ed25519 ...`: the key signs in, it is not sent.
        if copies && matches!(previous, "-i" | "-F" | "-o" | "-e" | "-S" | "-J") {
            previous = *arg;
            continue;
        }
        let candidate = if let Some(rest) = arg.split_once("=@").map(|(_, file)| file) {
            Some(rest)
        } else if let Some(rest) = arg.strip_prefix('@') {
            Some(rest)
        } else if let Some(rest) = arg.strip_prefix("--post-file=").or_else(|| arg.strip_prefix("--body-file=")).or_else(|| arg.strip_prefix("--upload-file=")) {
            Some(rest)
        } else if matches!(previous, "-T" | "--upload-file" | "--post-file" | "--body-file") || (copies && !arg.starts_with('-') && !remote_spec(arg)) {
            Some(*arg)
        } else if let Some(rest) = arg.strip_prefix("-d@").or_else(|| arg.strip_prefix("-F")) {
            Some(rest.split_once("=@").map(|(_, file)| file).unwrap_or(rest))
        } else {
            None
        };
        if let Some(file) = candidate
            && credential_file(file)
        {
            found = Some(file.to_owned());
            break;
        }
        previous = *arg;
    }
    let Some(file) = found else { return };
    if copies && !args.iter().any(|a| remote_spec(a)) {
        return;
    }
    out.push(Threat::new("secrets_to_network", true, format!("sends a credential file ({file}) to another machine")));
}

/// `host:path` or `user@host:path` of scp/rsync, not a local path.
fn remote_spec(arg: &str) -> bool {
    if arg.starts_with('/') || arg.starts_with('.') || arg.starts_with('~') || arg.starts_with('-') {
        return false;
    }
    match arg.split_once(':') {
        Some((host, _)) => {
            let host = host.rsplit('@').next().unwrap_or(host);
            !host.is_empty() && !crate::approval::is_local_host(host)
        }
        None => false,
    }
}

/// A file that holds credentials: SSH keys, cloud and registry tokens, `.env` files, keychains,
/// browser login stores.
pub fn credential_file(path: &str) -> bool {
    let path = path.trim_matches(|c: char| c == '"' || c == '\'');
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
    let lower = path.to_ascii_lowercase();
    if name.starts_with("id_") && !name.ends_with(".pub") && (lower.contains(".ssh/") || !lower.contains('/')) {
        return true;
    }
    if name == ".env" || (name.starts_with(".env.") && !["example", "sample", "template", "dist", "defaults"].iter().any(|s| name.contains(s))) {
        return true;
    }
    const NAMES: &[&str] = &[".netrc", ".npmrc", ".pypirc", ".git-credentials", "credentials.json", "login.keychain-db", "Login Data", "Cookies", "key4.db", "logins.json"];
    if NAMES.contains(&name) {
        return true;
    }
    const SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keychain", ".keychain-db"];
    if SUFFIXES.iter().any(|suffix| lower.ends_with(suffix)) {
        return true;
    }
    const PLACES: &[&str] = &[".aws/credentials", ".docker/config.json", ".kube/config", ".config/gh/hosts.yml", ".config/gcloud/", ".azure/"];
    PLACES.iter().any(|place| lower.contains(place))
}

fn redirect(node: Node, source: &[u8], out: &mut Vec<Threat>) {
    if let Some(target) = node.child_by_field_name("destination").and_then(|d| literal(d, source))
        && (target.starts_with("/dev/tcp/") || target.starts_with("/dev/udp/"))
    {
        out.push(Threat::new("reverse_shell", true, "opens a shell that another machine controls".to_owned()));
    }
}

// ---------------------------------------------------------------------------------------------
// invisible characters

/// The name of an invisible or direction-changing character, None for any other.
fn invisible_name(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{200B}' => "zero-width space",
        '\u{200C}' => "zero-width non-joiner",
        '\u{200D}' => "zero-width joiner",
        '\u{2060}' => "word joiner",
        '\u{FEFF}' => "zero-width no-break space",
        '\u{200E}' => "left-to-right mark",
        '\u{200F}' => "right-to-left mark",
        '\u{202A}' => "left-to-right embedding",
        '\u{202B}' => "right-to-left embedding",
        '\u{202C}' => "pop directional formatting",
        '\u{202D}' => "left-to-right override",
        '\u{202E}' => "right-to-left override",
        '\u{2066}' => "left-to-right isolate",
        '\u{2067}' => "right-to-left isolate",
        '\u{2068}' => "first strong isolate",
        '\u{2069}' => "pop directional isolate",
        '\u{E0000}'..='\u{E007F}' => "tag character",
        _ => return None,
    })
}

/// Whether the character at `index` is a hidden one, not part of an ordinary emoji: a zero-width
/// joiner between two emoji (`👩‍💻`) and tag characters after a black flag (subdivision flags) are
/// how emoji are written; the variation selector at the start of a file (BOM) is a file marker.
fn hidden_at(chars: &[char], index: usize) -> bool {
    let c = chars[index];
    if invisible_name(c).is_none() {
        return false;
    }
    let emoji = |c: Option<&char>| c.is_some_and(|c| (*c as u32) >= 0x2000 && !c.is_ascii() && invisible_name(*c).is_none());
    match c {
        '\u{200D}' => !(index > 0 && emoji(chars.get(index - 1)) && emoji(chars.get(index + 1))),
        '\u{FEFF}' => index != 0,
        '\u{E0000}'..='\u{E007F}' => {
            let start = chars[..index].iter().rposition(|c| !('\u{E0000}'..='\u{E007F}').contains(c));
            !start.is_some_and(|start| chars[start] == '\u{1F3F4}')
        }
        _ => true,
    }
}

fn hidden_text(script: &str, out: &mut Vec<Threat>) {
    let mut found: Vec<String> = Vec::new();
    let chars: Vec<char> = script.chars().collect();
    for (index, &c) in chars.iter().enumerate() {
        if hidden_at(&chars, index) {
            let code = format!("U+{:04X}", c as u32);
            if !found.contains(&code) {
                found.push(code);
            }
        }
    }
    if !found.is_empty() {
        out.push(Threat::new("hidden_text", true, format!("holds invisible characters ({}), so what runs is not what it shows", found.join(", "))));
    }
}

static LINE_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d+)\s*[#:|│ ]").expect("line number pattern"));

/// One line naming where text holds invisible or direction-changing characters, None when it holds
/// none. Lines are named by the number the text shows for them (a read's anchors), else counted
/// from `first_line`.
pub fn invisible_note(text: &str, first_line: usize) -> Option<String> {
    let mut spots: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (index, line) in text.lines().enumerate() {
        let names: Vec<String> = {
            let mut names: Vec<String> = Vec::new();
            let chars: Vec<char> = line.chars().collect();
            for (at, &c) in chars.iter().enumerate() {
                if !hidden_at(&chars, at) {
                    continue;
                }
                if let Some(name) = invisible_name(c) {
                    let named = format!("U+{:04X} {name}", c as u32);
                    if !names.contains(&named) {
                        names.push(named);
                    }
                }
            }
            names
        };
        if names.is_empty() {
            continue;
        }
        total += 1;
        if spots.len() < 5 {
            let number = LINE_NUMBER.captures(line).and_then(|c| c[1].parse::<usize>().ok()).unwrap_or(first_line + index);
            spots.push(format!("line {number} ({})", names.join(", ")));
        }
    }
    if spots.is_empty() {
        return None;
    }
    let more = if total > spots.len() { format!(" and {} more lines", total - spots.len()) } else { String::new() };
    Some(format!(
        "[invisible characters: {}{more}. Code with them can do something other than what it shows; check those lines before relying on them]",
        spots.join(", ")
    ))
}

/// The invisible characters `new` has that `old` did not, as one note for an edit's result.
pub fn added_invisible(old: &str, new: &str) -> Option<String> {
    let count = |text: &str| {
        let chars: Vec<char> = text.chars().collect();
        (0..chars.len()).filter(|&at| hidden_at(&chars, at)).count()
    };
    if count(new) <= count(old) {
        return None;
    }
    let before: std::collections::HashSet<&str> = old.lines().collect();
    let added: String = new.lines().enumerate().filter(|(_, line)| !before.contains(line)).map(|(i, line)| format!("{} {line}\n", i + 1)).collect();
    invisible_note(&added, 1).map(|note| note.replacen("[invisible characters:", "[this edit wrote invisible characters:", 1))
}

// ---------------------------------------------------------------------------------------------
// secret values

/// What a masked value reads as; an edit whose written lines hold it is refused (it would write the
/// mask over the real value).
pub const MASK: &str = "…(masked ";

static SECRETS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let patterns: &[(&str, &str)] = &[
        ("AWS key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        ("GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{50,})\b"),
        ("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}\b"),
        ("Slack token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}"),
        ("Stripe key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{20,}\b"),
        ("Anthropic key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
        ("OpenAI key", r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{32,}"),
        ("Google API key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
        ("npm token", r"\bnpm_[A-Za-z0-9]{36}\b"),
        ("PyPI token", r"\bpypi-[A-Za-z0-9_-]{50,}"),
        ("JWT", r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
    ];
    patterns.iter().filter_map(|(kind, pattern)| Regex::new(pattern).ok().map(|re| (*kind, re))).collect()
});
static URL_PASSWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([a-zA-Z][a-zA-Z0-9+.-]*://[^/\s:@]+:)([^@\s/]{3,})@").expect("url password pattern"));
static KEY_BODY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/=]{16,}").expect("key body pattern"));

fn masked(value: &str, kind: &str) -> String {
    let head: String = value.chars().take(4).collect();
    format!("{head}{MASK}{kind})")
}

/// Text with secret values masked line by line (the lines and their count stay, so a read's anchors
/// still match), and how many were masked.
pub fn mask_secrets(text: &str) -> (String, usize) {
    if !might_hold_secret(text) {
        return (text.to_owned(), 0);
    }
    let mut count = 0usize;
    let mut in_key = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut line = line.to_owned();
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
            in_key = true;
            out.push_str(&line);
            continue;
        }
        if in_key {
            if line.contains("-----END") {
                in_key = false;
            } else {
                let replaced = KEY_BODY.replace_all(&line, "…(masked private key)").into_owned();
                if replaced != line {
                    count += 1;
                    line = replaced;
                }
            }
            out.push_str(&line);
            continue;
        }
        for (kind, re) in SECRETS.iter() {
            if re.is_match(&line) {
                let replaced = re.replace_all(&line, |caps: &regex::Captures| masked(&caps[0], kind)).into_owned();
                count += re.find_iter(&line).count();
                line = replaced;
            }
        }
        if URL_PASSWORD.is_match(&line) {
            count += URL_PASSWORD.find_iter(&line).count();
            line = URL_PASSWORD.replace_all(&line, |caps: &regex::Captures| format!("{}{}@", &caps[1], masked(&caps[2], "password"))).into_owned();
        }
        out.push_str(&line);
    }
    (out, count)
}

/// A cheap first look: most output holds none of the patterns' fixed parts.
fn might_hold_secret(text: &str) -> bool {
    const MARKS: &[&str] = &["PRIVATE KEY", "AKIA", "ASIA", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "glpat-", "xox", "_live_", "sk-", "AIza", "npm_", "pypi-", "eyJ", "://"];
    MARKS.iter().any(|mark| text.contains(mark))
}

/// The note a result carries when values were masked.
pub fn masked_note(count: usize) -> String {
    let what = if count == 1 { "1 secret value is" } else { "secret values are" };
    format!("[{what} shown masked (`…(masked <kind>)`). Leave lines holding one as they are: an edit cannot write a masked value]")
}

// ---------------------------------------------------------------------------------------------
// installs

/// A package an install command adds, or a package runner (`npx`) downloads and runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// OSV's name: npm, PyPI, crates.io, Go, RubyGems, Packagist.
    pub ecosystem: &'static str,
    pub name: String,
    /// The exact version asked for, if any.
    pub version: Option<String>,
    /// The command names its own package index: only that index knows the name.
    pub private: bool,
    /// A runner (`npx`, `uvx`) runs it: a program already installed in the project goes first.
    pub runs: bool,
}

static EXACT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^v?\d+(\.\d+){0,3}([-+][0-9A-Za-z.+-]+)?$").expect("version pattern"));
static NPM_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(@[a-zA-Z0-9][a-zA-Z0-9._~-]*/)?[a-zA-Z0-9~][a-zA-Z0-9._~-]*$").expect("npm name pattern"));
static PY_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$").expect("python name pattern"));
static CRATE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$").expect("crate name pattern"));
static GEM_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*$").expect("gem name pattern"));
static COMPOSER_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]([_.-]?[a-z0-9]+)*/[a-z0-9](([_.]|-{1,2})?[a-z0-9]+)*$").expect("composer name pattern"));

fn exact(version: &str) -> Option<String> {
    EXACT.is_match(version).then(|| version.trim_start_matches('v').to_owned())
}

/// The packages a command installs or runs, from its parsed words. A path, a URL, a git source or
/// a requirements file adds nothing here (they are not names a registry answers for).
pub fn installs(command: &str) -> Vec<Install> {
    let mut out = Vec::new();
    for words in crate::approval::simple_commands(command) {
        let words = real_program(words);
        install_specs(&words, &mut out);
    }
    out.dedup();
    out
}

/// The non-option arguments, skipping each option's value; and whether one of `private` was given.
fn positionals<'a>(args: &[&'a str], takes_value: &[&str], private: &[&str]) -> (Vec<&'a str>, bool) {
    let mut out = Vec::new();
    let mut is_private = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if arg == "--" {
            out.extend(args[index..].iter().copied());
            break;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let name = arg.split_once('=').map(|(name, _)| name).unwrap_or(arg);
            if private.contains(&name) {
                is_private = true;
            }
            if !arg.contains('=') && takes_value.contains(&name) {
                index += 1;
            }
            continue;
        }
        out.push(arg);
    }
    (out, is_private)
}

fn install_specs(words: &[Option<String>], out: &mut Vec<Install>) {
    let Some(program) = program_of(words) else { return };
    // A word only known at run time is not a name to check.
    let args: Vec<&str> = words.iter().skip(1).map(|word| word.as_deref().unwrap_or("\u{0}")).collect();
    let rest = |n: usize| args.iter().skip(n).copied().collect::<Vec<&str>>();
    const NPM_VALUES: &[&str] = &["--registry", "--prefix", "-w", "--workspace", "--tag", "--omit", "--include", "--cache", "--userconfig", "-C", "--dir", "--cwd", "--filter", "-F"];
    const NPM_PRIVATE: &[&str] = &["--registry", "--userconfig"];
    // pnpm's `-w` / `--workspace` are switches (the root, a workspace package), not options with values.
    const PNPM_VALUES: &[&str] = &["--registry", "--prefix", "--tag", "--omit", "--include", "--cache", "--userconfig", "-C", "--dir", "--filter", "-F"];
    const PY_VALUES: &[&str] = &[
        "-r", "--requirement", "-c", "--constraint", "-e", "--editable", "-i", "--index-url", "--extra-index-url", "--index", "--default-index", "-f",
        "--find-links", "-t", "--target", "--prefix", "--root", "--python", "-p", "--group", "--optional", "-G", "--source", "--platform", "--extra",
        "--with", "--from", "--spec", "--directory", "--project", "--cache-dir",
    ];
    const NPX_VALUES: &[&str] = &[
        "--registry", "--prefix", "-w", "--workspace", "--tag", "--omit", "--include", "--cache", "--userconfig", "-C", "--dir", "--cwd", "--filter", "-F", "-p",
        "--package", "-c", "--call",
    ];
    const PY_PRIVATE: &[&str] = &["-i", "--index-url", "--extra-index-url", "--index", "--default-index", "-f", "--find-links", "--source"];
    match program {
        "npm" => match args.first().copied() {
            Some("install" | "i" | "add" | "in" | "isntall") => npm(&rest(1), NPM_VALUES, NPM_PRIVATE, false, out),
            Some("exec" | "x") => npm(&rest(1), NPX_VALUES, NPM_PRIVATE, true, out),
            _ => {}
        },
        "pnpm" => match args.first().copied() {
            Some("add" | "install" | "i") => npm(&rest(1), PNPM_VALUES, NPM_PRIVATE, false, out),
            Some("dlx") => npm(&rest(1), PNPM_VALUES, NPM_PRIVATE, true, out),
            _ => {}
        },
        "yarn" => match (args.first().copied(), args.get(1).copied()) {
            (Some("add"), _) => npm(&rest(1), NPM_VALUES, NPM_PRIVATE, false, out),
            (Some("global"), Some("add")) => npm(&rest(2), NPM_VALUES, NPM_PRIVATE, false, out),
            (Some("dlx"), _) => npm(&rest(1), NPM_VALUES, NPM_PRIVATE, true, out),
            _ => {}
        },
        "bun" => match args.first().copied() {
            Some("add" | "install" | "i") => npm(&rest(1), NPM_VALUES, NPM_PRIVATE, false, out),
            Some("x") => npm(&rest(1), NPM_VALUES, NPM_PRIVATE, true, out),
            _ => {}
        },
        "npx" | "bunx" => npm(&args, NPX_VALUES, NPM_PRIVATE, true, out),
        "pip" => {
            if args.first() == Some(&"install") {
                python(&rest(1), PY_VALUES, PY_PRIVATE, false, out);
            }
        }
        "python" => {
            if args.first() == Some(&"-m") && args.get(1).is_some_and(|m| *m == "pip" || *m == "pip3") && args.get(2) == Some(&"install") {
                python(&rest(3), PY_VALUES, PY_PRIVATE, false, out);
            }
        }
        "uv" => match (args.first().copied(), args.get(1).copied()) {
            (Some("pip"), Some("install")) | (Some("tool"), Some("install")) => python(&rest(2), PY_VALUES, PY_PRIVATE, false, out),
            (Some("tool"), Some("run")) => python(&rest(2), PY_VALUES, PY_PRIVATE, true, out),
            (Some("add"), _) => python(&rest(1), PY_VALUES, PY_PRIVATE, false, out),
            _ => {}
        },
        "uvx" => python(&args, PY_VALUES, PY_PRIVATE, true, out),
        "pipx" => match args.first().copied() {
            Some("install") => python(&rest(1), PY_VALUES, PY_PRIVATE, false, out),
            Some("run") => python(&rest(1), PY_VALUES, PY_PRIVATE, true, out),
            _ => {}
        },
        "poetry" | "pdm" => {
            if args.first() == Some(&"add") {
                python(&rest(1), PY_VALUES, PY_PRIVATE, false, out);
            }
        }
        "pipenv" => {
            if args.first() == Some(&"install") {
                python(&rest(1), PY_VALUES, PY_PRIVATE, false, out);
            }
        }
        "cargo" => match args.first().copied() {
            Some("add") => cargo(&rest(1), false, out),
            Some("install") => cargo(&rest(1), true, out),
            _ => {}
        },
        "go" => {
            if matches!(args.first().copied(), Some("get" | "install")) {
                go(&rest(1), out);
            }
        }
        "gem" => {
            if args.first() == Some(&"install") {
                gem(&rest(1), out);
            }
        }
        "composer" => {
            let at = match (args.first().copied(), args.get(1).copied()) {
                (Some("require"), _) => Some(1),
                (Some("global"), Some("require")) => Some(2),
                _ => None,
            };
            if let Some(at) = at {
                composer(&rest(at), out);
            }
        }
        _ => {}
    }
}

/// `name`, `name@1.2.3`, `@scope/name@^2`; a path, URL, git or alias spec is None.
fn npm_spec(spec: &str) -> Option<(String, Option<String>)> {
    if spec.contains('\u{0}') || spec.starts_with(['.', '/', '~']) || spec.contains(':') || spec.ends_with(".tgz") || spec.ends_with(".tar.gz") {
        return None;
    }
    let (name, version) = match spec.strip_prefix('@') {
        Some(rest) => match rest.find('@') {
            Some(i) => (format!("@{}", &rest[..i]), Some(&rest[i + 1..])),
            None => (spec.to_owned(), None),
        },
        None => match spec.find('@') {
            Some(i) => (spec[..i].to_owned(), Some(&spec[i + 1..])),
            None => (spec.to_owned(), None),
        },
    };
    // `user/repo` is a GitHub shorthand, not a registry name.
    if !name.starts_with('@') && name.contains('/') {
        return None;
    }
    NPM_NAME.is_match(&name).then(|| (name, version.and_then(exact)))
}

fn npm(args: &[&str], takes_value: &[&str], private: &[&str], runs: bool, out: &mut Vec<Install>) {
    let (positional, is_private) = positionals(args, takes_value, private);
    // `npx pkg args...` runs the first; with `-p pkg` the packages are those, and the first word is
    // the program they give.
    let named = args.iter().any(|a| matches!(*a, "-p" | "--package") || a.starts_with("--package="));
    let mut specs: Vec<&str> = match (runs, named) {
        (true, true) => Vec::new(),
        (true, false) => positional.first().copied().into_iter().collect(),
        (false, _) => positional,
    };
    let mut index = 0;
    while index < args.len() {
        if matches!(args[index], "-p" | "--package") && index + 1 < args.len() {
            specs.push(args[index + 1]);
            index += 1;
        } else if let Some(value) = args[index].strip_prefix("--package=") {
            specs.push(value);
        }
        index += 1;
    }
    for spec in specs {
        if let Some((name, version)) = npm_spec(spec) {
            out.push(Install { ecosystem: "npm", name, version, private: is_private, runs });
        }
    }
}

/// `name`, `name==1.2`, `name[extra]>=2`, `name@1.2.3` (poetry); a path, URL or file is None.
fn python_spec(spec: &str) -> Option<(String, Option<String>)> {
    let spec = spec.trim();
    if spec.contains('\u{0}') || spec.starts_with(['.', '/', '~']) || spec.contains("://") || [".whl", ".tar.gz", ".zip", ".txt"].iter().any(|s| spec.ends_with(s)) {
        return None;
    }
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    let end = spec.find(|c: char| matches!(c, '=' | '~' | '>' | '<' | '!' | '@' | '[' | ' ' | ',')).unwrap_or(spec.len());
    let name = &spec[..end];
    if !PY_NAME.is_match(name) {
        return None;
    }
    let after = spec[end..].trim_start();
    let after = match after.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map(|(_, tail)| tail.trim_start()).unwrap_or(""),
        None => after,
    };
    let version = after.strip_prefix("===").or_else(|| after.strip_prefix("==")).or_else(|| after.strip_prefix('@')).map(str::trim).and_then(exact);
    let normalized = name.to_ascii_lowercase().replace(['_', '.'], "-");
    Some((normalized, version))
}

fn python(args: &[&str], takes_value: &[&str], private: &[&str], runs: bool, out: &mut Vec<Install>) {
    let (positional, is_private) = positionals(args, takes_value, private);
    let named = runs && args.iter().any(|a| matches!(*a, "--from" | "--spec"));
    let mut specs: Vec<&str> = match (runs, named) {
        (true, true) => Vec::new(),
        (true, false) => positional.first().copied().into_iter().collect(),
        (false, _) => positional,
    };
    // `uvx --from pkg tool`, `pipx run --spec pkg tool`: the package is the option's value.
    for (index, arg) in args.iter().enumerate() {
        if runs && matches!(*arg, "--from" | "--spec" | "--with") && index + 1 < args.len() {
            specs.push(args[index + 1]);
        }
    }
    for spec in specs {
        if let Some((name, version)) = python_spec(spec) {
            out.push(Install { ecosystem: "PyPI", name, version, private: is_private, runs });
        }
    }
}

fn cargo(args: &[&str], install: bool, out: &mut Vec<Install>) {
    const VALUES: &[&str] = &[
        "--vers", "--version", "--git", "--branch", "--tag", "--rev", "--path", "--registry", "-p", "--package", "--features", "-F", "--rename",
        "--target", "--manifest-path", "--root", "--index", "-j", "--jobs", "--profile", "--bin", "--example",
    ];
    if args.iter().any(|a| matches!(*a, "--git" | "--path") || a.starts_with("--git=") || a.starts_with("--path=")) {
        return;
    }
    let is_private = args.iter().any(|a| a.starts_with("--registry") || a.starts_with("--index"));
    let (positional, _) = positionals(args, VALUES, &[]);
    let flag_version = args
        .windows(2)
        .find(|pair| matches!(pair[0], "--vers" | "--version"))
        .map(|pair| pair[1])
        .or_else(|| args.iter().find_map(|a| a.strip_prefix("--version=").or_else(|| a.strip_prefix("--vers="))));
    let single = positional.len() == 1;
    for spec in positional {
        if spec.contains('\u{0}') {
            continue;
        }
        let (name, version) = match spec.split_once('@') {
            Some((name, version)) => (name, exact(version)),
            None => (spec, if single || install { flag_version.and_then(|v| exact(v.trim_start_matches('='))) } else { None }),
        };
        if CRATE_NAME.is_match(name) {
            out.push(Install { ecosystem: "crates.io", name: name.to_owned(), version, private: is_private, runs: false });
        }
    }
}

fn go(args: &[&str], out: &mut Vec<Install>) {
    let (positional, _) = positionals(args, &["-modfile", "-C"], &[]);
    for spec in positional {
        if spec.contains('\u{0}') || spec.starts_with('.') || spec.starts_with('/') {
            continue;
        }
        let (module, version) = match spec.split_once('@') {
            Some((_, "none")) => continue,
            Some((module, version)) => (module, exact(version)),
            None => (spec, None),
        };
        let module = module.trim_end_matches("/...");
        // A module path's first element has a dot (`github.com/x/y`); `fmt` and `./...` are not modules.
        if module.split('/').next().is_some_and(|first| first.contains('.')) {
            out.push(Install { ecosystem: "Go", name: module.to_owned(), version: version.map(|v| format!("v{v}")), private: false, runs: false });
        }
    }
}

fn gem(args: &[&str], out: &mut Vec<Install>) {
    const VALUES: &[&str] = &["-v", "--version", "-i", "--install-dir", "-s", "--source", "-n", "--bindir", "--platform"];
    let is_private = args.iter().any(|a| matches!(*a, "-s" | "--source") || a.starts_with("--source="));
    let (positional, _) = positionals(args, VALUES, &[]);
    let version = args.windows(2).find(|pair| matches!(pair[0], "-v" | "--version")).and_then(|pair| exact(pair[1]));
    let single = positional.len() == 1;
    for spec in positional {
        let (name, version) = match spec.split_once(':') {
            Some((name, version)) => (name, exact(version)),
            None => (spec, if single { version.clone() } else { None }),
        };
        if !name.ends_with(".gem") && GEM_NAME.is_match(name) {
            out.push(Install { ecosystem: "RubyGems", name: name.to_owned(), version, private: is_private, runs: false });
        }
    }
}

fn composer(args: &[&str], out: &mut Vec<Install>) {
    let (positional, _) = positionals(args, &["--working-dir", "-d"], &[]);
    let mut index = 0;
    while index < positional.len() {
        let spec = positional[index];
        index += 1;
        let (name, version) = match spec.split_once(':').or_else(|| spec.split_once('=')) {
            Some((name, version)) => (name, exact(version)),
            None => {
                // `composer require vendor/name 1.2.3`
                let next = positional.get(index).filter(|next| !next.contains('/')).copied();
                if next.is_some() {
                    index += 1;
                }
                (spec, next.and_then(exact))
            }
        };
        let name = name.to_ascii_lowercase();
        if COMPOSER_NAME.is_match(&name) {
            out.push(Install { ecosystem: "Packagist", name, version, private: false, runs: false });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// package checks

#[derive(Debug, Clone, Default)]
pub struct Vuln {
    pub id: String,
    pub severity: String,
    pub fixed: Vec<String>,
    pub kev: bool,
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub install: Install,
    /// Whether the registry has the name; None when it was not asked or did not answer.
    pub exists: Option<bool>,
    /// Ids of malicious-package records (`MAL-...`).
    pub malware: Vec<String>,
    /// Known vulnerabilities of the exact version asked for.
    pub vulns: Vec<Vuln>,
    /// Days since the name was first published, when the registry says.
    pub age_days: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Checked {
    pub verdicts: Vec<Verdict>,
    /// Why the packages were not checked (or only partly).
    pub unchecked: Option<String>,
}

/// How long the whole check may take before the command runs without it.
const CHECK_LIMIT: Duration = Duration::from_secs(10);
/// A registry answer larger than this is a long-published, much-used package: its age is not read.
const REGISTRY_BYTES: usize = 2 << 20;
/// A name first published this recently is said in a note.
const NEW_DAYS: u64 = 30;

/// Check the packages: through the CortexTerm on this Mac when it shares `terminal.guard`, else
/// OSV and the registries directly.
pub async fn check_packages(host: &dyn Host, installs: &[Install], session_id: &str, workspace: &Path) -> Checked {
    // A runner runs a program the project has installed before it downloads anything.
    let installs: Vec<Install> = installs.iter().filter(|install| !(install.runs && installed_here(workspace, install))).cloned().collect();
    if installs.is_empty() {
        return Checked::default();
    }
    let installs = &installs[..];
    let private_index = private_index(workspace);
    if let Some(mut checked) = guard_packages(host, installs, session_id).await {
        for verdict in &mut checked.verdicts {
            if private_index.contains(&verdict.install.ecosystem) {
                verdict.exists = None;
            }
        }
        return checked;
    }
    match tokio::time::timeout(CHECK_LIMIT, direct(installs, &private_index)).await {
        Ok(checked) => checked,
        Err(_) => Checked {
            verdicts: Vec::new(),
            unchecked: Some(format!("the package registries did not answer in {} s", CHECK_LIMIT.as_secs())),
        },
    }
}

/// A program of the project's own: `node_modules/.bin/<name>` or the venv's `bin/<name>`.
fn installed_here(workspace: &Path, install: &Install) -> bool {
    let program = install.name.rsplit('/').next().unwrap_or(&install.name);
    match install.ecosystem {
        "npm" => workspace.join("node_modules").join(".bin").join(program).exists(),
        "PyPI" => [".venv", "venv"].iter().any(|venv| workspace.join(venv).join("bin").join(program).exists()),
        _ => false,
    }
}

pub const GUARD: &str = "terminal.guard";
const GUARD_VERSION: u32 = 1;

/// How long the answer "does a CortexTerm share the guard" is reused (every command asks).
const GUARD_FRESH: Duration = Duration::from_secs(15);
static GUARD_CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<cc_service_kit::client::Provider>)>> = std::sync::Mutex::new(None);

async fn guard_provider(host: &dyn Host) -> Option<(cc_protocol::Ctx, cc_service_kit::client::Provider)> {
    let link = host.link()?;
    let cached = GUARD_CACHE.lock().ok().and_then(|cache| cache.clone());
    let provider = match cached {
        Some((at, known)) if at.elapsed() < GUARD_FRESH => known,
        _ => {
            let found = cc_service_kit::client::provider(&link, GUARD, GUARD_VERSION).await;
            if let Ok(mut cache) = GUARD_CACHE.lock() {
                *cache = Some((std::time::Instant::now(), found.clone()));
            }
            found
        }
    };
    Some((link, provider?))
}

fn asker(session_id: &str) -> cc_service_kit::client::Asker {
    cc_service_kit::client::Asker { session_id: session_id.to_owned(), by: "agent".to_owned() }
}

async fn guard_packages(host: &dyn Host, installs: &[Install], session_id: &str) -> Option<Checked> {
    let (link, provider) = guard_provider(host).await?;
    let packages: Vec<Value> = installs
        .iter()
        .map(|install| {
            let mut package = json!({"ecosystem": install.ecosystem, "name": install.name});
            if let Some(version) = &install.version {
                package["version"] = json!(version);
            }
            package
        })
        .collect();
    let answer =
        cc_service_kit::client::read(&link, &provider, "terminal.guard.packages.get", json!({"packages": packages}), &asker(session_id), CHECK_LIMIT).await.ok()?;
    let rows = answer.get("packages").and_then(Value::as_array)?;
    let mut verdicts = Vec::new();
    for install in installs {
        let Some(row) = rows.iter().find(|row| {
            row.get("name").and_then(Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case(&install.name))
                && row.get("ecosystem").and_then(Value::as_str) == Some(install.ecosystem)
        }) else {
            continue;
        };
        let strings = |key: &str| -> Vec<String> { row.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default() };
        let vulns = row
            .get("vulns")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|v| Vuln {
                        id: v.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        severity: v.get("severity").and_then(Value::as_str).unwrap_or("unknown").to_owned(),
                        fixed: v.get("fixed").and_then(Value::as_str).map(|f| vec![f.to_owned()]).unwrap_or_default(),
                        kev: v.get("kev").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        verdicts.push(Verdict {
            install: install.clone(),
            exists: if install.private { None } else { row.get("exists").and_then(Value::as_bool) },
            malware: strings("malware"),
            vulns,
            age_days: None,
        });
    }
    let unchecked = answer.get("unchecked").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    Some(Checked { verdicts, unchecked })
}

/// Ecosystems whose project or user config names a package index of its own: only that index knows
/// a name, so a public registry's "no such package" means nothing there.
fn private_index(workspace: &Path) -> Vec<&'static str> {
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
    let home = crate::util::expand_home("~");
    let mut out = Vec::new();
    let npmrc = format!("{}\n{}", read(&workspace.join(".npmrc")), read(&home.join(".npmrc")));
    // Its own registry, or a sign-in to npm's (private packages answer an anonymous ask "not found").
    if npmrc.lines().any(|line| {
        let key = line.trim_start().split('=').next().unwrap_or_default().trim();
        key.ends_with("registry") || key.ends_with("_authToken") || key.ends_with("_auth")
    }) {
        out.push("npm");
    }
    let pip = format!(
        "{}\n{}\n{}\n{}",
        read(&workspace.join("pyproject.toml")),
        read(&workspace.join("Pipfile")),
        read(&home.join(".config/pip/pip.conf")),
        read(&home.join(".pip/pip.conf"))
    );
    if pip.contains("index-url") || pip.contains("[[tool.uv.index]]") || pip.contains("[[tool.poetry.source]]") || pip.contains("[[source]]") || std::env::var_os("PIP_INDEX_URL").is_some() {
        out.push("PyPI");
    }
    let cargo = format!("{}\n{}", read(&workspace.join(".cargo/config.toml")), read(&home.join(".cargo/config.toml")));
    if cargo.contains("[registries") || cargo.contains("replace-with") {
        out.push("crates.io");
    }
    if std::env::var_os("GOPRIVATE").is_some() || std::env::var("GOPROXY").is_ok_and(|proxy| !proxy.contains("proxy.golang.org")) {
        out.push("Go");
    }
    out
}

fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent(concat!("CortexCode/", env!("CARGO_PKG_VERSION"), " (package check)"))
        .build()
        .ok()
}

async fn direct(installs: &[Install], private_index: &[&'static str]) -> Checked {
    let Some(client) = client() else {
        return Checked { verdicts: Vec::new(), unchecked: Some("no HTTPS client could be made".to_owned()) };
    };
    let mut verdicts: Vec<Verdict> = installs
        .iter()
        .map(|install| Verdict { install: install.clone(), exists: None, malware: Vec::new(), vulns: Vec::new(), age_days: None })
        .collect();
    let mut unchecked: Option<String> = None;
    // OSV: one batch for every package; malicious-package records have `MAL-` ids.
    let queries: Vec<Value> = installs
        .iter()
        .map(|install| {
            let mut query = json!({"package": {"name": install.name, "ecosystem": install.ecosystem}});
            if let Some(version) = &install.version {
                query["version"] = json!(version);
            }
            query
        })
        .collect();
    let mut details: Vec<(usize, String)> = Vec::new();
    match client.post("https://api.osv.dev/v1/querybatch").json(&json!({"queries": queries})).send().await {
        Ok(response) if response.status().is_success() => match response.json::<Value>().await {
            Ok(answer) => {
                let results = answer.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
                for (index, result) in results.iter().enumerate().take(verdicts.len()) {
                    for vuln in result.get("vulns").and_then(Value::as_array).into_iter().flatten() {
                        let Some(id) = vuln.get("id").and_then(Value::as_str) else { continue };
                        if id.starts_with("MAL-") {
                            verdicts[index].malware.push(id.to_owned());
                        } else if verdicts[index].install.version.is_some() {
                            details.push((index, id.to_owned()));
                        }
                    }
                }
            }
            Err(error) => unchecked = Some(format!("OSV's answer could not be read ({error})")),
        },
        Ok(response) => unchecked = Some(format!("OSV answered {}", response.status())),
        Err(error) => unchecked = Some(format!("OSV did not answer ({})", short_error(&error))),
    }
    // Details (severity, fixed versions) of the vulnerabilities of exact versions, and each name's
    // registry entry, asked at the same time.
    let mut tasks = tokio::task::JoinSet::new();
    for (index, id) in details.into_iter().take(24) {
        let client = client.clone();
        let package = verdicts[index].install.name.clone();
        tasks.spawn(async move { Lookup::Vuln(index, vuln_detail(&client, &id, &package).await) });
    }
    for (index, verdict) in verdicts.iter().enumerate() {
        if verdict.install.private || private_index.contains(&verdict.install.ecosystem) || !verdict.malware.is_empty() {
            continue;
        }
        let client = client.clone();
        let install = verdict.install.clone();
        tasks.spawn(async move { Lookup::Registry(index, registry(&client, &install).await) });
    }
    while let Some(done) = tasks.join_next().await {
        match done {
            Ok(Lookup::Vuln(index, Some(vuln))) => verdicts[index].vulns.push(vuln),
            Ok(Lookup::Registry(index, Some((exists, age)))) => {
                verdicts[index].exists = Some(exists);
                verdicts[index].age_days = age;
            }
            _ => {}
        }
    }
    Checked { verdicts, unchecked }
}

enum Lookup {
    Vuln(usize, Option<Vuln>),
    Registry(usize, Option<(bool, Option<u64>)>),
}

fn short_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "no connection".to_owned()
    } else {
        error.to_string().chars().take(120).collect()
    }
}

async fn vuln_detail(client: &reqwest::Client, id: &str, package: &str) -> Option<Vuln> {
    let answer: Value = client.get(format!("https://api.osv.dev/v1/vulns/{id}")).send().await.ok()?.json().await.ok()?;
    let severity = answer
        .pointer("/database_specific/severity")
        .and_then(Value::as_str)
        .map(|s| match s.to_ascii_lowercase().as_str() {
            "moderate" => "medium".to_owned(),
            other => other.to_owned(),
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let mut fixed: Vec<String> = Vec::new();
    for affected in answer.get("affected").and_then(Value::as_array).into_iter().flatten() {
        let name = affected.pointer("/package/name").and_then(Value::as_str).unwrap_or_default();
        if !name.eq_ignore_ascii_case(package) && name.to_ascii_lowercase().replace(['_', '.'], "-") != package {
            continue;
        }
        for range in affected.get("ranges").and_then(Value::as_array).into_iter().flatten() {
            for event in range.get("events").and_then(Value::as_array).into_iter().flatten() {
                if let Some(version) = event.get("fixed").and_then(Value::as_str)
                    && !fixed.iter().any(|f| f == version)
                {
                    fixed.push(version.to_owned());
                }
            }
        }
    }
    let cve = answer.get("aliases").and_then(Value::as_array).and_then(|aliases| aliases.iter().filter_map(Value::as_str).find(|a| a.starts_with("CVE-")).map(str::to_owned));
    let id = match cve {
        Some(cve) if cve != id => format!("{id} ({cve})"),
        _ => id.to_owned(),
    };
    Some(Vuln { id, severity, fixed, kev: false })
}

/// Whether the public registry has the name, and how many days ago it was first published.
async fn registry(client: &reqwest::Client, install: &Install) -> Option<(bool, Option<u64>)> {
    let name = &install.name;
    let url = match install.ecosystem {
        "npm" => format!("https://registry.npmjs.org/{}", name.replace('/', "%2f")),
        "PyPI" => format!("https://pypi.org/pypi/{name}/json"),
        "crates.io" => format!("https://crates.io/api/v1/crates/{name}"),
        "Go" => {
            // The module proxy's case encoding: an upper-case letter is `!` and its lower case.
            let escaped: String = name.chars().map(|c| if c.is_ascii_uppercase() { format!("!{}", c.to_ascii_lowercase()) } else { c.to_string() }).collect();
            format!("https://proxy.golang.org/{escaped}/@v/list")
        }
        "RubyGems" => format!("https://rubygems.org/api/v1/gems/{name}.json"),
        "Packagist" => format!("https://repo.packagist.org/p2/{name}.json"),
        _ => return None,
    };
    let mut response = client.get(&url).send().await.ok()?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
        // A scoped npm name may be a private package (npm answers "not found" to an anonymous ask);
        // a Go path may be a package inside a module (`golang.org/x/tools/cmd/goimports`), which
        // the proxy does not list. Neither "not found" means there is no such thing.
        if (install.ecosystem == "npm" && name.starts_with('@')) || install.ecosystem == "Go" {
            return None;
        }
        return Some((false, None));
    }
    if !status.is_success() {
        return None;
    }
    if !matches!(install.ecosystem, "npm" | "crates.io") {
        return Some((true, None));
    }
    let mut body: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        body.extend_from_slice(&chunk);
        if body.len() > REGISTRY_BYTES {
            return Some((true, None));
        }
    }
    let answer: Value = serde_json::from_slice(&body).ok()?;
    let created = match install.ecosystem {
        "npm" => answer.pointer("/time/created").and_then(Value::as_str),
        _ => answer.pointer("/crate/created_at").and_then(Value::as_str),
    };
    Some((true, created.and_then(days_since)))
}

/// Days from an RFC 3339 date (`2026-10-01T12:00:00.000Z`) to now.
fn days_since(date: &str) -> Option<u64> {
    let day = date.get(..10)?;
    let mut parts = day.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (parts.next()??, parts.next()??, parts.next()??);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let now = (crate::util::now_ms() / 86_400_000) as i64;
    u64::try_from(now - days).ok()
}

/// What the run tool does with a check: refuse (a malicious package, a name no registry has), or
/// run with notes.
pub enum Gate {
    Refuse(String),
    Notes(Vec<String>),
}

fn registry_name(ecosystem: &str) -> &'static str {
    match ecosystem {
        "npm" => "npm",
        "PyPI" => "PyPI",
        "crates.io" => "crates.io",
        "Go" => "the Go module proxy",
        "RubyGems" => "RubyGems",
        "Packagist" => "Packagist",
        _ => "its registry",
    }
}

pub fn gate(checked: &Checked) -> Gate {
    let mut notes = Vec::new();
    for verdict in &checked.verdicts {
        let install = &verdict.install;
        let name = &install.name;
        let registry = registry_name(install.ecosystem);
        if !verdict.malware.is_empty() {
            return Gate::Refuse(format!(
                "Not run: `{name}` on {registry} is a known malicious package ({}). If the user really wants it, they install it themselves. Use a different package for the task.",
                verdict.malware.join(", ")
            ));
        }
        if verdict.exists == Some(false) {
            return Gate::Refuse(format!(
                "Not run: {registry} has no package named `{name}`. Find the package's real name (the project's docs, or the import the code needs), then install that."
            ));
        }
        if let Some(days) = verdict.age_days.filter(|days| *days < NEW_DAYS) {
            let when = if days == 0 { "today".to_owned() } else if days == 1 { "1 day ago".to_owned() } else { format!("{days} days ago") };
            notes.push(format!("[packages: `{name}` was first published on {registry} {when}; make sure it is the package the task means]"));
        }
        if !verdict.vulns.is_empty()
            && let Some(version) = &install.version
        {
            let listed: Vec<String> = verdict
                .vulns
                .iter()
                .take(5)
                .map(|v| if v.kev { format!("{} ({}, exploited in the wild)", v.id, v.severity) } else { format!("{} ({})", v.id, v.severity) })
                .collect();
            let more = if verdict.vulns.len() > 5 { format!(" and {} more", verdict.vulns.len() - 5) } else { String::new() };
            let mut fixed: Vec<&str> = verdict.vulns.iter().flat_map(|v| v.fixed.iter().map(String::as_str)).collect();
            fixed.sort();
            fixed.dedup();
            let fixed = if fixed.is_empty() { String::new() } else { format!("; fixed in {}", fixed.iter().take(4).copied().collect::<Vec<_>>().join(", ")) };
            notes.push(format!(
                "[packages: {name}@{version} has known vulnerabilities: {}{more}{fixed}. Use a fixed version unless the project needs this one]",
                listed.join(", ")
            ));
        }
    }
    if let Some(why) = &checked.unchecked {
        notes.push(format!("[packages not checked for malware or vulnerabilities: {why}]"));
    }
    Gate::Notes(notes)
}

// ---------------------------------------------------------------------------------------------
// downloads piped into a program

/// The largest script fetched for review.
const SCRIPT_BYTES: usize = 1 << 20;

/// A script a command would have piped into a program, fetched (never run) for the model to read:
/// through CortexTerm's `terminal.guard.script.get` when it is here (its threat rules), else
/// directly. The text and what CortexTerm found in it.
pub async fn fetch_script(host: &dyn Host, url: &str, session_id: &str) -> Result<(String, Vec<String>), String> {
    if let Some((link, provider)) = guard_provider(host).await
        && let Ok(answer) =
            cc_service_kit::client::read(&link, &provider, "terminal.guard.script.get", json!({"url": url}), &asker(session_id), Duration::from_secs(20)).await
        && let Some(text) = answer.get("text").and_then(Value::as_str)
    {
        let found: Vec<String> = answer
            .get("threats")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|t| {
                        let why = t.get("why").and_then(Value::as_str)?;
                        Some(match t.get("line").and_then(Value::as_u64) {
                            Some(line) => format!("line {line}: {why}"),
                            None => why.to_owned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Ok((text.to_owned(), found));
    }
    let client = client().ok_or_else(|| "no HTTPS client could be made".to_owned())?;
    if !url.starts_with("https://") {
        return Err("it is not an https:// address, so what arrives cannot be trusted to be what the address names".to_owned());
    }
    let mut response = client.get(url).send().await.map_err(|error| short_error(&error))?;
    if !response.status().is_success() {
        return Err(format!("the server answered {}", response.status()));
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| short_error(&error))? {
        body.extend_from_slice(&chunk);
        if body.len() > SCRIPT_BYTES {
            return Err("it is larger than 1 MiB".to_owned());
        }
    }
    let text = String::from_utf8(body).map_err(|_| "it is not text (a program, not a script)".to_owned())?;
    Ok((text, Vec::new()))
}

/// What the host finds in a fetched script: its own threat rules over each line.
pub fn script_threats(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        for threat in threats(line).into_iter().filter(|t| t.high) {
            let entry = format!("line {}: {}", index + 1, threat.why);
            if !found.contains(&entry) {
                found.push(entry);
            }
        }
        if found.len() >= 8 {
            break;
        }
    }
    found
}

/// The threats CortexTerm's own rules find in a command, when it shares `terminal.guard`: added to
/// this host's (CortexTerm knows hosts and patterns this one does not).
pub async fn guard_command(host: &dyn Host, command: &str, cwd: &Path, session_id: &str) -> Vec<Threat> {
    let Some((link, provider)) = guard_provider(host).await else { return Vec::new() };
    let params = json!({"command": command, "cwd": cwd.display().to_string()});
    let Ok(answer) = cc_service_kit::client::read(&link, &provider, "terminal.guard.command.get", params, &asker(session_id), Duration::from_secs(3)).await else {
        return Vec::new();
    };
    answer
        .get("threats")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|t| {
                    let why = t.get("why").and_then(Value::as_str)?.trim().trim_end_matches('.').to_owned();
                    let high = t.get("severity").and_then(Value::as_str) == Some("high");
                    Some(Threat { rule: "cortexterm", high, why, url: None, runner: None })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The approval class of a threat: its rule and the command's digest, so the person's yes runs
/// this command once and no other.
pub fn threat_class(command: &str, rule: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(command.as_bytes());
    let short: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("threat:{rule}:{short}")
}

//! Beginner-friendly shell command tokenizer, built-in CLI/flag/operator guide,
//! and background model fallback for commands not in the built-in database.

use serde_json::{Value, json};
use superviser_core::secrets;

use crate::state::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Command,
    Subcommand,
    Flag,
    Operator,
    Arg,
}

impl TokenKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "Command",
            Self::Subcommand => "Subcommand",
            Self::Flag => "Flag",
            Self::Operator => "Shell operator",
            Self::Arg => "Argument",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExplanation {
    pub title: String,
    pub summary: String,
    pub details: Vec<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainedToken {
    pub text: String,
    pub kind: TokenKind,
    pub explanation: Option<CommandExplanation>,
    pub space_before: bool,
}

/// Tokenizes a shell command and attaches plain-English explanations to known commands,
/// subcommands, flags (including bundled short flags like `-la` or `-rf`), and operators.
/// Unknown commands are still tagged as `TokenKind::Command` (`explanation: None`) so the
/// UI can offer to ask the local model what they do.
pub fn parse_command(input: &str) -> Vec<ExplainedToken> {
    let raw_tokens = tokenize(input);
    let mut out = Vec::with_capacity(raw_tokens.len());

    let mut expect_cmd = true;
    let mut current_cmd: Option<String> = None;
    let mut seen_subcommand = false;

    for raw in raw_tokens {
        if raw.is_operator {
            let explanation = explain_operator(&raw.text);
            out.push(ExplainedToken {
                text: raw.text,
                kind: TokenKind::Operator,
                explanation,
                space_before: raw.space_before,
            });
            // Operators like `|`, `&&`, `||`, `;` start a new command; redirections keep args.
            if matches!(
                out.last().unwrap().text.as_str(),
                "|" | "&&" | "||" | ";" | "&"
            ) {
                expect_cmd = true;
                current_cmd = None;
                seen_subcommand = false;
            }
            continue;
        }

        if expect_cmd {
            // Flags passed to a wrapper like `sudo -u root ls` or `xargs -0 rm` belong to the
            // wrapper, not as a separate command name.
            if raw.text.starts_with('-') && raw.text.len() > 1 && current_cmd.is_some() {
                let explanation = current_cmd
                    .as_deref()
                    .and_then(|cmd| explain_flag(cmd, &raw.text));
                out.push(ExplainedToken {
                    text: raw.text,
                    kind: TokenKind::Flag,
                    explanation,
                    space_before: raw.space_before,
                });
                continue;
            }
            // Skip leading env assignments like `FOO=bar cargo test`.
            if is_env_assignment(&raw.text) {
                out.push(ExplainedToken {
                    text: raw.text,
                    kind: TokenKind::Arg,
                    explanation: None,
                    space_before: raw.space_before,
                });
                continue;
            }
            let base = command_basename(&raw.text);
            let explanation = explain_command(&base);
            let is_wrapper = matches!(
                base.as_str(),
                "sudo" | "env" | "time" | "command" | "nohup" | "nice" | "xargs"
            );
            out.push(ExplainedToken {
                text: raw.text,
                kind: TokenKind::Command,
                explanation,
                space_before: raw.space_before,
            });
            if is_wrapper {
                // Keep `expect_cmd = true` only after wrappers with no flags; if next is a flag,
                // handle it normally by setting current_cmd for the wrapper.
                current_cmd = Some(base);
                expect_cmd = true;
                seen_subcommand = false;
            } else {
                current_cmd = Some(base);
                expect_cmd = false;
                seen_subcommand = false;
            }
            continue;
        }

        // Inside a command's arguments
        if raw.text == "--" {
            out.push(ExplainedToken {
                text: raw.text,
                kind: TokenKind::Operator,
                explanation: Some(CommandExplanation {
                    title: "-- (end of options)".into(),
                    summary: "Tells the command that everything after this is a regular argument (like a file name), not a flag.".into(),
                    details: Vec::new(),
                    note: None,
                }),
                space_before: raw.space_before,
            });
            continue;
        }

        if raw.text.starts_with('-') && raw.text.len() > 1 {
            let explanation = current_cmd
                .as_deref()
                .and_then(|cmd| explain_flag(cmd, &raw.text));
            out.push(ExplainedToken {
                text: raw.text,
                kind: TokenKind::Flag,
                explanation,
                space_before: raw.space_before,
            });
            continue;
        }

        // First non-flag word after a command with known subcommands.
        if !seen_subcommand
            && let Some(cmd) = current_cmd.as_deref()
            && let Some(sub_exp) = explain_subcommand(cmd, &raw.text)
        {
            seen_subcommand = true;
            out.push(ExplainedToken {
                text: raw.text,
                kind: TokenKind::Subcommand,
                explanation: Some(sub_exp),
                space_before: raw.space_before,
            });
            continue;
        }

        out.push(ExplainedToken {
            text: raw.text,
            kind: TokenKind::Arg,
            explanation: None,
            space_before: raw.space_before,
        });
    }

    out
}

/// Asks the currently loaded model (in a separate, tool-free request) to explain a shell command.
pub async fn ask_model_to_explain(
    app: &App,
    command: &str,
    focus_token: Option<&str>,
    preferred_model_id: Option<&str>,
) -> anyhow::Result<String> {
    let guard_url = app
        .guard_url()
        .ok_or_else(|| anyhow::anyhow!("The guard isn't running."))?;
    let (model, _) = preferred_model_id
        .and_then(|id| app.loaded(id))
        .or_else(|| app.latest_model())
        .ok_or_else(|| anyhow::anyhow!("Load a model first to ask what this command means."))?;
    let api_key = secrets::local_api_key(&app.paths)?;
    let url = format!("{}/chat/completions", guard_url.trim_end_matches('/'));

    let user_prompt = match focus_token {
        Some(tok) if !tok.is_empty() && tok != command => format!(
            "Explain `{tok}` in this shell command:\n```\n{command}\n```"
        ),
        _ => format!("Explain what this shell command does:\n```\n{command}\n```"),
    };

    let body = json!({
        "model": model.alias,
        "messages": [
            {
                "role": "system",
                "content": "You explain shell commands to beginners in plain, clear English. Keep it concise (2-5 short sentences or bullets): explain what the command and its main flags or pipes do, and note whether it reads files, modifies files, or uses the internet. Do not ask follow-up questions."
            },
            {
                "role": "user",
                "content": user_prompt
            }
        ],
        "stream": false,
        "temperature": 0.2,
        "max_tokens": 320,
        "reasoning_effort": "none",
        "chat_template_kwargs": { "enable_thinking": false }
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let response = client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("Model returned {status}: {text}");
    }
    let payload: Value = response.json().await?;
    let content = payload["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    if content.is_empty() {
        anyhow::bail!("The model returned an empty explanation.");
    }
    Ok(content)
}

struct RawToken {
    text: String,
    is_operator: bool,
    space_before: bool,
}

fn tokenize(input: &str) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut saw_space = false;

    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            saw_space = true;
            i += 1;
            continue;
        }

        // Multi-char shell operators
        let rest = &input[i..];
        let op = if rest.starts_with("2>&1") {
            Some("2>&1")
        } else if rest.starts_with("&&") {
            Some("&&")
        } else if rest.starts_with("||") {
            Some("||")
        } else if rest.starts_with(">>") {
            Some(">>")
        } else if rest.starts_with("2>") {
            Some("2>")
        } else if matches!(bytes[i], b'|' | b';' | b'>' | b'<' | b'&') {
            Some(&input[i..i + 1])
        } else {
            None
        };

        if let Some(op_str) = op {
            tokens.push(RawToken {
                text: op_str.to_string(),
                is_operator: true,
                space_before: saw_space && !tokens.is_empty(),
            });
            i += op_str.len();
            saw_space = false;
            continue;
        }

        // Word token (preserving single/double quotes and backslash escapes)
        let start = i;
        let mut quote: Option<u8> = None;
        while i < bytes.len() {
            let b = bytes[i];
            if let Some(q) = quote {
                if q == b'"' && b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == q {
                    quote = None;
                }
                i += 1;
            } else if b == b'\'' || b == b'"' {
                quote = Some(b);
                i += 1;
            } else if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
            } else if b.is_ascii_whitespace()
                || matches!(b, b'|' | b';' | b'>' | b'<' | b'&')
                || input[i..].starts_with("2>&1")
                || input[i..].starts_with("2>")
            {
                break;
            } else {
                i += 1;
            }
        }
        tokens.push(RawToken {
            text: input[start..i].to_string(),
            is_operator: false,
            space_before: saw_space && !tokens.is_empty(),
        });
        saw_space = false;
    }

    tokens
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}

fn command_basename(token: &str) -> String {
    let unquoted = token.trim_matches(['"', '\'']);
    unquoted
        .rsplit('/')
        .next()
        .unwrap_or(unquoted)
        .to_string()
}

fn exp(title: &str, summary: &str, note: Option<&str>) -> CommandExplanation {
    CommandExplanation {
        title: title.into(),
        summary: summary.into(),
        details: Vec::new(),
        note: note.map(str::to_string),
    }
}

fn explain_operator(op: &str) -> Option<CommandExplanation> {
    Some(match op {
        "|" => exp(
            "| (pipe)",
            "Sends the text output of the command on the left directly as input to the command on the right.",
            None,
        ),
        "&&" => exp(
            "&& (and)",
            "Runs the next command only if the previous command succeeded (exit code 0).",
            None,
        ),
        "||" => exp(
            "|| (or)",
            "Runs the next command only if the previous command failed (non-zero exit code).",
            None,
        ),
        ";" => exp(
            "; (then)",
            "Runs the next command after the previous one finishes, whether it succeeded or failed.",
            None,
        ),
        ">" => exp(
            "> (write to file)",
            "Saves the command's output into a file, replacing anything already in that file.",
            Some("Creates or overwrites the target file."),
        ),
        ">>" => exp(
            ">> (append to file)",
            "Adds the command's output to the end of a file without erasing existing contents.",
            Some("Modifies or creates the target file."),
        ),
        "2>&1" => exp(
            "2>&1 (merge errors)",
            "Combines error messages (stderr) with normal output (stdout) so both go to the same place.",
            None,
        ),
        "2>" => exp(
            "2> (redirect errors)",
            "Sends error messages (stderr) to a file (often /dev/null to hide errors).",
            None,
        ),
        "<" => exp(
            "< (read from file)",
            "Feeds a file's contents into the command as its input.",
            None,
        ),
        "&" => exp(
            "& (background)",
            "Starts the command in the background without waiting for it to finish.",
            None,
        ),
        _ => return None,
    })
}

fn explain_command(cmd: &str) -> Option<CommandExplanation> {
    Some(match cmd {
        // --- Core user-highlighted CLIs ---
        "ls" => exp(
            "ls (list files)",
            "Lists the files and folders in a directory without changing anything.",
            None,
        ),
        "rg" => exp(
            "rg (ripgrep)",
            "Fast text search tool that looks for a word or pattern across files in the project (respecting .gitignore).",
            None,
        ),
        "gh" => exp(
            "gh (GitHub CLI)",
            "GitHub's official command-line tool for viewing or managing pull requests, issues, releases, and repositories.",
            Some("Talks to GitHub over the internet when network access is allowed."),
        ),
        "brew" => exp(
            "brew (Homebrew)",
            "macOS package manager used to inspect, install, or update command-line tools and apps.",
            Some("Install/upgrade subcommands download software and modify Homebrew directories."),
        ),

        // --- Version control & build tools ---
        "git" => exp(
            "git",
            "Version control tool for tracking code changes, inspecting history, switching branches, and syncing repositories.",
            None,
        ),
        "cargo" => exp(
            "cargo (Rust package manager)",
            "Builds, tests, lints, and manages dependencies for Rust projects.",
            None,
        ),
        "rustc" => exp("rustc", "The Rust compiler.", None),
        "rustup" => exp(
            "rustup",
            "Manages installed Rust toolchains and components.",
            None,
        ),
        "npm" => exp(
            "npm (Node Package Manager)",
            "Installs JavaScript packages and runs scripts defined in package.json.",
            None,
        ),
        "npx" => exp(
            "npx",
            "Runs a JavaScript package executable (downloading it temporarily if not installed locally).",
            None,
        ),
        "pnpm" => exp(
            "pnpm",
            "Fast JavaScript package manager and script runner (similar to npm).",
            None,
        ),
        "yarn" => exp(
            "yarn",
            "JavaScript package manager and script runner (similar to npm).",
            None,
        ),
        "bun" => exp(
            "bun",
            "Fast JavaScript/TypeScript runtime, test runner, and package manager.",
            None,
        ),
        "node" => exp("node", "Runs JavaScript code using Node.js.", None),
        "deno" => exp("deno", "Runs JavaScript/TypeScript code using Deno.", None),
        "python" | "python3" => exp(
            cmd,
            "Runs Python scripts or inline Python code.",
            None,
        ),
        "pip" | "pip3" => exp(
            cmd,
            "Installs or inspects Python packages.",
            Some("Install subcommands download packages from PyPI."),
        ),
        "uv" => exp(
            "uv",
            "Fast Python package manager, virtual environment manager, and script runner.",
            None,
        ),
        "pytest" => exp("pytest", "Runs Python automated tests.", None),
        "ruff" => exp("ruff", "Fast Python linter and code formatter.", None),
        "swift" => exp(
            "swift",
            "Builds, runs, or tests Swift packages and code.",
            None,
        ),
        "xcodebuild" => exp(
            "xcodebuild",
            "Builds and tests Xcode projects and workspaces from the command line.",
            None,
        ),
        "make" => exp(
            "make",
            "Runs build or automation targets defined in a Makefile.",
            None,
        ),
        "cmake" => exp(
            "cmake",
            "Configures and builds C/C++ projects.",
            None,
        ),
        "go" => exp("go", "Builds, runs, tests, and manages Go projects.", None),

        // --- Files, folders & reading ---
        "cd" => exp(
            "cd (change directory)",
            "Moves the shell's working directory to another folder.",
            None,
        ),
        "pwd" => exp(
            "pwd (print working directory)",
            "Prints the full path of the current folder.",
            None,
        ),
        "cat" => exp(
            "cat (concatenate)",
            "Prints the contents of one or more files to the output.",
            None,
        ),
        "head" => exp(
            "head",
            "Shows only the first few lines of a file or piped output (default: 10 lines).",
            None,
        ),
        "tail" => exp(
            "tail",
            "Shows only the last few lines of a file or piped output (default: 10 lines).",
            None,
        ),
        "find" => exp(
            "find",
            "Searches directories recursively for files matching a name, type, or size.",
            None,
        ),
        "fd" => exp(
            "fd",
            "Fast, user-friendly file finder (modern alternative to find).",
            None,
        ),
        "tree" => exp(
            "tree",
            "Displays folders and files as an indented directory tree.",
            None,
        ),
        "wc" => exp(
            "wc (word count)",
            "Counts lines, words, and characters in files or piped text.",
            None,
        ),
        "diff" => exp(
            "diff",
            "Shows the line-by-line differences between two files or folders.",
            None,
        ),
        "stat" => exp(
            "stat",
            "Shows detailed metadata about a file (size, permissions, timestamps).",
            None,
        ),
        "file" => exp(
            "file",
            "Identifies what type of file something is (e.g. text, PNG image, binary).",
            None,
        ),
        "du" => exp(
            "du (disk usage)",
            "Shows how much disk space files and folders take.",
            None,
        ),
        "df" => exp(
            "df (disk free)",
            "Shows available and used disk space on drives.",
            None,
        ),
        "mkdir" => exp(
            "mkdir (make directory)",
            "Creates one or more new folders.",
            Some("Creates folders on disk."),
        ),
        "touch" => exp(
            "touch",
            "Creates an empty file if it doesn't exist, or updates its modification timestamp.",
            Some("Creates or updates file timestamps."),
        ),
        "cp" => exp(
            "cp (copy)",
            "Copies files or folders to a new location.",
            Some("Writes copied files to the destination."),
        ),
        "mv" => exp(
            "mv (move / rename)",
            "Moves or renames files or folders.",
            Some("Renames or overwrites the destination path."),
        ),
        "rm" => exp(
            "rm (remove)",
            "Permanently deletes files or folders (does not go to macOS Trash).",
            Some("Deletes files permanently inside the allowed project folder."),
        ),
        "ln" => exp(
            "ln (link)",
            "Creates a link (usually a symlink with -s) pointing to another file or folder.",
            Some("Creates a link on disk."),
        ),
        "chmod" => exp(
            "chmod (change mode)",
            "Changes a file's permissions (for example, making a script executable with +x).",
            Some("Modifies file permissions."),
        ),

        // --- Text & data processing ---
        "grep" => exp(
            "grep",
            "Searches text or files for lines matching a pattern.",
            None,
        ),
        "sed" => exp(
            "sed (stream editor)",
            "Transforms or extracts text line by line (often used for search-and-replace).",
            Some("With -i, edits files in place."),
        ),
        "awk" => exp(
            "awk",
            "Extracts columns or transforms structured text line by line.",
            None,
        ),
        "sort" => exp("sort", "Sorts lines of text alphabetically or numerically.", None),
        "uniq" => exp(
            "uniq",
            "Filters out adjacent duplicate lines (usually used after sort).",
            None,
        ),
        "cut" => exp(
            "cut",
            "Extracts specific columns or character ranges from each line of text.",
            None,
        ),
        "tr" => exp(
            "tr (translate)",
            "Replaces or deletes specific characters in piped text.",
            None,
        ),
        "tee" => exp(
            "tee",
            "Prints piped text to the screen AND writes a copy to a file at the same time.",
            Some("Writes output to the named file."),
        ),
        "xargs" => exp(
            "xargs",
            "Takes lines from piped input and passes them as arguments to another command.",
            None,
        ),
        "jq" => exp(
            "jq",
            "Parses, filters, and formats JSON data from files or piped output.",
            None,
        ),
        "echo" => exp("echo", "Prints a line of text.", None),
        "printf" => exp("printf", "Prints formatted text.", None),

        // --- Network & archives ---
        "curl" => exp(
            "curl",
            "Makes HTTP/HTTPS requests to download files or talk to web APIs.",
            Some("Uses the network; the guard checks destination hosts."),
        ),
        "tar" => exp(
            "tar",
            "Packs files into or extracts files from .tar / .tar.gz archives.",
            None,
        ),
        "zip" => exp("zip", "Compresses files into a .zip archive.", None),
        "unzip" => exp("unzip", "Extracts files from a .zip archive.", None),

        // --- System, shell & macOS ---
        "which" => exp(
            "which",
            "Shows the full file path of the program that would run for a given command name.",
            None,
        ),
        "env" => exp(
            "env",
            "Prints environment variables or runs a command with modified environment variables.",
            None,
        ),
        "export" => exp(
            "export",
            "Sets an environment variable for subsequent commands in this shell.",
            None,
        ),
        "ps" => exp("ps", "Lists running processes.", None),
        "kill" => exp(
            "kill",
            "Sends a signal to stop or terminate a process by its PID.",
            Some("Stops a running process."),
        ),
        "lsof" => exp(
            "lsof (list open files)",
            "Shows open files or which processes are listening on network ports.",
            None,
        ),
        "sh" | "bash" | "zsh" => exp(
            cmd,
            "Runs a shell script or inline shell command string.",
            None,
        ),
        "sudo" => exp(
            "sudo",
            "Runs a command with administrator (root) privileges.",
            Some("High privilege — blocked inside the sandbox."),
        ),
        "docker" => exp(
            "docker",
            "Builds or runs software inside Docker containers.",
            Some("Blocked inside the macOS sandbox by default."),
        ),
        "ssh" => exp(
            "ssh",
            "Connects to a remote machine over Secure Shell.",
            Some("Accesses remote hosts over the network."),
        ),
        "open" => exp(
            "open (macOS)",
            "Opens a file, folder, or URL in its default macOS app.",
            Some("Blocked inside the macOS sandbox."),
        ),
        "shasum" => exp(
            "shasum",
            "Computes or verifies cryptographic SHA checksums of files.",
            None,
        ),
        "sqlite3" => exp(
            "sqlite3",
            "Runs SQL queries against a local SQLite database file.",
            None,
        ),
        "sleep" => exp("sleep", "Pauses execution for a given number of seconds.", None),
        "uname" => exp("uname", "Prints operating system and CPU architecture info.", None),
        "true" => exp("true", "Does nothing and always succeeds (often used to ignore an error after ||).", None),
        _ => return None,
    })
}

fn explain_subcommand(cmd: &str, sub: &str) -> Option<CommandExplanation> {
    Some(match (cmd, sub) {
        // git
        ("git", "status") => exp("git status", "Shows which files are modified, staged, or untracked.", None),
        ("git", "diff") => exp("git diff", "Shows line-by-line code changes that haven't been committed yet.", None),
        ("git", "log") => exp("git log", "Displays the commit history.", None),
        ("git", "add") => exp("git add", "Stages file changes to be included in the next commit.", Some("Updates the Git index.")),
        ("git", "commit") => exp("git commit", "Saves staged changes as a new commit in local history.", Some("Writes a commit to the local repo.")),
        ("git", "push") => exp("git push", "Uploads local commits to a remote repository (such as GitHub).", Some("Modifies the remote repository over the network.")),
        ("git", "pull") => exp("git pull", "Downloads and merges the latest changes from the remote repository.", Some("Modifies local branch and uses the network.")),
        ("git", "fetch") => exp("git fetch", "Downloads remote branch info without modifying your working files.", Some("Uses the network.")),
        ("git", "checkout" | "switch") => exp(&format!("git {sub}"), "Switches branches (or restores files).", Some("Changes files in your working tree.")),
        ("git", "branch") => exp("git branch", "Lists, creates, or deletes Git branches.", None),
        ("git", "stash") => exp("git stash", "Temporarily shelves uncommitted changes so you can come back to them later.", None),
        ("git", "show") => exp("git show", "Displays details and diff of a specific commit or file.", None),
        ("git", "reset") => exp("git reset", "Moves the current branch pointer or unstages files.", Some("With --hard, permanently discards uncommitted changes.")),
        ("git", "restore") => exp("git restore", "Reverts modified files back to their last committed state.", Some("Discards local edits to those files.")),
        ("git", "clone") => exp("git clone", "Downloads a copy of a Git repository into a new folder.", Some("Uses the network and creates files.")),
        ("git", "init") => exp("git init", "Initializes a new empty Git repository in the folder.", None),
        ("git", "worktree") => exp("git worktree", "Manages multiple checked-out folders sharing one Git repository.", None),

        // gh
        ("gh", "pr") => exp("gh pr", "Lists, views, creates, checks out, or merges GitHub pull requests.", None),
        ("gh", "issue") => exp("gh issue", "Lists, views, creates, or edits GitHub issues.", None),
        ("gh", "repo") => exp("gh repo", "Views, clones, or manages GitHub repositories.", None),
        ("gh", "run") => exp("gh run", "Inspects or re-runs GitHub Actions workflow runs.", None),
        ("gh", "release") => exp("gh release", "Views or manages GitHub releases.", None),
        ("gh", "api") => exp("gh api", "Makes an authenticated request directly to the GitHub REST/GraphQL API.", None),
        ("gh", "auth") => exp("gh auth", "Checks or configures GitHub CLI authentication status.", None),
        ("gh", "search") => exp("gh search", "Searches repositories, issues, PRs, or code on GitHub.", None),

        // brew
        ("brew", "install") => exp("brew install", "Downloads and installs a package or app via Homebrew.", Some("Downloads and installs software.")),
        ("brew", "uninstall" | "remove") => exp(&format!("brew {sub}"), "Uninstalls a Homebrew package.", Some("Removes an installed package.")),
        ("brew", "upgrade") => exp("brew upgrade", "Updates installed Homebrew packages to newer versions.", Some("Downloads and updates packages.")),
        ("brew", "update") => exp("brew update", "Refreshes Homebrew's local list of available package versions.", Some("Uses the network.")),
        ("brew", "list") => exp("brew list", "Lists all packages currently installed via Homebrew.", None),
        ("brew", "search") => exp("brew search", "Searches Homebrew for available packages matching a name.", None),
        ("brew", "info") => exp("brew info", "Displays details, version, and dependencies of a Homebrew package.", None),
        ("brew", "doctor") => exp("brew doctor", "Checks your Homebrew installation for common problems.", None),
        ("brew", "services") => exp("brew services", "Lists, starts, or stops background services managed by Homebrew.", None),

        // cargo
        ("cargo", "build") => exp("cargo build", "Compiles the Rust project into binaries/libraries.", None),
        ("cargo", "check") => exp("cargo check", "Quickly checks Rust code for compile errors without producing binaries.", None),
        ("cargo", "test") => exp("cargo test", "Compiles and runs the project's Rust automated tests.", None),
        ("cargo", "clippy") => exp("cargo clippy", "Runs Rust's linter to catch common mistakes and style issues.", None),
        ("cargo", "run") => exp("cargo run", "Compiles and runs the Rust project's binary.", None),
        ("cargo", "fmt") => exp("cargo fmt", "Formats Rust source files according to rustfmt style rules.", None),
        ("cargo", "doc") => exp("cargo doc", "Generates HTML documentation for the crate.", None),
        ("cargo", "add") => exp("cargo add", "Adds a dependency to Cargo.toml.", Some("Modifies Cargo.toml and Cargo.lock.")),
        ("cargo", "clean") => exp("cargo clean", "Deletes compiled build artifacts in target/.", None),

        // npm / pnpm / yarn / bun
        ("npm" | "pnpm" | "yarn" | "bun", "install" | "i") => exp(&format!("{cmd} {sub}"), "Installs the project's JavaScript dependencies from package.json.", Some("Downloads packages and updates node_modules.")),
        ("npm" | "pnpm" | "yarn" | "bun", "run") => exp(&format!("{cmd} run"), "Runs a named script from package.json.", None),
        ("npm" | "pnpm" | "yarn" | "bun", "test") => exp(&format!("{cmd} test"), "Runs the project's test script.", None),
        ("npm" | "pnpm" | "yarn" | "bun", "build") => exp(&format!("{cmd} build"), "Runs the project's build script.", None),

        _ => return None,
    })
}

fn explain_flag(cmd: &str, raw_flag: &str) -> Option<CommandExplanation> {
    // Strip `=value` for lookup (e.g. `--state=open` -> `--state`).
    let flag = raw_flag.split('=').next().unwrap_or(raw_flag);

    if let Some(desc) = lookup_single_flag(cmd, flag) {
        return Some(CommandExplanation {
            title: raw_flag.to_string(),
            summary: desc.to_string(),
            details: Vec::new(),
            note: flag_risk_note(cmd, flag).map(str::to_string),
        });
    }

    // Handle bundled short flags like `-la`, `-rf`, `-rn`, `-sb`.
    if !flag.starts_with("--") && flag.starts_with('-') && flag.len() > 2 && flag[1..].bytes().all(|b| b.is_ascii_alphanumeric()) {
        let mut details = Vec::new();
        let mut risk_notes = Vec::new();
        for ch in flag[1..].chars() {
            let single = format!("-{ch}");
            if let Some(desc) = lookup_single_flag(cmd, &single) {
                details.push(format!("{single}: {desc}"));
                if let Some(risk) = flag_risk_note(cmd, &single)
                    && !risk_notes.contains(&risk)
                {
                    risk_notes.push(risk);
                }
            }
        }
        if !details.is_empty() {
            return Some(CommandExplanation {
                title: format!("{raw_flag} (combined flags)"),
                summary: format!("Combines {} short options for `{cmd}`:", details.len()),
                details,
                note: (!risk_notes.is_empty()).then(|| risk_notes.join(" ")),
            });
        }
    }

    None
}

fn lookup_single_flag(cmd: &str, flag: &str) -> Option<&'static str> {
    let specific = match (cmd, flag) {
        // ls
        ("ls", "-l") => Some("Long format: shows permissions, owner, file size, and last modified date."),
        ("ls", "-a" | "--all") => Some("Shows hidden files and folders whose names start with a dot (like .gitignore)."),
        ("ls", "-A") => Some("Shows hidden files except the `.` and `..` directory entries."),
        ("ls", "-h" | "--human-readable") => Some("Displays file sizes in readable units like KB, MB, and GB."),
        ("ls", "-R" | "--recursive") => Some("Lists subfolders and their contents recursively."),
        ("ls", "-t") => Some("Sorts entries by modification time, newest first."),
        ("ls", "-r" | "--reverse") => Some("Reverses the sort order."),
        ("ls", "-S") => Some("Sorts entries by file size, largest first."),
        ("ls", "-1") => Some("Prints one file per line."),
        ("ls", "-d") => Some("Lists directories themselves instead of their contents."),

        // rg / grep
        ("rg" | "grep", "-i" | "--ignore-case") => Some("Case-insensitive search (matches both uppercase and lowercase)."),
        ("rg" | "grep", "-n" | "--line-number") => Some("Shows the line number for every matching line."),
        ("rg" | "grep", "-l" | "--files-with-matches") => Some("Prints only the file paths that contain a match, not the matching lines."),
        ("rg" | "grep", "-c" | "--count") => Some("Counts how many matching lines each file has."),
        ("rg" | "grep", "-w" | "--word-regexp") => Some("Matches only whole words, not substrings inside larger words."),
        ("rg" | "grep", "-F" | "--fixed-strings") => Some("Treats the pattern as literal text instead of a regular expression."),
        ("rg" | "grep", "-v" | "--invert-match") => Some("Shows lines that do NOT match the pattern."),
        ("rg" | "grep", "-A" | "--after-context") => Some("Shows N lines of context after each match."),
        ("rg" | "grep", "-B" | "--before-context") => Some("Shows N lines of context before each match."),
        ("rg" | "grep", "-C" | "--context") => Some("Shows N lines of context before and after each match."),
        ("rg" | "grep", "-E") => Some("Uses extended regular expression syntax."),
        ("rg" | "grep", "-r" | "-R") => Some("Searches recursively through subdirectories."),
        ("rg", "-g" | "--glob") => Some("Includes or excludes files matching a glob pattern (like `*.rs`)."),
        ("rg", "-t" | "--type") => Some("Searches only files of a specific language/type (like `rust` or `js`)."),
        ("rg", "-S" | "--smart-case") => Some("Case-insensitive unless the pattern includes an uppercase letter."),
        ("rg", "--hidden") => Some("Includes hidden dot-files and dot-folders in the search."),
        ("rg", "--no-ignore") => Some("Also searches files normally skipped by .gitignore."),
        ("rg", "--files") => Some("Lists all files ripgrep would search without searching their contents."),

        // gh
        ("gh", "--json") => Some("Outputs structured JSON containing the requested fields."),
        ("gh", "--jq" | "-q") => Some("Filters the JSON output using a jq expression."),
        ("gh", "-R" | "--repo") => Some("Targets a specific `owner/repo` instead of the current directory's repo."),
        ("gh", "-L" | "--limit") => Some("Maximum number of items to return."),
        ("gh", "--state") => Some("Filters items by state (e.g. open, closed, merged, all)."),
        ("gh", "--web" | "-w") => Some("Opens the item in your web browser."),
        ("gh", "--draft" | "-d") => Some("Marks a pull request as a draft."),
        ("gh", "--base" | "-B") => Some("Sets the target base branch for a pull request."),
        ("gh", "--head" | "-H") => Some("Sets the source branch for a pull request."),
        ("gh", "--title" | "-t") => Some("Sets the title of the PR or issue."),
        ("gh", "--body" | "-b") => Some("Sets the description body of the PR or issue."),

        // brew
        ("brew", "--cask") => Some("Targets macOS GUI apps (casks) rather than command-line formulae."),
        ("brew", "--formula") => Some("Targets command-line packages (formulae)."),
        ("brew", "--versions") => Some("Shows installed version numbers next to each package."),

        // git
        ("git", "-s" | "--short") => Some("Shows a compact, short-format summary."),
        ("git", "-b" | "--branch") => Some("Shows current branch info (or creates a new branch in checkout)."),
        ("git", "-m") => Some("Sets the commit message inline."),
        ("git", "-a" | "--all") => Some("Includes all tracked modified files (or all branches/tags)."),
        ("git", "-n") => Some("Limits the number of commits shown (e.g. `-n 10`)."),
        ("git", "--oneline") => Some("Shows each commit on a single compact line."),
        ("git", "--stat") => Some("Shows a summary of changed files and line counts."),
        ("git", "--staged" | "--cached") => Some("Inspects changes that are already staged for commit."),
        ("git", "-p" | "--patch") => Some("Shows full patch diffs (or stages interactively)."),
        ("git", "-f" | "--force") => Some("Forces the operation, overriding safety checks."),
        ("git", "--hard") => Some("Resets working files and index to match the target commit, discarding local edits."),
        ("git", "-d") => Some("Deletes a merged branch."),
        ("git", "-D") => Some("Force-deletes a branch even if it has unmerged commits."),

        // cargo
        ("cargo", "-p" | "--package") => Some("Runs only for the named workspace crate/package."),
        ("cargo", "--workspace") => Some("Runs across every crate in the Cargo workspace."),
        ("cargo", "--all-targets") => Some("Includes library, binaries, tests, examples, and benches."),
        ("cargo", "--release") => Some("Builds with full compiler optimizations (slower compile, faster binary)."),
        ("cargo", "--locked") => Some("Requires Cargo.lock to be up to date without changing dependency versions."),
        ("cargo", "--nocapture") => Some("Prints test stdout/stderr directly instead of hiding it for passing tests."),
        ("cargo", "-D") => Some("Turns the named compiler/clippy lints into hard errors (e.g. `-D warnings`)."),

        // rm / cp / mv / mkdir
        ("rm" | "cp", "-r" | "-R" | "--recursive") => Some("Applies recursively to folders and everything inside them."),
        ("rm" | "mv" | "cp", "-f" | "--force") => Some("Skips confirmation prompts and ignores missing files."),
        ("mkdir", "-p" | "--parents") => Some("Creates any missing parent folders and doesn't error if the folder already exists."),
        ("ln", "-s" | "--symbolic") => Some("Creates a symbolic link (shortcut) instead of a hard link."),

        // head / tail
        ("head" | "tail", "-n") => Some("Specifies how many lines to show (e.g. `-n 20`)."),
        ("tail", "-f" | "--follow") => Some("Keeps watching the file and streams new lines as they are appended."),

        // curl
        ("curl", "-s" | "--silent") => Some("Hides the progress meter and non-error messages."),
        ("curl", "-S" | "--show-error") => Some("Still shows an error message if the request fails while using -s."),
        ("curl", "-L" | "--location") => Some("Follows HTTP redirects automatically."),
        ("curl", "-f" | "--fail") => Some("Fails with an error code on HTTP 4xx/5xx responses instead of printing the error page."),
        ("curl", "-o" | "--output") => Some("Writes the downloaded response to the given file path."),
        ("curl", "-X" | "--request") => Some("Sets the HTTP method (e.g. GET, POST, PUT, DELETE)."),
        ("curl", "-H" | "--header") => Some("Adds a custom HTTP header to the request."),
        ("curl", "-d" | "--data") => Some("Sends data in the HTTP request body (typically with POST)."),
        ("curl", "-I" | "--head") => Some("Fetches only the HTTP response headers, not the body."),

        // find / wc / sed / tar / jq
        ("find", "-name") => Some("Matches files whose name matches the given pattern."),
        ("find", "-type") => Some("Filters by entry type (`f` for files, `d` for directories)."),
        ("find", "-maxdepth") => Some("Limits how many folder levels deep `find` searches."),
        ("wc", "-l") => Some("Counts lines only."),
        ("wc", "-w") => Some("Counts words only."),
        ("wc", "-c") => Some("Counts bytes only."),
        ("sed", "-i") => Some("Edits the file in place instead of printing to output."),
        ("sed", "-n") => Some("Suppresses automatic printing so only explicitly printed lines appear."),
        ("sed", "-e") => Some("Adds an editing expression to run."),
        ("jq", "-r" | "--raw-output") => Some("Prints string results without surrounding JSON quotes."),
        ("tar", "-x") => Some("Extracts files from an archive."),
        ("tar", "-c") => Some("Creates a new archive."),
        ("tar", "-z") => Some("Compresses or decompresses with gzip (.tar.gz)."),
        ("tar", "-f") => Some("Specifies the archive file name."),
        ("tar", "-v") => Some("Lists each file as it is processed."),
        ("tar", "-C") => Some("Changes to the given directory before extracting or packing."),

        // sort / uniq / du / df / sh / bash / zsh
        ("sort", "-r") => Some("Sorts in reverse order."),
        ("sort", "-n") => Some("Sorts numerically instead of alphabetically."),
        ("sort", "-u") => Some("Removes duplicate lines while sorting."),
        ("uniq", "-c") => Some("Prefixes each line with the number of times it occurred."),
        ("du" | "df", "-h") => Some("Shows sizes in human-readable units (KB, MB, GB)."),
        ("du", "-s") => Some("Shows only the total size for each argument rather than every subfolder."),
        ("sh" | "bash" | "zsh", "-c") => Some("Executes the next argument as a shell command string."),
        _ => None,
    };
    if specific.is_some() {
        return specific;
    }

    // Universal CLI conventions
    match flag {
        "-h" | "--help" => Some("Prints usage instructions and available options for this command."),
        "-V" | "--version" => Some("Prints the version number of this tool."),
        "-v" | "--verbose" => Some("Prints extra detailed output while running."),
        "-q" | "--quiet" => Some("Reduces output to only essential messages or errors."),
        "--dry-run" => Some("Previews what would happen without making actual changes."),
        "--json" => Some("Formats output as JSON."),
        "--force" => Some("Forces the action without asking for confirmation."),
        _ => None,
    }
}

fn flag_risk_note(cmd: &str, flag: &str) -> Option<&'static str> {
    match (cmd, flag) {
        ("rm", "-r" | "-R" | "--recursive") => Some("Deletes entire folders and everything inside them."),
        ("rm", "-f" | "--force") => Some("Skips confirmation prompts."),
        ("git", "-f" | "--force" | "--hard" | "-D") => Some("Can overwrite or discard Git history/edits."),
        ("sed", "-i") => Some("Modifies the file directly on disk."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_commands_flags_subcommands_and_pipes() {
        let tokens = parse_command("ls -la | rg -in \"hello world\" && gh pr list --state open");
        assert_eq!(tokens[0].text, "ls");
        assert_eq!(tokens[0].kind, TokenKind::Command);
        assert!(tokens[0].explanation.is_some());

        assert_eq!(tokens[1].text, "-la");
        assert_eq!(tokens[1].kind, TokenKind::Flag);
        let la = tokens[1].explanation.as_ref().unwrap();
        assert_eq!(la.details.len(), 2);

        assert_eq!(tokens[2].text, "|");
        assert_eq!(tokens[2].kind, TokenKind::Operator);
        assert!(tokens[2].explanation.as_ref().unwrap().title.contains("pipe"));

        assert_eq!(tokens[3].text, "rg");
        assert_eq!(tokens[3].kind, TokenKind::Command);
        assert!(tokens[3].explanation.is_some());

        assert_eq!(tokens[4].text, "-in");
        assert_eq!(tokens[4].kind, TokenKind::Flag);
        assert_eq!(tokens[4].explanation.as_ref().unwrap().details.len(), 2);

        assert_eq!(tokens[5].text, "\"hello world\"");
        assert_eq!(tokens[5].kind, TokenKind::Arg);

        assert_eq!(tokens[6].text, "&&");
        assert_eq!(tokens[6].kind, TokenKind::Operator);

        assert_eq!(tokens[7].text, "gh");
        assert_eq!(tokens[7].kind, TokenKind::Command);
        assert_eq!(tokens[8].text, "pr");
        assert_eq!(tokens[8].kind, TokenKind::Subcommand);
        assert!(tokens[8].explanation.is_some());
    }

    #[test]
    fn marks_unknown_commands_for_model_fallback() {
        let tokens = parse_command("FOO=1 my_obscure_cli --weird-flag arg");
        assert_eq!(tokens[0].kind, TokenKind::Arg);
        assert_eq!(tokens[1].text, "my_obscure_cli");
        assert_eq!(tokens[1].kind, TokenKind::Command);
        assert!(tokens[1].explanation.is_none());
    }

    #[test]
    fn explains_brew_and_flags() {
        let tokens = parse_command("brew install --cask ghostty");
        assert_eq!(tokens[0].kind, TokenKind::Command);
        assert!(tokens[0].explanation.is_some());
        assert_eq!(tokens[1].kind, TokenKind::Subcommand);
        assert!(tokens[1].explanation.is_some());
        assert_eq!(tokens[2].kind, TokenKind::Flag);
        assert!(tokens[2].explanation.is_some());
    }

    #[test]
    fn handles_wrapper_flags_and_redirections() {
        let tokens = parse_command("xargs -0 rm -rf 2>&1");
        assert_eq!(tokens[0].text, "xargs");
        assert_eq!(tokens[0].kind, TokenKind::Command);
        assert_eq!(tokens[1].text, "-0");
        assert_eq!(tokens[1].kind, TokenKind::Flag);
        assert_eq!(tokens[2].text, "rm");
        assert_eq!(tokens[2].kind, TokenKind::Command);
        assert_eq!(tokens[3].text, "-rf");
        assert_eq!(tokens[3].kind, TokenKind::Flag);
        assert!(tokens[3].explanation.as_ref().unwrap().note.is_some());
        assert_eq!(tokens[4].text, "2>&1");
        assert_eq!(tokens[4].kind, TokenKind::Operator);
        assert!(tokens[4].explanation.is_some());
    }
}

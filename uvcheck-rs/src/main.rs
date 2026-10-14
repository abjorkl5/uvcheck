use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use serde_json::{json, Value};
use std::env;
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const PROGRESS_BYTES: &[u8] = b".FEsxXpr";
const HELP: &str = r"Usage:
    uvcheck
    uvcheck --json [COMMAND [ARGUMENT ...]]
    uvcheck --json --junit-xml PATH [COMMAND [ARGUMENT ...]]
    uvcheck COMMAND [ARGUMENT ...]

Options:
    --help, -h  Show this help message
    --init      Create the configuration file
    --start     Start a shared check session
    --stop      Stop a shared check session and print its summary
    --json      Emit JSON output for check runs
    --junit-xml PATH  Write a JUnit XML report to PATH (requires --json)
";

#[derive(Debug)]
struct State {
    started_at: u128,
    passed: u64,
    failed: u64,
}

type ConfiguredCommand = (Vec<String>, Vec<String>);

fn split_runner(command: &[String]) -> Option<(&'static [&'static str], usize)> {
    const RUNNERS: &[&[&str]] = &[&["uv", "run"], &["uvx"], &["uv", "tool", "run"]];
    for runner in RUNNERS {
        if command.len() >= runner.len()
            && command
                .iter()
                .zip(*runner)
                .all(|(part, expected)| part == expected)
        {
            return Some((runner, runner.len()));
        }
    }
    None
}

impl State {
    fn parse(contents: &str) -> io::Result<Self> {
        let mut values = contents.split_whitespace();
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid uvcheck state");
        let started_at = values
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let passed = values
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let failed = values
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        if values.next().is_some() {
            return Err(invalid());
        }
        Ok(Self {
            started_at,
            passed,
            failed,
        })
    }

    fn write(&self, file: &mut File) -> io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "{} {} {}", self.started_at, self.passed, self.failed)?;
        let length = file.stream_position()?;
        file.set_len(length)
    }
}

fn no_color() -> bool {
    env::var_os("NO_COLOR").is_some() || env::var("TERM").is_ok_and(|term| term == "dumb")
}

fn stdout_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

fn colorize_status(status: &str) -> String {
    if !stdout_is_tty() || no_color() {
        return status.to_string();
    }
    let color = match status {
        "PASS" => "\u{1b}[32m",
        "FAIL" => "\u{1b}[31m",
        _ => return status.to_string(),
    };
    format!("{color}{status}\u{1b}[0m")
}

fn state_file() -> PathBuf {
    env::var_os("UVCHECK_STATE_FILE").map_or_else(
        || {
            let uid = unsafe { libc::getuid() };
            let parent_pid = unsafe { libc::getppid() };
            env::temp_dir().join(format!("uvcheck-{uid}-{parent_pid}.state"))
        },
        PathBuf::from,
    )
}

fn config_file() -> PathBuf {
    env::var_os("UVCHECK_CONFIG_FILE").map_or_else(|| PathBuf::from(".uvcheck"), PathBuf::from)
}

fn lock(file: &File) -> io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn now_ns() -> io::Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(io::Error::other)
}

fn start_session() -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(state_file())?;
    lock(&file)?;
    State {
        started_at: now_ns()?,
        passed: 0,
        failed: 0,
    }
    .write(&mut file)
}

fn update_session(returncode: i32) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(state_file()) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    lock(&file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut state = State::parse(&contents)?;
    if returncode == 0 {
        state.passed += 1;
    } else {
        state.failed += 1;
    }
    state.write(&mut file)
}

fn stop_session() -> io::Result<i32> {
    let path = state_file();
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            eprintln!("uvcheck: no active session");
            return Ok(2);
        }
        Err(error) => return Err(error),
    };
    lock(&file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let state = State::parse(&contents)?;
    fs::remove_file(path)?;

    let elapsed = now_ns()?.saturating_sub(state.started_at) as f64 / 1_000_000_000.0;
    let (status, returncode) = if state.failed == 0 {
        ("PASS", 0)
    } else {
        ("FAIL", 1)
    };
    let colored_status = colorize_status(status);
    println!(
        "\n{colored_status} ({} PASS, {} FAIL) in {elapsed:.2}s",
        state.passed, state.failed
    );
    Ok(returncode)
}

fn stop_session_json(commands: Option<Vec<Value>>) -> io::Result<i32> {
    let path = state_file();
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("{}", json!({"error": "no active session", "exit_code": 2}));
            return Ok(2);
        }
        Err(error) => return Err(error),
    };
    lock(&file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let state = State::parse(&contents)?;
    fs::remove_file(path)?;

    let elapsed = now_ns()?.saturating_sub(state.started_at) as f64 / 1_000_000_000.0;
    let status = if state.failed == 0 { "PASS" } else { "FAIL" };
    let mut report = json!({
        "summary": {
            "status": status,
            "passed": state.passed,
            "failed": state.failed,
            "duration_seconds": elapsed
        }
    });
    if let Some(commands) = commands {
        report["commands"] = json!(commands);
    }
    println!("{report}");
    Ok(if state.failed == 0 { 0 } else { 1 })
}

#[cfg(test)]
fn configured_commands_from(path: &std::path::Path) -> io::Result<Vec<ConfiguredCommand>> {
    let contents = fs::read_to_string(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("configuration not found: {}", path.display()),
            )
        } else {
            error
        }
    })?;
    let mut commands = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let command = shlex::split(line).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}:{}: invalid shell syntax", path.display(), index + 1),
            )
        })?;
        if let Some((runner, runner_length)) = split_runner(&command) {
            if command.len() == runner_length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}:{}: expected a command after the launcher",
                        path.display(),
                        index + 1
                    ),
                ));
            }
            commands.push((
                runner.iter().map(|part| part.to_string()).collect(),
                command[runner_length..].to_vec(),
            ));
        } else {
            commands.push((vec!["uv".to_string(), "run".to_string()], command));
        }
    }
    Ok(commands)
}

fn default_commands() -> Vec<Vec<String>> {
    vec![
        vec![
            "ruff".to_string(),
            "format".to_string(),
            "--check".to_string(),
        ],
        vec!["ruff".to_string(), "check".to_string()],
        vec![
            "python".to_string(),
            "-m".to_string(),
            "unittest".to_string(),
            "-b".to_string(),
        ],
    ]
}

fn default_config() -> String {
    "# Formatting\n\
uv run ruff format --check\n\
# Linting\n\
uv run ruff check\n\
# Unit Testing\n\
uv run python -m unittest -b\n"
        .to_string()
}

fn init_config() -> io::Result<()> {
    let path = config_file();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("configuration already exists: {}", path.display()),
                )
            } else {
                error
            }
        })?;
    file.write_all(default_config().as_bytes())?;
    println!("uvcheck: created {}", path.display());
    Ok(())
}

fn configured_commands() -> io::Result<Vec<ConfiguredCommand>> {
    let path = config_file();
    let explicit_config = env::var_os("UVCHECK_CONFIG_FILE").is_some();
    match fs::read_to_string(&path) {
        Ok(contents) => {
            let mut commands = Vec::new();
            for (index, line) in contents.lines().enumerate() {
                if line.trim().is_empty() || line.trim_start().starts_with('#') {
                    continue;
                }
                let command = shlex::split(line).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}:{}: invalid shell syntax", path.display(), index + 1),
                    )
                })?;
                if let Some((runner, runner_length)) = split_runner(&command) {
                    if command.len() == runner_length {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{}:{}: expected a command after the launcher",
                                path.display(),
                                index + 1
                            ),
                        ));
                    }
                    commands.push((
                        runner.iter().map(|part| part.to_string()).collect(),
                        command[runner_length..].to_vec(),
                    ));
                } else {
                    commands.push((vec!["uv".to_string(), "run".to_string()], command));
                }
            }
            Ok(commands)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !explicit_config => {
            Ok(default_commands()
                .into_iter()
                .map(|command| (vec!["uv".to_string(), "run".to_string()], command))
                .collect())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("configuration not found: {}", path.display()),
        )),
        Err(error) => Err(error),
    }
}

fn is_unittest(command: &[String]) -> bool {
    command.first().map(String::as_str) == Some("python")
        && command.get(1).map(String::as_str) == Some("-m")
        && command.iter().any(|argument| argument == "unittest")
        && command.iter().any(|argument| argument == "-b")
}

fn is_pytest(command: &[String]) -> bool {
    command.first().map(String::as_str) == Some("pytest")
        && command.iter().any(|argument| argument == "-q")
}

fn display_command(command: &[String]) -> String {
    command
        .iter()
        .map(|argument| {
            if argument
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-._/".contains(character))
            {
                argument.clone()
            } else {
                format!("'{}'", argument.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

enum ProgressState {
    Dots,
    Pad,
    Bracket,
    AfterBracket,
}

// Reads a stream, optionally tracking unittest's `.FEs` or pytest's `.FEsxXpr[ NN%]` progress
// characters live, swallowing the trailing bracket so the dots stay on one line.
fn read_stream(mut stream: impl Read, track_progress: bool) -> (Vec<u8>, bool) {
    let mut output = Vec::new();
    if !track_progress {
        stream
            .read_to_end(&mut output)
            .expect("failed to read stream");
        return (output, false);
    }

    let mut progress_active = true;
    let mut progress_started = false;
    let mut state = ProgressState::Dots;
    let mut buffer = [0; 4096];

    loop {
        let bytes_read = stream.read(&mut buffer).expect("failed to read stream");
        if bytes_read == 0 {
            break;
        }
        for byte in &buffer[..bytes_read] {
            if !progress_active {
                output.push(*byte);
                continue;
            }
            let character = *byte as char;
            match state {
                ProgressState::Dots => {
                    if PROGRESS_BYTES.contains(byte) {
                        print!("{character}");
                        io::stdout().flush().expect("failed to flush progress");
                        progress_started = true;
                    } else if character == ' ' && progress_started {
                        state = ProgressState::Pad;
                    } else if character == '\n' && progress_started {
                        progress_active = false;
                    } else {
                        progress_active = false;
                        output.push(*byte);
                    }
                }
                ProgressState::Pad => {
                    if character == ' ' {
                        // swallow padding before the percentage bracket
                    } else if character == '[' {
                        state = ProgressState::Bracket;
                    } else {
                        progress_active = false;
                        output.push(*byte);
                    }
                }
                ProgressState::Bracket => {
                    if character == ']' {
                        state = ProgressState::AfterBracket;
                    }
                }
                ProgressState::AfterBracket => {
                    if character == '\n' {
                        state = ProgressState::Dots;
                    } else {
                        progress_active = false;
                        output.push(*byte);
                    }
                }
            }
        }
    }
    (output, progress_started)
}

fn line_starting_with<'a>(output: &'a str, prefix: &str) -> Option<&'a str> {
    output.lines().find(|line| line.starts_with(prefix))
}

fn pytest_summary_line(output: &str) -> Option<&str> {
    output.lines().find(|line| {
        line.chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
            && line.contains(" in ")
            && line.ends_with('s')
    })
}

fn test_result(stdout: &[u8], stderr: &[u8], progress_started: bool, success: bool, pytest: bool) {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let output = format!("{stdout}\n{stderr}");
    if progress_started {
        if stdout_is_tty() {
            print!("\r\u{1b}[2K");
        } else {
            println!();
        }
    }
    if !success {
        print!("{stdout}");
        eprint!("{stderr}");
    }
    if pytest {
        if let Some(summary) = pytest_summary_line(&output) {
            println!("{summary}");
        }
    } else if let (Some(status), Some(summary)) = (
        output
            .lines()
            .find(|line| *line == "OK" || line.starts_with("FAILED")),
        line_starting_with(&output, "Ran "),
    ) {
        println!("{status} - {summary}");
    }
}

fn run(runner: &[String], command: &[String]) -> io::Result<i32> {
    println!("+ {} {}", display_command(runner), display_command(command));
    let started = Instant::now();
    let unittest = is_unittest(command);
    let pytest = is_pytest(command);
    let mut process = Command::new(&runner[0]);
    process
        .args(&runner[1..])
        .args(command)
        .env("NO_COLOR", "1");

    if unittest || pytest {
        let mut child = process
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("missing stdout pipe");
        let stderr = child.stderr.take().expect("missing stderr pipe");
        let stdout_thread = thread::spawn(move || read_stream(stdout, pytest));
        let stderr_thread = thread::spawn(move || read_stream(stderr, unittest));
        let status = child.wait()?;
        let (stdout, stdout_progress_started) = stdout_thread.join().expect("stdout reader failed");
        let (stderr, stderr_progress_started) = stderr_thread.join().expect("stderr reader failed");
        let progress_started = stdout_progress_started || stderr_progress_started;
        test_result(&stdout, &stderr, progress_started, status.success(), pytest);

        let result = if status.success() { "PASS" } else { "FAIL" };
        println!(
            "{} {} ({:.2}s)",
            colorize_status(result),
            display_command(command),
            started.elapsed().as_secs_f64()
        );
        let returncode = status.code().unwrap_or(1);
        update_session(returncode)?;
        return Ok(returncode);
    }

    let status = process.status()?;

    let result = if status.success() { "PASS" } else { "FAIL" };
    println!(
        "{} {} ({:.2}s)",
        colorize_status(result),
        display_command(command),
        started.elapsed().as_secs_f64()
    );
    let returncode = status.code().unwrap_or(1);
    update_session(returncode)?;
    Ok(returncode)
}

fn run_json(runner: &[String], command: &[String]) -> io::Result<(i32, Value)> {
    let started = Instant::now();
    let output = Command::new(&runner[0])
        .args(&runner[1..])
        .args(command)
        .env("NO_COLOR", "1")
        .output()?;
    let returncode = output.status.code().unwrap_or(1);
    let status = if returncode == 0 { "PASS" } else { "FAIL" };
    update_session(returncode)?;
    Ok((
        returncode,
        json!({
            "command": runner.iter().chain(command).collect::<Vec<_>>(),
            "status": status,
            "exit_code": returncode,
            "duration_seconds": started.elapsed().as_secs_f64(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr)
        }),
    ))
}

fn write_xml_text(writer: &mut Writer<Vec<u8>>, name: &str, text: &str) -> io::Result<()> {
    writer.write_event(Event::Start(BytesStart::new(name)))?;
    writer.write_event(Event::Text(BytesText::new(text)))?;
    writer.write_event(Event::End(BytesEnd::new(name)))
}

fn junit_timestamp() -> io::Result<String> {
    let seconds = unsafe { libc::time(std::ptr::null_mut()) };
    if seconds == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut broken_down = std::mem::MaybeUninit::<libc::tm>::uninit();
    let broken_down = unsafe { libc::gmtime_r(&seconds, broken_down.as_mut_ptr()) };
    if broken_down.is_null() {
        return Err(io::Error::last_os_error());
    }
    let format = b"%Y-%m-%dT%H:%M:%SZ\0";
    let mut buffer = [0 as libc::c_char; 32];
    let length = unsafe {
        libc::strftime(
            buffer.as_mut_ptr(),
            buffer.len(),
            format.as_ptr().cast(),
            broken_down,
        )
    };
    if length == 0 {
        return Err(io::Error::other("failed to format JUnit timestamp"));
    }
    Ok(unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned())
}

fn junit_xml_report(results: &[Value], timestamp: &str) -> io::Result<Vec<u8>> {
    let tests = results.len().to_string();
    let failures = results
        .iter()
        .filter(|result| result["status"] == "FAIL")
        .count()
        .to_string();
    let elapsed = results
        .iter()
        .map(|result| result["duration_seconds"].as_f64().unwrap_or(0.0))
        .sum::<f64>();
    let elapsed = format!("{elapsed:.6}");
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    let mut suites = BytesStart::new("testsuites");
    suites.push_attribute(("name", "uvcheck"));
    suites.push_attribute(("tests", tests.as_str()));
    suites.push_attribute(("failures", failures.as_str()));
    suites.push_attribute(("errors", "0"));
    suites.push_attribute(("time", elapsed.as_str()));
    suites.push_attribute(("timestamp", timestamp));
    writer.write_event(Event::Start(suites))?;

    let mut suite = BytesStart::new("testsuite");
    suite.push_attribute(("name", "uvcheck"));
    suite.push_attribute(("tests", tests.as_str()));
    suite.push_attribute(("failures", failures.as_str()));
    suite.push_attribute(("errors", "0"));
    suite.push_attribute(("time", elapsed.as_str()));
    suite.push_attribute(("timestamp", timestamp));
    writer.write_event(Event::Start(suite))?;

    for result in results {
        let command = result["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        let command = display_command(&command);
        let escaped_command = quick_xml::escape::escape(&command);
        let duration = format!("{:.6}", result["duration_seconds"].as_f64().unwrap_or(0.0));
        let mut testcase = BytesStart::new("testcase");
        testcase.push_attribute(("classname", "uvcheck"));
        testcase.push_attribute(("name", escaped_command.as_ref()));
        testcase.push_attribute(("time", duration.as_str()));
        writer.write_event(Event::Start(testcase))?;

        let exit_code = result["exit_code"].as_i64().unwrap_or(1);
        if exit_code != 0 {
            let message = format!("Command failed with exit code {exit_code}");
            let mut failure = BytesStart::new("failure");
            failure.push_attribute(("message", message.as_str()));
            writer.write_event(Event::Start(failure))?;
            writer.write_event(Event::Text(BytesText::new(&message)))?;
            writer.write_event(Event::End(BytesEnd::new("failure")))?;
        }
        for (key, tag) in [("stdout", "system-out"), ("stderr", "system-err")] {
            if let Some(output) = result[key].as_str().filter(|output| !output.is_empty()) {
                write_xml_text(&mut writer, tag, output)?;
            }
        }
        writer.write_event(Event::End(BytesEnd::new("testcase")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("testsuite")))?;
    writer.write_event(Event::End(BytesEnd::new("testsuites")))?;
    Ok(writer.into_inner())
}

fn print_xml_command(runner: &[String], command: &[String], result: &Value) {
    println!("+ {} {}", display_command(runner), display_command(command));
    if let Some(stdout) = result["stdout"].as_str() {
        print!("{stdout}");
    }
    if let Some(stderr) = result["stderr"].as_str() {
        eprint!("{stderr}");
    }
    let status = result["status"].as_str().unwrap_or("FAIL");
    let duration = result["duration_seconds"].as_f64().unwrap_or(0.0);
    println!(
        "{} {} ({duration:.2}s)",
        colorize_status(status),
        display_command(command)
    );
}

fn run_xml(
    runner: &[String],
    command: &[String],
    path: &Path,
    json_output: bool,
) -> io::Result<i32> {
    let timestamp = junit_timestamp()?;
    let (returncode, result) = run_json(runner, command)?;
    if json_output {
        println!("{result}");
    } else {
        print_xml_command(runner, command, &result);
    }
    fs::write(path, junit_xml_report(&[result], &timestamp)?)?;
    Ok(returncode)
}

fn run_all() -> io::Result<i32> {
    let commands = configured_commands()?;
    start_session()?;
    for (runner, command) in commands {
        run(&runner, &command)?;
    }
    stop_session()
}

fn run_all_json() -> io::Result<i32> {
    let commands = configured_commands()?;
    start_session()?;
    let mut results = Vec::new();
    for (runner, command) in commands {
        let (_, result) = run_json(&runner, &command)?;
        results.push(result);
    }
    stop_session_json(Some(results))
}

fn run_all_xml(path: &Path, json_output: bool) -> io::Result<i32> {
    let timestamp = junit_timestamp()?;
    let commands = configured_commands()?;
    start_session()?;
    let mut results = Vec::new();
    for (runner, command) in commands {
        let (_, result) = run_json(&runner, &command)?;
        if !json_output {
            print_xml_command(&runner, &command, &result);
        }
        results.push(result);
    }
    let status = if json_output {
        stop_session_json(Some(results.clone()))?
    } else {
        stop_session()?
    };
    fs::write(path, junit_xml_report(&results, &timestamp)?)?;
    Ok(status)
}

fn print_help() {
    print!("{HELP}");
}

fn main() -> ExitCode {
    let mut arguments: Vec<String> = env::args().skip(1).collect();
    let json_output = arguments
        .first()
        .is_some_and(|argument| argument == "--json");
    if json_output {
        arguments.remove(0);
    }
    let junit_xml_path = if arguments
        .first()
        .is_some_and(|argument| argument == "--junit-xml")
    {
        if arguments.len() < 2 {
            eprintln!("uvcheck: --junit-xml requires a PATH");
            return ExitCode::from(2);
        }
        let path = PathBuf::from(arguments.remove(1));
        arguments.remove(0);
        Some(path)
    } else {
        None
    };
    if junit_xml_path.is_some() && !json_output {
        eprintln!("uvcheck: --junit-xml requires --json");
        return ExitCode::from(2);
    }
    let junit_xml_output = junit_xml_path.is_some();
    let result = match arguments.as_slice() {
        [] if junit_xml_output => run_all_xml(junit_xml_path.as_deref().unwrap(), json_output),
        [] if json_output => run_all_json(),
        [] => run_all(),
        [argument] if argument == "--help" || argument == "-h" => {
            print_help();
            Ok(0)
        }
        [argument] if argument == "--init" => init_config().map(|()| 0),
        [argument] if argument == "--start" => start_session().map(|()| 0),
        [argument] if argument == "--stop" => stop_session(),
        command if junit_xml_output => run_xml(
            &["uv".to_string(), "run".to_string()],
            command,
            junit_xml_path.as_deref().unwrap(),
            json_output,
        ),
        command if json_output => {
            run_json(&["uv".to_string(), "run".to_string()], command).map(|(returncode, report)| {
                println!("{report}");
                returncode
            })
        }
        command => run(&["uv".to_string(), "run".to_string()], command),
    };

    match result {
        Ok(returncode) => ExitCode::from(returncode as u8),
        Err(error) => {
            if json_output {
                println!("{}", json!({"error": error.to_string(), "exit_code": 2}));
            } else {
                eprintln!("uvcheck: {error}");
            }
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_python_session_state() {
        let state = State::parse("123 4 2").unwrap();

        assert_eq!(state.started_at, 123);
        assert_eq!(state.passed, 4);
        assert_eq!(state.failed, 2);
    }

    #[test]
    fn help_describes_the_command_interface() {
        assert!(HELP.contains("uvcheck COMMAND [ARGUMENT ...]"));
        assert!(HELP.contains("--init"));
        assert!(HELP.contains("--start"));
        assert!(HELP.contains("--stop"));
        assert!(HELP.contains("--json --junit-xml PATH"));
    }

    #[test]
    fn reports_missing_config_file_with_path() {
        let path = std::env::temp_dir().join(format!(
            "uvcheck-config-missing-{}-{}.uvcheck",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));

        let error = configured_commands_from(&path).unwrap_err();
        let message = error.to_string();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(message.contains("configuration not found"));
        assert!(message.contains(path.to_string_lossy().as_ref()));
    }

    #[test]
    fn uses_default_configuration_when_project_config_is_missing() {
        let original_dir = std::env::current_dir().unwrap();
        let temp_dir = std::env::temp_dir().join(format!(
            "uvcheck-default-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();
        std::env::remove_var("UVCHECK_CONFIG_FILE");

        let commands = configured_commands().unwrap();
        assert_eq!(
            commands,
            vec![
                (
                    vec!["uv".to_string(), "run".to_string()],
                    vec![
                        "ruff".to_string(),
                        "format".to_string(),
                        "--check".to_string()
                    ]
                ),
                (
                    vec!["uv".to_string(), "run".to_string()],
                    vec!["ruff".to_string(), "check".to_string()]
                ),
                (
                    vec!["uv".to_string(), "run".to_string()],
                    vec![
                        "python".to_string(),
                        "-m".to_string(),
                        "unittest".to_string(),
                        "-b".to_string()
                    ]
                ),
            ]
        );

        std::env::set_current_dir(original_dir).unwrap();
        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn formats_default_config() {
        assert_eq!(
            default_config(),
            "# Formatting\nuv run ruff format --check\n# Linting\nuv run ruff check\n# Unit Testing\nuv run python -m unittest -b\n"
        );
    }

    #[test]
    fn parses_valid_config_lines() {
        let path = std::env::temp_dir().join(format!(
            "uvcheck-config-valid-{}-{}.uvcheck",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(
            &path,
            "# comment\nuv run ruff check\nuvx black .\nuv tool run mypy src\n",
        )
        .unwrap();

        let commands = configured_commands_from(&path).unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(
            commands[0],
            (
                vec!["uv".to_string(), "run".to_string()],
                vec!["ruff".to_string(), "check".to_string()]
            )
        );
        assert_eq!(
            commands[1],
            (
                vec!["uvx".to_string()],
                vec!["black".to_string(), ".".to_string()]
            )
        );
        assert_eq!(
            commands[2],
            (
                vec!["uv".to_string(), "tool".to_string(), "run".to_string()],
                vec!["mypy".to_string(), "src".to_string()]
            )
        );

        std::fs::remove_file(path).unwrap();
    }
}

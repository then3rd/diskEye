//! External command execution behind a trait so provider tests can replay
//! recorded output from `tests/fixtures`.

#[cfg(test)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

pub trait CommandRunner: Send + Sync {
    /// Run `argv`; `None` if the program doesn't exist.
    fn run(&self, argv: &[&str]) -> Option<Output>;

    /// Whether `prog` is on PATH (or an absolute path that exists).
    fn has(&self, prog: &str) -> bool;

    /// Read a file (sysfs, /proc, config). Lets tests fake the filesystem.
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

pub struct SystemRunner {
    pub timeout: Duration,
}

impl Default for SystemRunner {
    fn default() -> Self {
        SystemRunner { timeout: Duration::from_secs(60) }
    }
}

pub fn which(prog: &str) -> Option<PathBuf> {
    if prog.contains('/') {
        return std::path::Path::new(prog).exists().then(|| prog.into());
    }
    let path = std::env::var_os("PATH")
        .unwrap_or_else(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/sbin", "/usr/bin"].iter().map(PathBuf::from))
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

impl CommandRunner for SystemRunner {
    fn run(&self, argv: &[&str]) -> Option<Output> {
        let prog = which(argv[0])?;
        let mut child = Command::new(prog)
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("LC_ALL", "C")
            .spawn()
            .ok()?;
        // Read pipes on threads so a chatty command can't deadlock us.
        let mut so = child.stdout.take()?;
        let mut se = child.stderr.take()?;
        let t1 = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = std::io::Read::read_to_end(&mut so, &mut s);
            s
        });
        let t2 = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = std::io::Read::read_to_end(&mut se, &mut s);
            s
        });
        let start = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st.code().unwrap_or(-1),
                Ok(None) if start.elapsed() > self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break -2;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break -1,
            }
        };
        Some(Output {
            status,
            stdout: String::from_utf8_lossy(&t1.join().unwrap_or_default()).into_owned(),
            stderr: String::from_utf8_lossy(&t2.join().unwrap_or_default()).into_owned(),
        })
    }

    fn has(&self, prog: &str) -> bool {
        which(prog).is_some()
    }
}

/// Replays canned outputs keyed by the joined argv. Files can be faked too.
#[cfg(test)]
#[derive(Default)]
pub struct FakeRunner {
    pub outputs: HashMap<String, Output>,
    pub files: HashMap<String, String>,
    pub programs: Vec<String>,
}

#[cfg(test)]
impl FakeRunner {
    pub fn with(mut self, argv: &str, stdout: &str) -> Self {
        self.outputs.insert(argv.to_string(), Output { status: 0, stdout: stdout.to_string(), stderr: String::new() });
        let prog = argv.split(' ').next().unwrap_or_default().to_string();
        if !self.programs.contains(&prog) {
            self.programs.push(prog);
        }
        self
    }

    pub fn file(mut self, path: &str, content: &str) -> Self {
        self.files.insert(path.to_string(), content.to_string());
        self
    }
}

#[cfg(test)]
impl CommandRunner for FakeRunner {
    fn run(&self, argv: &[&str]) -> Option<Output> {
        if !self.has(argv[0]) {
            return None;
        }
        Some(self.outputs.get(&argv.join(" ")).cloned().unwrap_or(Output {
            status: 1,
            stdout: String::new(),
            stderr: format!("no fixture for: {}", argv.join(" ")),
        }))
    }

    fn has(&self, prog: &str) -> bool {
        self.programs.iter().any(|p| p == prog)
    }

    fn read(&self, path: &str) -> Option<String> {
        self.files.get(path).cloned()
    }
}

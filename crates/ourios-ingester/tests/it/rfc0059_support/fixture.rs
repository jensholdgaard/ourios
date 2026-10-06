//! The startup crash fixture, run as a real OS process.

use std::time::Duration;

use super::Node;

/// How long the startup crash fixture has to reach its kill point.
const FIXTURE_WATCHDOG: Duration = Duration::from_secs(60);

/// The startup crash fixture as a real OS process over a node's roots,
/// killed and reaped however the test ends, since it never exits on its
/// own.
pub struct Fixture {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Fixture {
    /// Spawn the fixture in `mode` (`reserve` or `scan`) over `node`, with
    /// a thread forwarding its stdout lines. The thread keeps the pipe open
    /// until the child dies, so the kill, not a failed write, ends it.
    pub fn spawn(mode: &str, node: &Node) -> Self {
        use std::io::BufRead;

        let mut child =
            std::process::Command::new(env!("CARGO_BIN_EXE_template_ids_crash_fixture"))
                .arg(mode)
                .arg(&node.wal)
                .arg(&node.store)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn the template-id crash fixture");
        let stdout = child.stdout.take().expect("fixture stdout piped");
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    /// Wait for the fixture's first line, which must start with `prefix`.
    pub fn reached(&self, prefix: &str) -> String {
        match self.lines.recv_timeout(FIXTURE_WATCHDOG) {
            Ok(line) if line.starts_with(prefix) => line,
            Ok(line) => panic!("the fixture printed {line:?} before {prefix}"),
            Err(e) => panic!("the fixture never printed {prefix}: {e}"),
        }
    }

    /// `SIGKILL` the child and reap it.
    pub fn kill(&mut self) {
        self.child.kill().expect("SIGKILL the fixture");
        self.child.wait().expect("reap the fixture");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Already reaped on the passing path; these only matter on a failing one.
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

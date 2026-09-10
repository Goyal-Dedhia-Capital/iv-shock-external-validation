use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use backtest_contracts::{ResearchRequest, ResearchResponse};
use backtest_engine::DecisionRunner;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("failed to spawn research process: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("research process did not expose piped {0}")]
    MissingPipe(&'static str),
    #[error("research process I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("research process returned no response")]
    EndOfStream,
    #[error("research process returned invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

pub struct ProcessRunner {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
}

impl ProcessRunner {
    /// Starts one persistent research process with piped JSONL input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot start or expose both pipes.
    pub fn spawn(spec: &ProcessSpec) -> Result<Self, ProcessError> {
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(ProcessError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or(ProcessError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ProcessError::MissingPipe("stdout"))?;
        Ok(Self {
            child,
            stdin: Some(BufWriter::new(stdin)),
            stdout: BufReader::new(stdout),
        })
    }

    /// Closes input and waits for the research process to exit.
    ///
    /// # Errors
    ///
    /// Returns an error if waiting on the child process fails.
    pub fn shutdown(mut self) -> Result<(), ProcessError> {
        drop(self.stdin.take());
        let _status = self.child.wait()?;
        Ok(())
    }

    fn exchange(&mut self, request: &ResearchRequest) -> Result<ResearchResponse, ProcessError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(ProcessError::MissingPipe("stdin"))?;
        serde_json::to_writer(&mut *stdin, request)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;

        let mut line = String::new();
        if self.stdout.read_line(&mut line)? == 0 {
            return Err(ProcessError::EndOfStream);
        }
        Ok(serde_json::from_str(&line)?)
    }
}

impl DecisionRunner for ProcessRunner {
    fn decide(&mut self, request: &ResearchRequest) -> Result<ResearchResponse, String> {
        self.exchange(request).map_err(|error| error.to_string())
    }
}

impl Drop for ProcessRunner {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

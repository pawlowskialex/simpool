//! Running simctl, simfast and friends.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::ffi::OsStr;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Runs a program to completion. A non-zero exit is an error carrying stderr.
pub async fn run<I, S>(program: impl AsRef<OsStr>, args: I, stdin: Option<&[u8]>, envs: &[(String, String)], timeout: Duration) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref().to_owned();
    let mut cmd = Command::new(&program);
    cmd.args(args)
        .envs(envs.iter().map(|(k, v)| (k, v)))
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| format!("starting {}", program.to_string_lossy()))?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(input).await?;
        drop(pipe);
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{} timed out after {}s", program.to_string_lossy(), timeout.as_secs()))??;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let msg = if stderr.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { stderr };
        bail!("{} failed ({}): {}", program.to_string_lossy(), out.status, msg);
    }
    Ok(Output { stdout: out.stdout, stderr })
}

/// `xcrun simctl <args>`
pub async fn simctl<S: AsRef<str>>(args: &[S]) -> Result<Output> {
    simctl_with(args, None, &[]).await
}

pub async fn simctl_with<S: AsRef<str>>(args: &[S], stdin: Option<&[u8]>, envs: &[(String, String)]) -> Result<Output> {
    simctl_full(args, stdin, envs, Duration::from_secs(120)).await
}

/// For the rare command whose duration depends on its input, like installing a huge app.
pub async fn simctl_timeout<S: AsRef<str>>(args: &[S], timeout: Duration) -> Result<Output> {
    simctl_full(args, None, &[], timeout).await
}

async fn simctl_full<S: AsRef<str>>(args: &[S], stdin: Option<&[u8]>, envs: &[(String, String)], timeout: Duration) -> Result<Output> {
    let mut all = vec!["simctl"];
    all.extend(args.iter().map(|a| a.as_ref()));
    // pbcopy/pbpaste and friends pick their text encoding from the locale.
    let mut envs = envs.to_vec();
    envs.push(("LC_ALL".into(), "en_US.UTF-8".into()));
    run("xcrun", all, stdin, &envs, timeout).await.map_err(|e| anyhow!("{}", e.to_string().replacen("xcrun failed", "simctl failed", 1)))
}

/// Converts any plist (XML, binary or the old OpenStep text format simctl prints) to JSON.
pub async fn plist_to_json(data: &[u8]) -> Result<Value> {
    let out = run("plutil", ["-convert", "json", "-r", "-o", "-", "--", "-"], Some(data), &[], Duration::from_secs(30)).await?;
    Ok(serde_json::from_slice(&out.stdout)?)
}

pub fn developer_dir() -> Result<String> {
    if let Ok(d) = std::env::var("DEVELOPER_DIR") {
        return Ok(d);
    }
    let out = std::process::Command::new("xcode-select").arg("-p").output().context("running xcode-select")?;
    if !out.status.success() {
        bail!("xcode-select -p failed; is Xcode installed?");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

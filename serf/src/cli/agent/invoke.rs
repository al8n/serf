use std::{
  env, io,
  process::{Command, Stdio},
  sync::LazyLock,
  time::{Duration, Instant},
};

use futures::io::AsyncWriteExt;
use memberlist::{
  metrics,
  net::{AddressResolver, Transport},
};
use serde_json::error;
use serf_core::{delegate::Delegate, event::Event, types::Member};
use smol_str::SmolStr;

/// Limits how much data we collect from a handler.
/// This is to prevent Serf's memory from growing to an enormous
/// amount due to a faulty handler.
const MAX_BUF_SIZE: usize = 8 * 1024;

/// Used to warn about a slow handler invocation
const WARN_SLOW: Duration = Duration::from_secs(1);

const SANITIZE_TAG_REGEXP: LazyLock<regex::Regex> =
  LazyLock::new(|| regex::Regex::new(r"[^A-Z0-9_]").unwrap());

pub(super) fn invoke_event_script<T, D>(
  script: &str,
  this: Member<T::Id, <T::Resolver as AddressResolver>::Address>,
  event: Event<T, D>,
) -> Result<(), ()>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
{
  let now = Instant::now();
  scopeguard::defer!({
    let h = metrics::histogram!("serf.agent.invoke");
    h.record(now.elapsed().as_millis() as f64);
  });

  // Determine shell and flag based on OS
  let (shell, flag) = if cfg!(windows) {
    ("cmd", "/C")
  } else {
    ("/bin/sh", "-c")
  };

  // Create command with shell and flag
  let mut cmd = Command::new(shell);
  cmd.arg(flag).arg(script).stdin(Stdio::piped());

  // Get current environment variables
  let current_env = env::vars();

  // Preserve existing environment variables
  for (key, value) in current_env {
    cmd.env(key, value);
  }

  // Add custom environment variables
  cmd
    .env("SERF_EVENT", event.ty().as_str())
    .env("SERF_SELF_NAME", this.node().id());

  if let Some(role) = this.tags().get("role") {
    cmd.env("SERF_SELF_ROLE", role);
  } else {
    cmd.env("SERF_SELF_ROLE", "");
  }

  for (name, val) in this.tags().iter() {
    if name.eq("role") {
      continue;
    }

    //http://stackoverflow.com/questions/2821043/allowed-characters-in-linux-environment-variable-names
    //(http://pubs.opengroup.org/onlinepubs/000095399/basedefs/xbd_chap08.html for the long version)
    //says that env var names must be in [A-Z0-9_] and not start with [0-9].
    //we only care about the first part, so convert all chars not in [A-Z0-9_] to _
    let sanitized_name = SANITIZE_TAG_REGEXP.replace_all(&name.as_str().to_uppercase(), "_");
    // let tag_env = format!("SERF_TAG_{}", sanitized_name);
    cmd.env(format!("SERF_TAG_{}", sanitized_name), val);
  }

  match event.ty() {}

  // Execute command and capture output
  let output_result = cmd.output()?;

  // Write stdout and stderr to the output
  output.write_all(&output_result.stdout)?;
  output.write_all(&output_result.stderr)?;

  todo!()
}

/// Sends data on stdin for an event. The stdin simply contains the
/// payload (if any).
/// Most shells read implementations need a newline, force it to be there
async fn stream_payload<W>(stdin: &mut W, buf: &[u8])
where
  W: futures::io::AsyncWrite + Unpin,
{
  const MAX_INLINE_SIZE: usize = 128;

  macro_rules! defer_close {
    ($io:ident) => {
      let _ = $io.close().await;
    };
  }

  macro_rules! write_payload {
    ($io:ident($buf: expr)) => {
      if let Err(e) = $io.write_all(&buffer[..len + 1]).await {
        tracing::error!(err=%e,"error writing payload");
      }
    };
  }

  // Append a newline to payload if missing
  let len = buf.len();
  if len > 0 && buf[len - 1] != b'\n' {
    if len + 1 > MAX_INLINE_SIZE {
      let mut buffer = Vec::with_capacity(len + 1);
      buffer.extend_from_slice(buf);
      buffer.push(b'\n');
    } else {
      let mut buffer = [0; MAX_INLINE_SIZE];
      buffer[..len].copy_from_slice(buf);
      buffer[len] = b'\n';

      if let Err(e) = stdin.write_all(&buffer[..len + 1]).await {
        tracing::error!(err=%e,"error writing payload");
      }
    }
  } else {
    if let Err(e) = stdin.write_all(&buf).await {
      tracing::error!(err=%e,"error writing payload");
    }
  }

  defer_close!(stdin);
}

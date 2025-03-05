use std::{
  env, io, process::Stdio, sync::LazyLock, time::{Duration, Instant}
};

use agnostic::{process::{Child, ChildStdin, Command, Process}, AfterHandle, Runtime, RuntimeLite};
use futures::io::{AsyncWrite, AsyncWriteExt};
use memberlist::{
  bytes::Bytes, metrics, net::Transport,
};

use serf_core::{delegate::Delegate, event::{Event, MemberEvent}, types::Member};
use smol_str::{format_smolstr, SmolStr, StrExt};

/// Limits how much data we collect from a handler.
/// This is to prevent Serf's memory from growing to an enormous
/// amount due to a faulty handler.
const MAX_BUF_SIZE: usize = 8 * 1024;

/// Used to warn about a slow handler invocation
const WARN_SLOW: Duration = Duration::from_secs(1);

const SANITIZE_TAG_REGEXP: LazyLock<regex::Regex> =
  LazyLock::new(|| regex::Regex::new(r"[^A-Z0-9_]").unwrap());

pub(super) async fn invoke_event_script<T, D>(
  script: SmolStr,
  this: Member<T::Id, T::ResolvedAddress>,
  event: Event<T, D>,
) -> io::Result<()>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
  T: Transport,
  T::Runtime: Runtime,
  ChildStdin<<<<T::Runtime as Runtime>::Process as Process>::Child as Child>::Stdin>: AsyncWrite + Unpin + Send,
{
  let now = Instant::now();
  scopeguard::defer!({
    let h = metrics::histogram!("serf.agent.invoke");
    h.record(now.elapsed().as_millis() as f64);
  });
  let mut output = circularbuf::Buffer::new(vec![0; MAX_BUF_SIZE]);

  // Determine shell and flag based on OS
  let (shell, flag) = if cfg!(windows) {
    ("cmd", "/C")
  } else {
    ("/bin/sh", "-c")
  };

  // Create command with shell and flag
  let mut cmd = <<<T::Runtime as Runtime>::Process as Process>::Command as Command>::new(shell);
  cmd.arg(flag).arg(&script)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

  // Get current environment variables
  let current_env = env::vars();

  // Preserve existing environment variables
  for (key, value) in current_env {
    cmd.env(key, value);
  }

  // Add custom environment variables
  cmd
    .env("SERF_EVENT", event.ty().as_str())
    .env("SERF_SELF_NAME", this.node().id().to_string());

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
    let name = name.as_str().to_uppercase_smolstr();
    let sanitized_name = SANITIZE_TAG_REGEXP.replace_all(&name, "_");
    cmd.env(format_smolstr!("SERF_TAG_{}", sanitized_name), val);
  }

  // Start a timer to warn about slow handlers
  let script1 = script.clone();
  let slow_timer = <T::Runtime as RuntimeLite>::spawn_after(WARN_SLOW, async move {
    tracing::warn!("agent: slow handler: {}", script1);
  });

  let mut qe = None;
  match event {
    Event::Member(member_event) => {
      let mut process = cmd.spawn()?;

      let mut stdin = process.take_stdin().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdin"))?;

      <T::Runtime as RuntimeLite>::spawn_detach(async move {
        member_event_stdin(&mut stdin, member_event).await;
      });
    },
    Event::User(ue) => {
      cmd.env("SERF_USER_EVENT", ue.name());
      cmd.env("SERF_USER_LTIME", format_smolstr!("{}", ue.ltime()));

      let mut process = cmd.spawn()?;

      let mut stdin = process.take_stdin().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdin"))?;

      <T::Runtime as RuntimeLite>::spawn_detach(async move {
        stream_payload(&mut stdin, ue.into_components().1).await;
      });
    },
    Event::Query(query_event) => {
      cmd.env("SERF_QUERY_NAME", query_event.name());
      cmd.env("SERF_QUERY_LTIME", format_smolstr!("{}", query_event.ltime()));

      let mut process = cmd.spawn()?;

      let mut stdin = process.take_stdin().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdin"))?;

      let payload = query_event.payload().clone();
      <T::Runtime as RuntimeLite>::spawn_detach(async move {
        stream_payload(&mut stdin, payload).await;
      });
      qe = Some(query_event);
    },
    _ => {
      tracing::warn!("unknown event type: {}", event.ty().as_str());
      return Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown event type"));
    },
  };

  // Execute command and capture output
  let output_result = cmd.output().await?;
  slow_timer.abort();

  // Write stdout and stderr to the output
  output.write_all(&output_result.stdout).await?;
  output.write_all(&output_result.stderr).await?;

  if output.written() > output.size() {
    tracing::warn!("agent: script '{}' generated {} bytes of output, truncated to {}", script, output.written(), output.size());
  }

  tracing::debug!(output = ?output, "agent: event '{}'", script);

  // If this is a query and we have output, respond
  if let Some(qe) = qe {
    if output.written() > 0 {
      let output = match output.read_to_bytes() {
        std::borrow::Cow::Borrowed(data) => Bytes::copy_from_slice(data),
        std::borrow::Cow::Owned(data) => data.into(),
      };
      if let Err(e) = qe.respond(output).await {
        tracing::warn!(name=%qe.name(), err=%e, "agent: failed to respond to query");
      }
    }
  }

  Ok(())
}

async fn member_event_stdin<I, A, W>(stdin: &mut W, e: MemberEvent<I, A>)
where
  W: futures::io::AsyncWrite + Unpin,
  I: core::fmt::Display,
  A: core::fmt::Display,
{
  for member in e.members() {
    // Format the tags as tag1=v1,tag2=v2,...
    let tags = member.tags().iter().map(|(k, v)| {
      format_smolstr!("{}={}", k, v)
    })
    .collect::<Vec<_>>().join(",");

    // Send the entire line
    let node = member.node();
    let line = format!(
      "{}\t{}\t{}\t{}\n",
      event_clean(Some(node.id().to_string())),
      node.address(),
      event_clean(member.tags().get("role").cloned()),
      event_clean(Some(tags))
    );

    if let Err(e) = stdin.write_all(line.as_bytes()).await {
      tracing::error!(err=%e, "serf: error writing member event");

      if let Err(e) = stdin.flush().await {
        tracing::error!(err=%e, "serf: error flushing member event");
      }
    }    

    let _ = stdin.close().await;
  }
}

fn event_clean(s: Option<impl Into<SmolStr>>) -> SmolStr {
  match s {
    Some(s) => {
      s.into().replace_smolstr("\t", "\\t").replace_smolstr("\n", "\\n")
    },
    None => SmolStr::default(),
  }
}

/// Sends data on stdin for an event. The stdin simply contains the
/// payload (if any).
/// Most shells read implementations need a newline, force it to be there
async fn stream_payload<W>(stdin: &mut W, buf: Bytes)
where
  W: futures::io::AsyncWrite + Unpin,
{
  const MAX_INLINE_SIZE: usize = 1024;

  macro_rules! write_payload {
    ($io:ident($buf: expr)) => {
      if let Err(e) = $io.write_all($buf).await {
        tracing::error!(err=%e, "serf: error writing payload");

        if let Err(e) = $io.flush().await {
          tracing::error!(err=%e, "serf: error flushing payload");
        }
      }
    };
  }

  // Append a newline to payload if missing
  let len = buf.len();
  if len > 0 && buf[len - 1] != b'\n' {
    if len + 1 > MAX_INLINE_SIZE {
      let mut buffer = Vec::from(buf);
      buffer.push(b'\n');
      write_payload!(stdin(&buffer));
    } else {
      let mut buffer = [0; MAX_INLINE_SIZE];
      buffer[..len].copy_from_slice(&buf);
      buffer[len] = b'\n';

      write_payload!(stdin(&buffer[..len + 1]));
    }
  } else if let Err(e) = stdin.write_all(&buf).await {
    tracing::error!(err=%e,"error writing payload");
  }

  let _ = stdin.close().await;
}

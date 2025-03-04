use std::{net::SocketAddr, sync::Arc};

use bincode::{deserialize, serialize};
use clap::Parser;
use crossbeam_skiplist::SkipMap;
use serf::{
  MemberlistOptions, Options,
  agnostic::tokio::TokioRuntime,
  delegate::CompositeDelegate,
  net::{
    NetTransportOptions, Node, NodeId, resolver::socket_addr::SocketAddrResolver,
    stream_layer::tcp::Tcp,
  },
  tokio::TokioTcpSerf,
  types::{MaybeResolvedAddress, SmolStr},
};

use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::{UnixListener, UnixStream},
  sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
  },
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

type ConsulDelegate = CompositeDelegate<NodeId, SocketAddr>;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Service {
  name: SmolStr,
  addr: SocketAddr,
}

struct Inner {
  serf: TokioTcpSerf<NodeId, SocketAddrResolver<TokioRuntime>, ConsulDelegate>,
  services: SkipMap<SmolStr, Service>,
  tx: UnboundedSender<Event>,
}

#[derive(Clone)]
struct ToyConsul {
  inner: Arc<Inner>,
}

impl ToyConsul {
  async fn new(
    opts: Options,
    net_opts: NetTransportOptions<NodeId, SocketAddrResolver<TokioRuntime>, Tcp<TokioRuntime>>,
  ) -> Result<Self> {
    let serf = TokioTcpSerf::new(net_opts, opts.with_event_buffer_size(256)).await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    let this = Self {
      inner: Inner {
        serf,
        services: SkipMap::new(),
        tx,
      }
      .into(),
    };

    this.clone().handle_events(rx);

    Ok(this)
  }

  fn handle_events(self, mut rx: UnboundedReceiver<Event>) {
    tokio::spawn(async move {
      loop {
        tokio::select! {
          _ = tokio::signal::ctrl_c() => {
            tracing::info!("toyconsul: shutting down event listener");
          }
          ev = rx.recv() => {
            if let Some(ev) = ev {
              match ev {
                Event::Join { id, addr, tx } => {
                  let res = self.inner.serf.join(Node::new(id, MaybeResolvedAddress::Resolved(addr)), false).await;
                  let _ = tx.send(res.map_err(Into::into).map(|_| ()));
                }
                Event::Register { name, addr, tx } => {
                  self.inner.services.insert(
                    name.clone(),
                    Service {
                      name,
                      addr,
                    });
                  let _ = tx.send(Ok(()));
                }
                Event::List { tx } => {
                  let services = self.inner.services.iter().map(|ent| ent.value().clone()).collect();
                  let _ = tx.send(Ok(services));
                }
              }
            }
          }
        }
      }
    });
  }

  async fn handle_register<W: tokio::io::AsyncWrite + Unpin>(
    &self,
    name: SmolStr,
    addr: SocketAddr,
    stream: &mut W,
  ) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    if let Err(e) = self.inner.tx.send(Event::Register { name, addr, tx }) {
      tracing::error!(err=%e, "toyconsul: fail to send get event");
      return Ok(());
    }

    let resp = rx.await?;
    tracing::info!(value=?resp, "toyconsul: fetch key");
    match bincode::serialize(&resp.map_err(|e| e.to_string())) {
      Ok(resp) => {
        let mut prefixed_data = vec![0; resp.len() + 4];
        prefixed_data[..4].copy_from_slice(&(resp.len() as u32).to_le_bytes());
        prefixed_data[4..].copy_from_slice(&resp);
        if let Err(e) = stream.write_all(&prefixed_data).await {
          tracing::error!(err=%e, "toyconsul: fail to write rpc response");
        } else {
          tracing::info!(data=?prefixed_data, "toyconsul: send get response");
        }
      }
      Err(e) => {
        tracing::error!(err=%e, "toyconsul: fail to encode rpc response");
      }
    }
    Ok(())
  }

  async fn handle_join<W: tokio::io::AsyncWrite + Unpin>(
    &self,
    id: NodeId,
    addr: SocketAddr,
    stream: &mut W,
  ) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.inner.tx.send(Event::Join {
      id: id.clone(),
      addr,
      tx,
    })?;

    let resp = rx.await?;
    if let Err(e) = resp {
      let res = std::result::Result::<(), String>::Err(e.to_string());
      match bincode::serialize(&res) {
        Ok(resp) => {
          let mut prefixed_data = vec![0; resp.len() + 4];
          prefixed_data[..4].copy_from_slice(&(resp.len() as u32).to_le_bytes());
          prefixed_data[4..].copy_from_slice(&resp);
          if let Err(e) = stream.write_all(&prefixed_data).await {
            tracing::error!(err=%e, "toyconsul: fail to write rpc response");
          }
        }
        Err(e) => {
          tracing::error!(err=%e, "toyconsul: fail to encode rpc response");
        }
      }
    } else {
      let res = std::result::Result::<(), String>::Ok(());
      match bincode::serialize(&res) {
        Ok(resp) => {
          let mut prefixed_data = vec![0; resp.len() + 4];
          prefixed_data[..4].copy_from_slice(&(resp.len() as u32).to_le_bytes());
          prefixed_data[4..].copy_from_slice(&resp);
          if let Err(e) = stream.write_all(&prefixed_data).await {
            tracing::error!(err=%e, "toyconsul: fail to write rpc response");
          }
        }
        Err(e) => {
          tracing::error!(err=%e, "toyconsul: fail to encode rpc response");
        }
      }
    }

    Ok(())
  }

  async fn handle_list<W: tokio::io::AsyncWrite + Unpin>(&self, stream: &mut W) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self.inner.tx.send(Event::List { tx })?;

    let resp = rx.await?;

    match resp {
      Ok(s) => {
        let res = std::result::Result::<Vec<Service>, String>::Ok(s);
        match bincode::serialize(&res) {
          Ok(resp) => {
            let mut prefixed_data = vec![0; resp.len() + 4];
            prefixed_data[..4].copy_from_slice(&(resp.len() as u32).to_le_bytes());
            prefixed_data[4..].copy_from_slice(&resp);
            if let Err(e) = stream.write_all(&prefixed_data).await {
              tracing::error!(err=%e, "toyconsul: fail to write rpc response");
            }
          }
          Err(e) => {
            tracing::error!(err=%e, "toyconsul: fail to encode rpc response");
          }
        }
      }
      Err(e) => {
        let res = std::result::Result::<(), String>::Err(e.to_string());
        match bincode::serialize(&res) {
          Ok(resp) => {
            let mut prefixed_data = vec![0; resp.len() + 4];
            prefixed_data[..4].copy_from_slice(&(resp.len() as u32).to_le_bytes());
            prefixed_data[4..].copy_from_slice(&resp);
            if let Err(e) = stream.write_all(&prefixed_data).await {
              tracing::error!(err=%e, "toyconsul: fail to write rpc response");
            }
          }
          Err(e) => {
            tracing::error!(err=%e, "toyconsul: fail to encode rpc response");
          }
        }
      }
    }
    Ok(())
  }
}

#[derive(clap::Args)]
struct StartArgs {
  /// The id of the db instance
  #[clap(short, long)]
  id: NodeId,
  /// The address the memberlist should bind to
  #[clap(short, long)]
  addr: SocketAddr,
  /// The rpc address to listen on commands
  #[clap(short, long)]
  rpc_addr: std::path::PathBuf,
}

#[derive(clap::Subcommand)]
enum Commands {
  /// Start the toyconsul instance
  Start(StartArgs),
  /// Join to an existing toyconsul cluster
  Join {
    #[clap(short, long)]
    id: NodeId,
    #[clap(short, long)]
    addr: SocketAddr,
    #[clap(short, long)]
    rpc_addr: std::path::PathBuf,
  },
  /// Register a service to the toyconsul
  Register {
    #[clap(short, long)]
    name: String,
    #[clap(short, long)]
    addr: SocketAddr,
    #[clap(short, long)]
    rpc_addr: std::path::PathBuf,
  },
  /// List all services in the toyconsul
  List,
}

#[derive(clap::Parser)]
#[command(name = "toyconsul")]
#[command(about = "CLI for toyconsul example", long_about = None)]
struct Cli {
  #[clap(subcommand)]
  command: Commands,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum Op {
  Register { name: SmolStr, addr: SocketAddr },
  List,
  Join { addr: SocketAddr, id: NodeId },
}

enum Event {
  Register {
    name: SmolStr,
    addr: SocketAddr,
    tx: oneshot::Sender<Result<()>>,
  },
  List {
    tx: oneshot::Sender<Result<Vec<Service>>>,
  },
  Join {
    addr: SocketAddr,
    id: NodeId,
    tx: oneshot::Sender<Result<()>>,
  },
}

#[tokio::main]
async fn main() -> Result<()> {
  let filter = std::env::var("TOY_CONSUL_LOG").unwrap_or_else(|_| "info".to_owned());
  tracing::subscriber::set_global_default(
    tracing_subscriber::fmt::fmt()
      .without_time()
      .with_line_number(true)
      .with_env_filter(filter)
      .with_file(false)
      .with_target(true)
      .with_ansi(true)
      .finish(),
  )
  .unwrap();

  let cli = Cli::parse();
  match cli.command {
    Commands::Join { addr, id, rpc_addr } => {
      handle_join_cmd(id, addr, rpc_addr).await?;
    }
    Commands::Register {
      name,
      addr,
      rpc_addr,
    } => {
      handle_register_cmd(name, addr, rpc_addr).await?;
    }
    Commands::Start(args) => {
      handle_start_cmd(args).await?;
    }
    Commands::List => {
      handle_list_cmd().await?;
    }
  }

  Ok(())
}

async fn handle_join_cmd(id: NodeId, addr: SocketAddr, rpc_addr: std::path::PathBuf) -> Result<()> {
  let conn = UnixStream::connect(rpc_addr).await?;
  let data = serialize(&Op::Join { id, addr })?;

  let (reader, mut writer) = conn.into_split();

  let mut prefixed_data = vec![0; data.len() + 4];
  prefixed_data[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
  prefixed_data[4..].copy_from_slice(&data);

  writer.write_all(&prefixed_data).await?;
  writer.shutdown().await?;

  let mut reader = tokio::io::BufReader::new(reader);
  let mut len_buf = [0; 4];
  reader.read_exact(&mut len_buf).await?;
  let len = u32::from_le_bytes(len_buf) as usize;

  let mut buf = vec![0; len];
  reader.read_exact(&mut buf).await?;
  let res = deserialize::<std::result::Result<(), String>>(&buf)?;
  match res {
    Ok(_) => {
      println!("join successfully");
    }
    Err(e) => {
      println!("fail to join {e}")
    }
  }
  Ok(())
}

async fn handle_register_cmd(
  name: String,
  addr: SocketAddr,
  rpc_addr: std::path::PathBuf,
) -> Result<()> {
  let conn = UnixStream::connect(rpc_addr).await?;
  let data = serialize(&Op::Register {
    name: name.clone().into(),
    addr,
  })?;

  let (reader, mut writer) = conn.into_split();

  let mut prefixed_data = vec![0; data.len() + 4];
  prefixed_data[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
  prefixed_data[4..].copy_from_slice(&data);

  writer.write_all(&prefixed_data).await?;
  writer.shutdown().await?;

  let mut reader = tokio::io::BufReader::new(reader);
  let mut len_buf = [0; 4];
  reader.read_exact(&mut len_buf).await?;
  let len = u32::from_le_bytes(len_buf) as usize;

  let mut buf = vec![0; len];
  reader.read_exact(&mut buf).await?;
  let res = deserialize::<std::result::Result<(), String>>(&buf)?;
  match res {
    Ok(_) => {
      println!("register {}({}) successfully", name, addr);
    }
    Err(e) => {
      println!("fail to register {e}");
    }
  }
  Ok(())
}

async fn handle_start_cmd(args: StartArgs) -> Result<()> {
  let opts = Options::new().with_memberlist_options(MemberlistOptions::local());
  let net_opts =
    NetTransportOptions::new(args.id).with_bind_addresses([args.addr].into_iter().collect());

  let consul = ToyConsul::new(opts, net_opts).await?;

  struct Guard {
    sock: std::path::PathBuf,
  }

  impl Drop for Guard {
    fn drop(&mut self) {
      if let Err(e) = std::fs::remove_file(&self.sock) {
        tracing::error!(err=%e, "toyconsul: fail to remove rpc sock");
      }
    }
  }

  let _guard = Guard {
    sock: args.rpc_addr.clone(),
  };

  let listener = UnixListener::bind(&args.rpc_addr)?;

  tracing::info!("toyconsul: start listening on {}", args.rpc_addr.display());

  loop {
    tokio::select! {
      conn = listener.accept() => {
        let (stream, _) = conn?;
        let mut stream = tokio::io::BufReader::new(stream);
        let mut len_buf = [0; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_le_bytes(len_buf) as usize;

        let mut data = vec![0; len];
        if let Err(e) = stream.read_exact(&mut data).await {
          tracing::error!(err=%e, "toyconsul: fail to read from rpc stream");
          continue;
        }

        let op: Op = match bincode::deserialize(&data) {
          Ok(op) => op,
          Err(e) => {
            tracing::error!(err=%e, "toyconsul: fail to decode rpc message");
            continue;
          }
        };

        match op {
          Op::Join { addr, id } => {
            consul.handle_join(id, addr, &mut stream).await?;
          }
          Op::Register {
            name,
            addr,
          } => {
            consul.handle_register(name, addr, &mut stream).await?;
          },
          Op::List => {
            consul.handle_list(&mut stream).await?;
          }
        }

        if let Err(e) = stream.into_inner().shutdown().await {
          tracing::error!(err=%e, "toyconsul: fail to shutdown rpc stream");
        }
      }
      _ = tokio::signal::ctrl_c() => {
        break;
      }
    }
  }
  Ok(())
}

async fn handle_list_cmd() -> Result<()> {
  let conn = UnixStream::connect("/tmp/toyconsul.sock").await?;
  let data = serialize(&Op::List)?;

  let (reader, mut writer) = conn.into_split();

  let mut prefixed_data = vec![0; data.len() + 4];
  prefixed_data[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
  prefixed_data[4..].copy_from_slice(&data);

  writer.write_all(&prefixed_data).await?;
  writer.shutdown().await?;

  let mut reader = tokio::io::BufReader::new(reader);
  let mut len_buf = [0; 4];
  reader.read_exact(&mut len_buf).await?;
  let len = u32::from_le_bytes(len_buf) as usize;

  let mut buf = vec![0; len];
  reader.read_exact(&mut buf).await?;
  let res = deserialize::<std::result::Result<Vec<Service>, String>>(&buf)?;
  match res {
    Ok(services) => {
      for service in services {
        println!("{}({})", service.name, service.addr);
      }
    }
    Err(e) => {
      println!("fail to list {e}")
    }
  }
  Ok(())
}

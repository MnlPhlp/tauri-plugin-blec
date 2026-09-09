//! blec stress server: a scriptable "chaos" BLE GATT peripheral for stress-testing
//! the tauri-plugin-blec client. See README.md and ../stress-protocol.md.

mod control;
mod gatt;
mod script;

use std::{sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use clap::Parser;
use control::{log, Config, ServerState};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    time::sleep,
};

#[derive(Parser, Debug)]
#[command(
    name = "stress-server",
    about = "Chaos BLE GATT peripheral for tauri-plugin-blec"
)]
struct Cli {
    /// Advertised local name.
    #[arg(long, default_value = "blec_stress")]
    name: String,

    /// Bluetooth adapter to use (e.g. hci0). Defaults to the first adapter.
    #[arg(long)]
    adapter: Option<String>,

    /// Interval between COUNTER notifications while a client is subscribed.
    #[arg(long, default_value_t = 100)]
    counter_interval_ms: u64,

    /// Size in bytes of the LARGE characteristic value.
    #[arg(long, default_value_t = 512)]
    large_size: usize,
}

// current_thread on purpose: bluer spawns one task per incoming D-Bus method
// call, and only the current-thread scheduler polls spawned tasks in spawn
// order. On the multi-thread runtime two write-without-response commands that
// arrive back to back may be processed in swapped order, even with the
// `echo_lock` (the lock is fair, but the tasks race to reach it).
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // bluer logs through the `log` crate; enable with e.g. RUST_LOG=bluer=debug.
    env_logger::init();
    let cli = Cli::parse();
    if cli.counter_interval_ms == 0 {
        bail!("--counter-interval-ms must be > 0");
    }

    let session = bluer::Session::new()
        .await
        .context("connecting to bluetoothd")?;
    let adapter = match &cli.adapter {
        Some(name) => session.adapter(name)?,
        None => session.default_adapter().await?,
    };
    adapter
        .set_powered(true)
        .await
        .context("powering adapter")?;
    log(
        "info",
        format!(
            "adapter {} address {}",
            adapter.name(),
            adapter.address().await?
        ),
    );

    let state = ServerState::new(
        adapter,
        Config {
            name: cli.name.clone(),
            counter_interval: Duration::from_millis(cli.counter_interval_ms),
            large_size: cli.large_size,
        },
    );
    state.start().await?;

    // Follow BlueZ device objects so connect/disconnect is logged without traffic.
    {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = control::watch_devices(state).await {
                log("error", format!("device watcher stopped: {err:#}"));
            }
        });
    }
    // The device watcher can miss `Connected` changes; poll as a fallback.
    tokio::spawn(control::poll_central_liveness(state.clone()));

    log("info", "ready; type `help` for REPL commands");
    repl(state.clone()).await;

    log("info", "shutting down");
    let _ = state.script.abort();
    state.shutdown().await;
    sleep(Duration::from_millis(500)).await;
    Ok(())
}

// -----------------------------------------------------------------------------------------------
// REPL
// -----------------------------------------------------------------------------------------------

const HELP: &str = "\
commands:
  drop                       disconnect the connected central
  hide <secs>                stop advertising for <secs> seconds
  show                       start advertising again
  storm <count> <interval_ms> send <count> extra COUNTER notifications
  slow <ms>                  delay every response by <ms> (0 = off)
  fail <count>               fail the next <count> read/write requests
  restart <secs>             drop link, unregister GATT app + advertising, re-register after <secs>
  script <id>                start integration script <id> (1-7), aborting a running one
  abort                      abort the running script
  report                     print the REPORT JSON
  status                     print the STATUS JSON
  help                       this text
  quit                       unregister everything and exit";

/// Read commands from stdin until `quit` or EOF. On EOF (no terminal) the server keeps
/// running forever so it can be driven over CONTROL alone.
async fn repl(state: Arc<ServerState>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                log("info", "stdin closed; running until killed");
                std::future::pending::<()>().await;
                unreachable!();
            }
            Err(err) => {
                log("error", format!("stdin: {err}"));
                std::future::pending::<()>().await;
                unreachable!();
            }
        };
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        if words[0] == "quit" || words[0] == "exit" {
            return;
        }
        match run_command(&state, &words).await {
            Ok(Some(output)) => println!("{output}"),
            Ok(None) => println!("ok"),
            Err(err) => println!("error: {err:#}"),
        }
    }
}

fn arg<T: std::str::FromStr>(words: &[&str], idx: usize, what: &str) -> Result<T> {
    words
        .get(idx)
        .with_context(|| format!("missing <{what}>"))?
        .parse::<T>()
        .map_err(|_| anyhow::anyhow!("invalid <{what}>: {}", words[idx]))
}

/// Execute one REPL command. `Ok(Some(text))` prints text, `Ok(None)` prints `ok`.
async fn run_command(state: &Arc<ServerState>, words: &[&str]) -> Result<Option<String>> {
    match words[0] {
        "drop" => {
            state.drop_link(None).await?;
        }
        "hide" => state.hide(arg(words, 1, "secs")?).await?,
        "show" => state.show().await?,
        "storm" => {
            let count: u32 = arg(words, 1, "count")?;
            let interval: u64 = arg(words, 2, "interval_ms")?;
            // The outcome is logged by the COUNTER session (`storm | done ...`).
            drop(state.storm(count, Duration::from_millis(interval)).await?);
        }
        "slow" => state.set_slow(arg(words, 1, "ms")?),
        "fail" => state.fail_next(arg(words, 1, "count")?),
        "restart" => {
            state.restart(arg(words, 1, "secs")?).await?;
        }
        "script" => state.script.start(state, arg(words, 1, "id")?)?,
        "abort" => state.script.abort()?,
        "report" => return Ok(Some(state.script.report_json())),
        "status" => return Ok(Some(state.status_json())),
        "help" => return Ok(Some(HELP.to_string())),
        other => bail!("unknown command `{other}` (try `help`)"),
    }
    Ok(None)
}

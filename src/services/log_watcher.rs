use std::{
    fs,
    fs::File,
    io,
    io::BufRead,
    path::Path,
    sync::{mpsc::channel, Arc},
    thread::sleep,
    time::Duration,
};

use notify::{DebouncedEvent, RecursiveMode, Watcher};
use tokio::runtime::Runtime;

use crate::{error::Result, rpc::rpc_client, services, Enforcer};

use tokio::sync::RwLock;

pub fn watch(enforcer: &Arc<RwLock<Enforcer>>) {
    debug!("Starting dnsmasq.log watcher");
    let (tx, rx) = channel();
    let mut watcher = notify::watcher(tx, Duration::from_secs(10)).unwrap();

    let path: &Path;
    let tmp_path: &Path;
    if services::is_system_mode() {
        path = "/tmp/dnsmasq.log".as_ref();
        tmp_path = "/tmp/dnsmasq.log.tmp".as_ref();
    } else {
        path = "dnsmasq.log".as_ref();
        tmp_path = "dnsmasq.log.tmp".as_ref();
    };
    if !path.is_file() {
        warn!("Skipping watching dnsmasq.log, since dnsmasq is either not running or wrongly configured");
        return;
    }
    if let Err(e) = read_log_file(&enforcer, path, tmp_path) {
        warn!("failed to process file {}", e);
    }
    loop {
        if let Err(e) = watcher.watch(path, RecursiveMode::NonRecursive) {
            warn!("Failed to watch dnsmasq.log! {}", e);
            sleep(Duration::from_secs(10));
            continue;
        }

        loop {
            match rx.recv() {
                Ok(DebouncedEvent::Write(_)) | Ok(DebouncedEvent::NoticeWrite(_)) => {
                    // inner function to make use of Result
                    if let Err(e) = read_log_file(&enforcer, path, tmp_path) {
                        warn!("failed to process file {}", e);
                    }
                },
                Ok(_) => {},
                Err(e) => warn!("watch error: {}", e),
            }
        }
    }
}

fn read_log_file(enforcer: &Arc<RwLock<Enforcer>>, path: &Path, tmp_path: &Path) -> Result<()> {
    debug!("reading dnsmasq log file");
    fs::rename(path, tmp_path)?;
    let lines = io::BufReader::new(File::open(tmp_path)?).lines();
    // create async runtime to run rpc client
    Runtime::new()?.block_on(async {
        let mut enforcer = enforcer.write().await;
        debug!("acquired known devices");
        let lines = lines
            .filter(|l| {
                if let Ok(l) = l {
                    enforcer
                        .config
                        .known_devices()
                        .iter()
                        .filter(|d| d.collect_data)
                        .any(|d| l.contains(&d.ip.to_string()))
                } else {
                    false
                }
            })
            .collect::<io::Result<_>>()?;
        enforcer
            .client
            .send_logs(rpc_client::current_rpc_context(), lines)
            .await
    })?;
    Ok(())
}

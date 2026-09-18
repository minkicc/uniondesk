//! End to end test: two engines find each other over the loopback interface,
//! complete the Noise handshake, pair with a code, and reach the connected
//! state with a usable screen edge.
//!
//! Each engine gets its own configuration directory and an OS chosen port, so
//! the test is safe to run on a developer machine.

use std::path::PathBuf;
use std::time::Duration;

use ud_core::config::Settings;
use ud_engine::view::{ConnectionState, EngineEvent, Snapshot};
use ud_engine::{start, Command, EngineHandle, EngineOptions};

/// Minimal temporary directory helper so the test needs no extra dependencies.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = format!(
            "uniondesk-test-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct TestEngine {
    handle: EngineHandle,
    events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
    snapshot: Option<Snapshot>,
    _dir: TempDir,
}

impl TestEngine {
    async fn spawn(name: &str) -> TestEngine {
        let dir = TempDir::new(name);
        let options = EngineOptions::in_dir(dir.path());
        let mut settings = Settings::default();
        settings.device_name = name.to_string();
        // Port 0 lets the OS choose, so parallel runs never collide, and
        // discovery is off so the test does not touch the local network.
        settings.port = 0;
        settings.discoverable = false;
        settings.transfer.download_dir = dir.path().join("downloads");
        settings.save(&options.settings_path).unwrap();

        let (handle, events) = start(options).unwrap();
        let mut engine = TestEngine {
            handle,
            events,
            snapshot: None,
            _dir: dir,
        };
        engine.snapshot = Some(engine.next_snapshot().await);
        engine
    }

    fn snap(&self) -> &Snapshot {
        self.snapshot.as_ref().expect("a snapshot has been received")
    }

    async fn next_snapshot(&mut self) -> Snapshot {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.events.recv()).await {
                Ok(Some(EngineEvent::Snapshot(snapshot))) => return *snapshot,
                Ok(Some(EngineEvent::Notice(notice))) => {
                    eprintln!("[engine notice] {notice:?}");
                    continue;
                }
                Ok(None) | Err(_) => panic!("engine stopped before producing a snapshot"),
            }
        }
    }

    async fn wait_until(
        &mut self,
        label: &str,
        mut predicate: impl FnMut(&Snapshot) -> bool,
    ) -> Snapshot {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        loop {
            if let Some(snapshot) = &self.snapshot {
                if predicate(snapshot) {
                    return snapshot.clone();
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {label}; last snapshot: {:#?}",
                self.snapshot
            );
            self.snapshot = Some(self.next_snapshot().await);
        }
    }

    /// Pushes whatever the engine sends through the current snapshot.
    async fn pump(&mut self) {
        self.snapshot = Some(self.next_snapshot().await);
    }
}

fn peer_state(snapshot: &Snapshot, peer: &str) -> Option<ConnectionState> {
    snapshot
        .peers
        .iter()
        .find(|view| view.device_id.0 == peer)
        .map(|view| view.connection.clone())
}

fn offered_code(snapshot: &Snapshot, peer: &str) -> Option<String> {
    snapshot
        .peers
        .iter()
        .find(|view| view.device_id.0 == peer)
        .and_then(|view| view.pairing.as_ref())
        .and_then(|pairing| pairing.code_to_share.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_pair_and_connect() {
    let mut alpha = TestEngine::spawn("alpha").await;
    let mut beta = TestEngine::spawn("beta").await;

    alpha
        .wait_until("alpha to listen", |snapshot| snapshot.listening_port > 0)
        .await;
    beta.wait_until("beta to listen", |snapshot| snapshot.listening_port > 0)
        .await;

    let alpha_id = alpha.snap().device.device_id.0.clone();
    let beta_id = beta.snap().device.device_id.0.clone();
    let beta_port = beta.snap().listening_port;
    assert_ne!(alpha_id, beta_id);

    alpha
        .handle
        .send(Command::ConnectAddress {
            address: format!("127.0.0.1:{beta_port}").parse().unwrap(),
            name: Some("beta".into()),
        })
        .unwrap();

    // Exactly one side displays a code; the other has to type it in.
    let mut code = None;
    let mut shown_by_alpha = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while code.is_none() {
        if let Some(value) = offered_code(alpha.snap(), &beta_id) {
            code = Some(value);
            shown_by_alpha = true;
            break;
        }
        if let Some(value) = offered_code(beta.snap(), &alpha_id) {
            code = Some(value);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no side offered a pairing code; alpha={:#?} beta={:#?}",
            alpha.snap().control,
            beta.snap().control
        );
        alpha.pump().await;
        beta.pump().await;
    }
    let code = code.unwrap();
    assert_eq!(code.len(), 6, "pairing codes are six digits");

    let (typer, typer_peer) = if shown_by_alpha {
        // Alpha shows the code, so beta has to answer alpha.
        (&beta, ud_core::identity::DeviceId(alpha_id.clone()))
    } else {
        (&alpha, ud_core::identity::DeviceId(beta_id.clone()))
    };
    typer
        .handle
        .send(Command::AnswerPairing {
            peer: typer_peer,
            code: Some(code),
            accept: true,
        })
        .unwrap();

    alpha
        .wait_until("alpha to pair", |snapshot| {
            matches!(
                peer_state(snapshot, &beta_id),
                Some(ConnectionState::Connected)
            )
        })
        .await;
    beta.wait_until("beta to pair", |snapshot| {
        matches!(
            peer_state(snapshot, &alpha_id),
            Some(ConnectionState::Connected)
        )
    })
    .await;

    let view = alpha
        .snap()
        .peers
        .iter()
        .find(|peer| peer.device_id.0 == beta_id)
        .expect("beta should be listed");
    assert!(view.trusted, "a paired machine is trusted");
    assert!(
        view.link.is_some(),
        "pairing assigns a default screen edge so the machine is usable at once"
    );
    assert_eq!(
        peer_state(beta.snap(), &alpha_id),
        Some(ConnectionState::Connected),
        "both sides agree the pairing succeeded"
    );
}

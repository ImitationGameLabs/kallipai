//! UDS accept loop: one short connection, one request line, one response
//! line. Every handler is answered from a fresh directory scan; the daemon
//! keeps no in-memory registry (the tree is the truth).

use std::path::PathBuf;

use kallip_daemon_common::wire::{
    ErrorCode, InstanceState, MAX_LINE_BYTES, OkPayload, PROTOCOL_VERSION, RequestBody, Response,
    decode_request, encode_response, err, ok,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt as _, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::scan;

/// Shared handler context for one daemon process.
#[derive(Debug, Clone)]
pub struct Daemon {
    pub record_root: PathBuf,
}

impl Daemon {
    pub fn new(record_root: PathBuf) -> Self {
        Self { record_root }
    }

    /// Serve requests on `listener` until the process is stopped.
    pub async fn serve(self, listener: UnixListener) -> anyhow::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            // SO_PEERCRED: the kernel answers who is on the other end.
            // The 0600 socket already gates access; the uid becomes a
            // spawned instance's owner so a multi-user install can account
            // instances per requesting peer.
            let peer_uid = match stream.peer_cred() {
                Ok(cred) => cred.uid(),
                Err(error) => {
                    tracing::warn!(%error, "peer credentials unavailable");
                    continue;
                }
            };
            let daemon = self.clone();
            tokio::spawn(async move {
                if let Err(error) = daemon.handle(stream, peer_uid).await {
                    // A client that hangs up mid-exchange is routine
                    // (panel polling, a ctrl-c'd kallipctl), not a
                    // daemon problem: keep it out of the warn stream.
                    let gone = error
                        .root_cause()
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| {
                            matches!(
                                e.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::NotConnected
                            )
                        });
                    if gone {
                        tracing::debug!(%error, "peer went away mid-exchange");
                    } else {
                        tracing::warn!(%error, "connection handler failed");
                    }
                }
            });
        }
    }

    async fn handle(&self, stream: tokio::net::UnixStream, peer_uid: u32) -> anyhow::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let reader = BufReader::new(reader);
        // Cap the request line at the protocol limit: `take` bounds the
        // read, so a runaway client streaming bytes cannot grow memory
        // unbounded — an over-long line reads back truncated (no newline)
        // and fails to parse, which answers bad_request.
        let mut reader = reader.take(MAX_LINE_BYTES as u64 + 1);
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // connected and left: nothing to answer
        }
        let response = match decode_request(line.trim_end()) {
            Ok(request) if request.v == PROTOCOL_VERSION => {
                self.dispatch(request.body, peer_uid).await
            }
            Ok(_) => err(ErrorCode::BadRequest, "unsupported protocol version"),
            Err(error) => err(
                ErrorCode::BadRequest,
                format!("unparseable request: {error}"),
            ),
        };
        let mut out = encode_response(&response)?.into_bytes();
        out.push(b'\n');
        writer.write_all(&out).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn dispatch(&self, body: RequestBody, peer_uid: u32) -> Response {
        let instances = scan::scan_instances(&self.record_root);
        match body {
            RequestBody::List => ok(OkPayload::List {
                instances: instances.iter().map(|i| i.info()).collect(),
            }),
            RequestBody::Health { slug: None } => ok(OkPayload::Health {
                report: kallip_daemon_common::wire::HealthReport {
                    slug: None,
                    running: true,
                    state: InstanceState::Running,
                    detail: None,
                },
            }),
            RequestBody::Health { slug: Some(slug) } => {
                match instances.iter().find(|i| i.slug == slug) {
                    Some(instance) => ok(OkPayload::Health {
                        report: instance.health(),
                    }),
                    None => err(ErrorCode::NotFound, format!("no instance named {slug}")),
                }
            }
            RequestBody::Spawn {
                slug,
                workspace,
                env,
                exe,
                user,
            } => {
                let slug_out = slug.clone();
                // Blocking work on the connection task: spawn waits up to
                // 30s for the pidfile; spawn_blocking keeps the runtime free.
                let timeout = std::time::Duration::from_secs(30);
                match tokio::task::spawn_blocking({
                    let record_root = self.record_root.clone();
                    move || {
                        let slug = slug.clone();
                        crate::spawn::spawn(
                            &record_root,
                            &slug,
                            &workspace,
                            &env,
                            exe.as_deref(),
                            timeout,
                            peer_uid,
                            user.as_deref(),
                        )
                    }
                })
                .await
                {
                    Ok(Ok((pid, port))) => ok(OkPayload::Spawn {
                        slug: slug_out,
                        pid,
                        port,
                    }),
                    Ok(Err(error)) => {
                        log_spawn_outcome(&slug_out, &error);
                        spawn_error_response(&error)
                    }
                    Err(join_error) => {
                        err(ErrorCode::Internal, format!("spawn task: {join_error}"))
                    }
                }
            }
            RequestBody::Stop { slug } => {
                let slug_out = slug.clone();
                match tokio::task::spawn_blocking({
                    let record_root = self.record_root.clone();
                    move || crate::stop::stop(&record_root, &slug, peer_uid)
                })
                .await
                {
                    Ok(Ok(())) => ok(OkPayload::Stop { slug: slug_out }),
                    Ok(Err(error)) => {
                        let code = kallip_daemon_common::wire::ErrorCode::from(&error);
                        err(code, error.to_string())
                    }
                    Err(join_error) => err(ErrorCode::Internal, format!("stop task: {join_error}")),
                }
            }
            RequestBody::Start { slug, env, exe } => {
                let slug_out = slug.clone();
                // Same blocking profile as spawn: start waits up to 30s for
                // the relaunched tagma's runtime.json.
                let timeout = std::time::Duration::from_secs(30);
                match tokio::task::spawn_blocking({
                    let record_root = self.record_root.clone();
                    move || {
                        crate::start::start(
                            &record_root,
                            &slug,
                            &env,
                            exe.as_deref(),
                            timeout,
                            &crate::scan::pid_is_alive,
                            peer_uid,
                        )
                    }
                })
                .await
                {
                    Ok(Ok((pid, port))) => ok(OkPayload::Spawn {
                        slug: slug_out,
                        pid,
                        port,
                    }),
                    Ok(Err(error)) => {
                        log_start_outcome(&slug_out, &error);
                        let code = kallip_daemon_common::wire::ErrorCode::from(&error);
                        err(code, error.to_string())
                    }
                    Err(join_error) => {
                        err(ErrorCode::Internal, format!("start task: {join_error}"))
                    }
                }
            }
            RequestBody::Adopt {
                slug,
                workspace,
                data_dir,
                env,
                user,
                accept_local_only,
            } => {
                let slug_out = slug.clone();
                // Pure registration still touches the passwd db, the
                // record area, and /proc: same blocking profile as spawn.
                match tokio::task::spawn_blocking({
                    let record_root = self.record_root.clone();
                    move || {
                        crate::adopt::adopt(
                            &record_root,
                            &slug,
                            &workspace,
                            &data_dir,
                            &env,
                            peer_uid,
                            user.as_deref(),
                            accept_local_only,
                        )
                    }
                })
                .await
                {
                    Ok(Ok(state)) => ok(OkPayload::Adopt {
                        slug: slug_out,
                        state,
                    }),
                    Ok(Err(error)) => spawn_error_response(&error),
                    Err(join_error) => {
                        err(ErrorCode::Internal, format!("adopt task: {join_error}"))
                    }
                }
            }
            RequestBody::Remove { slug } => {
                let slug_out = slug.clone();
                match tokio::task::spawn_blocking({
                    let record_root = self.record_root.clone();
                    move || crate::remove::remove(&record_root, &slug, peer_uid)
                })
                .await
                {
                    Ok(Ok(())) => ok(OkPayload::Remove { slug: slug_out }),
                    Ok(Err(error)) => {
                        let code = kallip_daemon_common::wire::ErrorCode::from(&error);
                        err(code, error.to_string())
                    }
                    Err(join_error) => {
                        err(ErrorCode::Internal, format!("remove task: {join_error}"))
                    }
                }
            }
            RequestBody::Log {
                slug,
                lines,
                file,
                cursor,
            } => {
                // Read-only, but still file I/O plus the passwd
                // lookup behind the owner-aware log placement: the
                // same blocking profile as the other verbs.
                match tokio::task::spawn_blocking({
                    let record_root = self.record_root.clone();
                    move || {
                        // Placement follows the RECORD's target user:
                        // the daemon may run as root while the instance
                        // runs as someone else, so the daemon's own
                        // state home would read the wrong tree. A
                        // missing record stays NotFound before any
                        // passwd work; an unresolvable uid is an
                        // explicit error, never a fallback.
                        let logs_dir = crate::records::read_record(&record_root, &slug)
                            .ok_or_else(|| crate::log::LogError::NotFound(slug.clone()))
                            .and_then(|record| {
                                crate::reconcile::instance_logs_dir(
                                    record.target_uid,
                                    &slug,
                                    crate::reconcile::passwd_home,
                                )
                                .map_err(|error| crate::log::LogError::LogsHome(error.uid))
                            })?;
                        crate::log::tail(
                            &record_root,
                            &logs_dir,
                            &slug,
                            lines,
                            file.as_deref(),
                            cursor.as_ref(),
                            peer_uid,
                        )
                    }
                })
                .await
                {
                    Ok(Ok(outcome)) => ok(OkPayload::Log {
                        text: outcome.text,
                        next_cursor: outcome.next_cursor,
                    }),
                    Ok(Err(error)) => {
                        let code = kallip_daemon_common::wire::ErrorCode::from(&error);
                        err(code, error.to_string())
                    }
                    Err(join_error) => err(ErrorCode::Internal, format!("log task: {join_error}")),
                }
            }
        }
    }
}

fn spawn_error_response(error: &crate::spawn::SpawnError) -> Response {
    use crate::spawn::SpawnError;
    let code = match error {
        SpawnError::SlugTaken(_) => ErrorCode::SlugTaken,
        SpawnError::Denied { .. } => ErrorCode::Denied,
        SpawnError::Overlap { .. } => ErrorCode::WorkspaceOverlap,
        SpawnError::Invalid(_) => ErrorCode::InvalidSpawnInput,
        SpawnError::Timeout { .. } => ErrorCode::SpawnTimeout,
        SpawnError::Internal(_) => ErrorCode::Internal,
    };
    err(code, error.to_string())
}

/// Outcome logging for spawn/start at the dispatch layer — the slug is
/// only reliably known here (error variants carry it partially). Levels
/// follow the audit rubric: input rejections are routine business
/// answers (info), state conflicts are worth attention (warn), failed
/// actions need a human (error). Success is logged inside spawn/start,
/// where the pid and port are native.
fn log_spawn_outcome(slug: &str, error: &crate::spawn::SpawnError) {
    use crate::spawn::SpawnError;
    match error {
        SpawnError::Invalid(_) => {
            tracing::info!(slug = %slug, %error, "spawn refused: invalid request")
        }
        SpawnError::Denied { .. } => {
            tracing::info!(slug = %slug, %error, "spawn refused: denied")
        }
        SpawnError::SlugTaken(_) => {
            tracing::warn!(slug = %slug, %error, "spawn refused: slug already exists")
        }
        SpawnError::Overlap { .. } => {
            tracing::warn!(slug = %slug, %error, "spawn refused: workspace overlap")
        }
        SpawnError::Timeout { .. } | SpawnError::Internal(_) => {
            tracing::error!(slug = %slug, %error, "spawn failed")
        }
    }
}

/// Start's twin of [`log_spawn_outcome`]. AlreadyRunning is logged
/// inside start (with the recorded pid and its comm) and stays silent
/// here to keep one diagnostic line per refusal.
fn log_start_outcome(slug: &str, error: &crate::start::StartError) {
    use crate::start::StartError;
    match error {
        StartError::AlreadyRunning(_) => {}
        StartError::NotFound(_) => {
            tracing::info!(slug = %slug, %error, "start refused: no such instance")
        }
        StartError::Denied { .. } => {
            tracing::info!(slug = %slug, %error, "start refused: denied")
        }
        StartError::Invalid(_) => {
            tracing::info!(slug = %slug, %error, "start refused: invalid request")
        }
        StartError::Timeout { .. } | StartError::Internal(_) => {
            tracing::error!(slug = %slug, %error, "start failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_daemon_client::DaemonClient;

    #[tokio::test]
    async fn uds_round_trip_list_and_health() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = tempfile::tempdir().expect("data tempdir");
        crate::records::write_record(
            dir.path(),
            "alpha",
            &crate::records::InstanceRecord {
                instance_id: "id-1".into(),
                owner_uid: 1000,
                target_uid: 1000,
                target_username: None,
                workspace: Some("/tmp/w".into()),
                env: Vec::new(),
                identity: None,
                data_dir: data.path().to_path_buf(),
            },
        )
        .expect("write record");
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).expect("bind");

        let daemon = Daemon::new(dir.path().to_path_buf());
        let task = tokio::spawn(daemon.serve(listener));

        let client = DaemonClient::new(&socket);
        let list = client.call(RequestBody::List).await.expect("list");
        match list.body {
            kallip_daemon_common::wire::ResponseBody::Ok { payload } => {
                let kallip_daemon_common::wire::OkPayload::List { instances } = payload else {
                    panic!("expected list payload");
                };
                assert_eq!(instances.len(), 1);
                assert_eq!(instances[0].slug, "alpha");
                assert_eq!(instances[0].workspace, "/tmp/w");
                assert_eq!(instances[0].state, InstanceState::Stopped);
            }
            other => panic!("expected ok, got {other:?}"),
        }

        let health = client
            .call(RequestBody::Health {
                slug: Some("alpha".into()),
            })
            .await
            .expect("health");
        match health.body {
            kallip_daemon_common::wire::ResponseBody::Ok {
                payload: kallip_daemon_common::wire::OkPayload::Health { report },
            } => {
                assert_eq!(report.state, InstanceState::Stopped);
            }
            other => panic!("expected ok, got {other:?}"),
        }

        let missing = client
            .call(RequestBody::Health {
                slug: Some("nope".into()),
            })
            .await
            .expect("health missing");
        match missing.body {
            kallip_daemon_common::wire::ResponseBody::Err { code, .. } => {
                assert_eq!(code, ErrorCode::NotFound);
            }
            other => panic!("expected err, got {other:?}"),
        }

        // A deliberately bad line gets bad_request, not a dropped connection.
        let response = client
            .raw_line("{\"v\":1,\"type\":\"nonsense\"}")
            .await
            .expect("connection still answers");
        match response.body {
            kallip_daemon_common::wire::ResponseBody::Err { code, .. } => {
                assert_eq!(code, ErrorCode::BadRequest);
            }
            other => panic!("expected bad request, got {other:?}"),
        }

        task.abort();
    }
}

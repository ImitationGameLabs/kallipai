//! Wire-payload unwrapping shared by the backends: strip the daemon's
//! response tags and surface either the plain value or a classified
//! error the HTTP layer knows how to render.

use kallipai_daemon_client::ClientError;
use kallipai_daemon_common::wire::{ErrorCode, HealthReport, InstanceInfo, Response, ResponseBody};

use crate::api::{Spawned, Stopped};

/// A failed backend call, classified by what the HTTP layer should do
/// with it: a daemon refusal keeps the daemon's own error grammar; a
/// transport failure is the backend's own fault to report.
#[derive(Debug)]
pub enum BackendError {
    /// The daemon (or an equivalent backend) refused the request.
    Fault { code: ErrorCode, message: String },
    /// The backend itself could not be reached or failed mid-exchange.
    Transport(ClientError),
}

impl From<ClientError> for BackendError {
    fn from(error: ClientError) -> Self {
        BackendError::Transport(error)
    }
}

fn mismatch(want: &str) -> BackendError {
    BackendError::Fault {
        code: ErrorCode::Internal,
        message: format!("unexpected payload kind for {want}"),
    }
}

/// The daemon's tagged response → one plain spawn result.
pub fn unwrap_spawn(wire: Response) -> Result<Spawned, BackendError> {
    match wire.body {
        ResponseBody::Ok {
            payload: kallipai_daemon_common::wire::OkPayload::Spawn { slug, pid, port },
        } => Ok(Spawned { slug, pid, port }),
        ResponseBody::Ok { .. } => Err(mismatch("spawn")),
        ResponseBody::Err { code, message } => Err(BackendError::Fault { code, message }),
    }
}

/// The daemon's tagged response → one plain stop result.
pub fn unwrap_stop(wire: Response) -> Result<Stopped, BackendError> {
    match wire.body {
        ResponseBody::Ok {
            payload: kallipai_daemon_common::wire::OkPayload::Stop { slug },
        } => Ok(Stopped { slug }),
        ResponseBody::Ok { .. } => Err(mismatch("stop")),
        ResponseBody::Err { code, message } => Err(BackendError::Fault { code, message }),
    }
}

/// The daemon's tagged response → the instance list.
pub fn unwrap_list(wire: Response) -> Result<Vec<InstanceInfo>, BackendError> {
    match wire.body {
        ResponseBody::Ok {
            payload: kallipai_daemon_common::wire::OkPayload::List { instances },
        } => Ok(instances),
        ResponseBody::Ok { .. } => Err(mismatch("list")),
        ResponseBody::Err { code, message } => Err(BackendError::Fault { code, message }),
    }
}

/// The daemon's tagged response → one health report.
pub fn unwrap_health(wire: Response) -> Result<HealthReport, BackendError> {
    match wire.body {
        ResponseBody::Ok {
            payload: kallipai_daemon_common::wire::OkPayload::Health { report },
        } => Ok(report),
        ResponseBody::Ok { .. } => Err(mismatch("health")),
        ResponseBody::Err { code, message } => Err(BackendError::Fault { code, message }),
    }
}

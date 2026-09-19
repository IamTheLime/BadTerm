use std::io::{BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use rmpv::Value;
use thiserror::Error;

pub mod ui;

pub type RequestId = u64;

#[derive(Debug, Error)]
pub enum NvimError {
    #[error("spawn nvim: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("nvim did not provide stdin")]
    MissingStdin,
    #[error("nvim did not provide stdout")]
    MissingStdout,
    #[error("write Msgpack: {0}")]
    Write(#[source] std::io::Error),
    #[error("wait for nvim: {0}")]
    Wait(#[source] std::io::Error),
    #[error("encode Msgpack: {0}")]
    Encode(String),
    #[error("decode Msgpack: {0}")]
    Decode(String),
    #[error("Neovim event channel closed")]
    ChannelClosed,
}

#[derive(Debug)]
pub enum NvimEvent {
    Response { id: RequestId, error: Value, result: Value },
    Notification { method: String, params: Vec<Value> },
    Request { id: RequestId, method: String, params: Vec<Value> },
    ProtocolError(String),
}

pub struct NvimProcess {
    child: Child,
    input: BufWriter<ChildStdin>,
    events: Receiver<NvimEvent>,
    next_id: RequestId,
}

impl NvimProcess {
    pub fn spawn() -> Result<Self, NvimError> {
        let mut child = Command::new("nvim")
            .args(["--headless", "--embed", "--clean"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(NvimError::Spawn)?;
        let input = child.stdin.take().ok_or(NvimError::MissingStdin)?;
        let output = child.stdout.take().ok_or(NvimError::MissingStdout)?;
        let (sender, events) = mpsc::channel();
        thread::Builder::new()
            .name("nvim-rpc-reader".to_owned())
            .spawn(move || read_events(output, sender))
            .map_err(NvimError::Spawn)?;
        // Nvim emits its startup API response with id 1; application requests start at 2.
        Ok(Self { child, input: BufWriter::new(input), events, next_id: 2 })
    }

    pub fn request(&mut self, method: &str, params: Vec<Value>) -> Result<RequestId, NvimError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(Value::Array(vec![
            Value::from(0i64),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]))?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str, params: Vec<Value>) -> Result<(), NvimError> {
        self.write_message(Value::Array(vec![Value::from(2i64), Value::from(method), Value::Array(params)]))
    }

    pub fn respond(&mut self, id: RequestId, error: Value, result: Value) -> Result<(), NvimError> {
        self.write_message(Value::Array(vec![Value::from(1i64), Value::from(id), error, result]))
    }

    pub fn recv(&self) -> Result<NvimEvent, NvimError> {
        self.events.recv().map_err(|_| NvimError::ChannelClosed)
    }

    pub fn try_recv(&self) -> Result<Option<NvimEvent>, NvimError> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(NvimError::ChannelClosed),
        }
    }

    pub fn wait(&mut self) -> Result<std::process::ExitStatus, NvimError> {
        self.child.wait().map_err(NvimError::Wait)
    }

    fn write_message(&mut self, message: Value) -> Result<(), NvimError> {
        rmpv::encode::write_value(&mut self.input, &message).map_err(|error| NvimError::Encode(error.to_string()))?;
        self.input.flush().map_err(NvimError::Write)
    }
}

impl Drop for NvimProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_events(output: ChildStdout, sender: mpsc::Sender<NvimEvent>) {
    let mut input = BufReader::new(output);
    loop {
        match rmpv::decode::read_value(&mut input) {
            Ok(value) => match decode_event(value) {
                Ok(event) => {
                    if sender.send(event).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(NvimEvent::ProtocolError(error.to_string()));
                    return;
                }
            },
            Err(error) => {
                let _ = sender.send(NvimEvent::ProtocolError(error.to_string()));
                return;
            }
        }
    }
}

fn decode_event(value: Value) -> Result<NvimEvent, NvimError> {
    let parts = match value {
        Value::Array(parts) => parts,
        _ => return Err(NvimError::Decode("message is not an array".to_owned())),
    };
    let message_type = parts.first().and_then(Value::as_i64).ok_or_else(|| NvimError::Decode("message type is not an integer".to_owned()))?;
    match message_type {
        0 => {
            if parts.len() != 4 {
                return Err(NvimError::Decode("request has the wrong arity".to_owned()));
            }
            Ok(NvimEvent::Request {
                id: request_id(&parts[1])?,
                method: method_name(&parts[2])?,
                params: params(&parts[3])?,
            })
        }
        1 => {
            if parts.len() != 4 {
                return Err(NvimError::Decode("response has the wrong arity".to_owned()));
            }
            Ok(NvimEvent::Response { id: request_id(&parts[1])?, error: parts[2].clone(), result: parts[3].clone() })
        }
        2 => {
            if parts.len() != 3 {
                return Err(NvimError::Decode("notification has the wrong arity".to_owned()));
            }
            Ok(NvimEvent::Notification { method: method_name(&parts[1])?, params: params(&parts[2])? })
        }
        other => Err(NvimError::Decode(format!("unknown message type {other}"))),
    }
}

fn request_id(value: &Value) -> Result<RequestId, NvimError> {
    value.as_u64().or_else(|| value.as_i64().and_then(|id| u64::try_from(id).ok())).ok_or_else(|| NvimError::Decode("request id is not an unsigned integer".to_owned()))
}

fn method_name(value: &Value) -> Result<String, NvimError> {
    value.as_str().map(str::to_owned).ok_or_else(|| NvimError::Decode("method is not a string".to_owned()))
}

fn params(value: &Value) -> Result<Vec<Value>, NvimError> {
    match value {
        Value::Array(params) => Ok(params.clone()),
        _ => Err(NvimError::Decode("parameters are not an array".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_decode_each_msgpack_rpc_message_kind() {
        let request = decode_event(Value::Array(vec![Value::from(0i64), Value::from(7u64), Value::from("nvim_get_api_info"), Value::Array(vec![])])).unwrap();
        assert!(matches!(request, NvimEvent::Request { id: 7, method, params } if method == "nvim_get_api_info" && params.is_empty()));

        let response = decode_event(Value::Array(vec![Value::from(1i64), Value::from(7u64), Value::Nil, Value::Boolean(true)])).unwrap();
        assert!(matches!(response, NvimEvent::Response { id: 7, error: Value::Nil, result: Value::Boolean(true) }));

        let notification = decode_event(Value::Array(vec![Value::from(2i64), Value::from("redraw"), Value::Array(vec![])])).unwrap();
        assert!(matches!(notification, NvimEvent::Notification { method, params } if method == "redraw" && params.is_empty()));
    }
}

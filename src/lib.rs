use std::fmt;

use bytes::Bytes;
use futures_util::{
    sink::SinkExt,
    stream::{Stream, StreamExt},
};
use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::mpsc,
};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

// TODO: dox
// TODO: make this work for not oneshots
// TODO: maybe pass fd's over uds

#[derive(Debug)]
pub enum Error {
    Bind {
        path: &'static str,
        source: std::io::Error,
    },

    Connect {
        path: &'static str,
        source: std::io::Error,
    },

    Hup,

    IntermittentIo(std::io::Error),

    SendReply(std::io::Error),

    Cbor(serde_cbor::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Error::*;
        match self {
            Bind { source, path } => write!(f, "Can't bind to {}: {}", path, source),
            Connect { source, path } => write!(f, "Can't connect to {}: {}", path, source),
            Hup => f.write_str("Hung up before completing request"),
            IntermittentIo(e) => write!(f, "IO error while reading req/rep: {}", e),
            SendReply(e) => write!(f, "IO error while sending rep: {}", e),
            Cbor(e) => write!(f, "Io while serializing/deserializing from cbor: {}", e),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        use Error::*;
        match self {
            Bind { source, .. } => Some(source),
            Connect { source, .. } => Some(source),
            Hup => None,
            IntermittentIo(e) => Some(e),
            SendReply(e) => Some(e),
            Cbor(e) => Some(e),
        }
    }
}

pub trait Req: Serialize + DeserializeOwned + Send
where
    Self::Rep: Serialize + DeserializeOwned + Send,
{
    type Rep;
}

#[must_use = "Please actually reply to your requests"]
pub struct Request<R> {
    kind: R,
    framed: Framed<UnixStream, LengthDelimitedCodec>,
}

impl<R> Request<R>
where
    R: Req,
{
    pub async fn reply(mut self, reply: &R::Rep) -> Result<(), Error> {
        let ret = Bytes::from(serde_cbor::to_vec(reply).map_err(Error::Cbor)?);
        self.framed.send(ret).await.map_err(Error::SendReply)
    }

    pub fn kind(&self) -> &R {
        &self.kind
    }
}

async fn read_request<R>(conn: UnixStream) -> Result<Request<R>, Error>
where
    R: Req,
{
    let mut framed = Framed::new(conn, LengthDelimitedCodec::new());
    let buf = framed
        .next()
        .await
        .ok_or(Error::Hup)?
        .map_err(Error::IntermittentIo)?;

    let kind = serde_cbor::from_slice(&buf).map_err(Error::Cbor)?;

    Ok(Request { kind, framed })
}

pub fn listen<R>(
    path: &'static str,
    req_buffer: usize,
) -> Result<impl Stream<Item = Request<R>>, Error>
where
    R: Req + 'static,
{
    let mut listener = UnixListener::bind(path).map_err(|e| Error::Bind { path, source: e })?;
    let (mut tx, rx) = mpsc::channel(req_buffer);

    tokio::task::spawn(async move {
        let mut incoming = listener.incoming();
        while let Some(cxn) = incoming.next().await {
            if let Ok(cxn) = cxn {
                if let Ok(req) = read_request::<R>(cxn).await {
                    if let Err(_) = tx.send(req).await {
                        break;
                    }
                }
            }
        }
    });

    Ok(rx)
}

pub async fn send_request<R>(path: &'static str, req: R) -> Result<R::Rep, Error>
where
    R: Req,
{
    let stream = UnixStream::connect(path)
        .await
        .map_err(|e| Error::Connect { path, source: e })?;
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

    let buf = serde_cbor::to_vec(&req).map_err(Error::Cbor)?;
    framed
        .send(Bytes::from(buf))
        .await
        .map_err(Error::IntermittentIo)?;

    let buf = framed
        .next()
        .await
        .ok_or(Error::Hup)?
        .map_err(Error::IntermittentIo)?;

    serde_cbor::from_slice(&buf).map_err(Error::Cbor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_works() {
        use serde::Deserialize;

        #[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
        struct Hi;

        #[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
        struct Hello;

        impl Req for Hi {
            type Rep = Hello;
        }

        let path = "\0oneshot-test";

        let srv = listen::<Hi>(path, 1).unwrap();

        tokio::task::spawn(async move {
            futures_util::pin_mut!(srv);
            let req = srv.next().await.unwrap();
            req.reply(&Hello).await.unwrap();
        });

        let resp = send_request(path, Hi).await.unwrap();
        assert_eq!(resp, Hello);
    }
}

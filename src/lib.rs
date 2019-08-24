use bytes::Bytes;
use futures::prelude::*;
use serde::{de::DeserializeOwned, Serialize};
use snafu::{ResultExt, Snafu};
use tokio::{
    codec::{Framed, LengthDelimitedCodec},
    net::unix::{UnixListener, UnixStream},
    prelude::*,
};

// TODO: dox
// TODO: make this work for not oneshots
// TODO: maybe pass fd's over uds

#[derive(Snafu, Debug)]
pub enum Error {
    #[snafu(display("Can't bind to {}: {}", path, source))]
    Bind {
        path: &'static str,
        source: std::io::Error,
    },

    #[snafu(display("Can't connect to {}: {}", path, source))]
    Connect {
        path: &'static str,
        source: std::io::Error,
    },

    #[snafu(display("Connection hung up before finishing req-rep"))]
    Hup,

    #[snafu(display("Io error: {}", source))]
    IntermittentIo { source: std::io::Error },

    #[snafu(display("Reply"))]
    SendReply { source: std::io::Error },

    #[snafu(display("Serializing/Deserializing using bincode failed"))]
    Bincode { source: bincode::Error },
}

pub trait Req: Serialize + DeserializeOwned
where
    Self::Rep: Serialize + DeserializeOwned,
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
    pub async fn reply(mut self, reply: R::Rep) -> Result<(), Error> {
        let ret = Bytes::from(bincode::serialize(&reply).context(Bincode)?);
        self.framed.send(ret).await.context(SendReply)
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
        .context(IntermittentIo)?;

    let kind = bincode::deserialize(&buf).context(Bincode)?;

    Ok(Request { kind, framed })
}

pub fn listen<R>(path: &'static str) -> Result<impl Stream<Item = Request<R>>, Error>
where
    R: Req,
{
    let listener = UnixListener::bind(path).context(Bind { path })?;

    Ok(listener
        .incoming()
        .filter_map(|conn| async { conn.ok() })
        .map(read_request)
        .filter_map(|cmd| async { cmd.await.ok() }))
}

pub async fn send_request<R>(path: &'static str, req: R) -> Result<R::Rep, Error>
where
    R: Req,
{
    let stream = UnixStream::connect(path).await.context(Connect { path })?;
    let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

    let buf = bincode::serialize(&req).context(Bincode)?;
    framed
        .send(Bytes::from(buf))
        .await
        .context(IntermittentIo)?;

    let buf = framed
        .next()
        .await
        .ok_or(Error::Hup)?
        .context(IntermittentIo)?;

    bincode::deserialize(&buf).context(Bincode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        use serde::Deserialize;
        use tokio::runtime::current_thread;
        #[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
        struct Hi;

        #[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
        struct Hello;

        impl Req for Hi {
            type Rep = Hello;
        }

        let mut rt = current_thread::Runtime::new().unwrap();

        let path = "\0oneshot-test";
        let srv = listen::<Hi>(path).unwrap();

        rt.spawn(async move {
            futures::pin_mut!(srv);
            let req = srv.next().await.unwrap();
            req.reply(Hello).await.unwrap();
        });

        rt.spawn(async move {
            let resp = send_request(path, Hi).await.unwrap();
            assert_eq!(resp, Hello)
        });

        rt.run().unwrap();
    }
}

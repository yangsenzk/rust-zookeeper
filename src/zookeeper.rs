use std::convert::From;
use std::net::{SocketAddr, ToSocketAddrs};
use std::result;
use std::string::ToString;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use mio_extras::channel::Sender as MioSender;
use rand::{seq::SliceRandom, thread_rng};
use retry::{delay::Fixed, retry};

use crate::acl::*;
use crate::consts::*;
use crate::data::*;
use crate::io::ZkIo;
use crate::listeners::{ListenerSet, Subscription};
use crate::proto::*;
use crate::watch::{Watch, Watcher, WatchType, ZkWatch};

/// Value returned from potentially-error operations.
pub type ZkResult<T> = result::Result<T, ZkError>;

pub struct RawRequest {
    pub opcode: OpCode,
    pub data: ByteBuf,
    pub listener: Option<SyncSender<RawResponse>>,
    pub watch: Option<Watch>,
}

pub struct RawResponse {
    pub header: ReplyHeader,
    pub data: ByteBuf,
}

/// The client interface for interacting with a ZooKeeper cluster.
pub struct ZooKeeper {
    chroot: Option<String>,
    xid: AtomicIsize,
    io: Mutex<MioSender<RawRequest>>,
    listeners: ListenerSet<ZkState>,
}

impl ZooKeeper {
    fn zk_thread<F>(name: &str, task: F) -> ZkResult<thread::JoinHandle<()>>
        where
            F: FnOnce() + Send + 'static,
    {
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(task)
            .map_err(|_| ZkError::SystemError)
    }

    /// Connect to a ZooKeeper cluster.
    ///
    /// - `connect_string`: comma separated host:port pairs, each corresponding to a zk server,
    ///   e.g. `"127.0.0.1:3000,127.0.0.1:3001,127.0.0.1:3002"` If the optional chroot suffix is
    ///   used the example would look like: `"127.0.0.1:3000,127.0.0.1:3001,127.0.0.1:3002/app/a"`
    ///   where the client would be rooted at `"/app/a"` and all paths would be relative to this
    ///   root -- ie getting/setting/etc...`"/foo/bar"` would result in operations being run on
    ///   `"/app/a/foo/bar"` (from the server perspective).
    /// - `timeout`: session timeout -- how long should a client go without receiving communication
    ///   from a server before considering it connection loss?
    /// - `watcher`: a watcher object to be notified of connection state changes.
    pub fn connect<W>(connect_string: &str, timeout: Duration, watcher: W) -> ZkResult<ZooKeeper>
        where
            W: Watcher + 'static,
    {
        let (addrs, chroot) = Self::parse_connect_string(connect_string)?;

        debug!("Initiating connection to {}", connect_string);

        let (watch, watch_sender) = ZkWatch::new(watcher, chroot.clone());
        let listeners = ListenerSet::<ZkState>::new();
        let listeners1 = listeners.clone();
        let io = ZkIo::new(String::from(connect_string), addrs, timeout, watch_sender, listeners1);
        let sender = io.sender();

        Self::zk_thread("event", move || watch.run().unwrap())?;
        Self::zk_thread("io", move || io.run().unwrap())?;

        trace!("Returning a ZooKeeper");

        Ok(ZooKeeper {
            chroot: chroot,
            xid: AtomicIsize::new(1),
            io: Mutex::new(sender),
            listeners: listeners,
        })
    }

    fn parse_connect_string(connect_string: &str) -> ZkResult<(Vec<SocketAddr>, Option<String>)> {
        let (chroot, _end) = match connect_string.find('/') {
            Some(start) => match &connect_string[start..connect_string.len()] {
                "" | "/" => (None, start),
                chroot => (Some(Self::validate_path(chroot)?.to_owned()), start),
            },
            None => (None, connect_string.len()),
        };

        let mut parsed_addrs = Vec::new();
        for addr_str in connect_string.split(',') {
            let parsed_sock = retry(Fixed::from_millis(10).take(10), || {
                addr_str.trim().to_socket_addrs()
            });
            match parsed_sock {
                Ok(addrs) => {
                    for a in addrs.into_iter() {
                        parsed_addrs.push(a);
                    }
                }
                Err(e) => {
                    eprintln!("failed to refresh host of {}, error: {}", addr_str, e);
                    return Err(ZkError::BadArguments);
                }
            };
        }
        parsed_addrs.shuffle(&mut thread_rng());
        Ok((parsed_addrs, chroot))
    }

    fn xid(&self) -> i32 {
        self.xid.fetch_add(1, Ordering::Relaxed) as i32
    }

    fn request<Req: WriteTo, Resp: ReadFrom>(
        &self,
        opcode: OpCode,
        xid: i32,
        req: Req,
        watch: Option<Watch>,
    ) -> ZkResult<Resp> {
        trace!("request opcode={:?} xid={:?}", opcode, xid);
        let rh = RequestHeader {
            xid: xid,
            opcode: opcode,
        };
        let buf = to_len_prefixed_buf(rh, req).map_err(|_| ZkError::MarshallingError)?;

        let (resp_tx, resp_rx) = sync_channel(0);
        let request = RawRequest {
            opcode: opcode,
            data: buf,
            listener: Some(resp_tx),
            watch: watch,
        };

        self.io
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(request)
            .map_err(|err| {
                warn!("error sending request: {:?}", err);
                ZkError::ConnectionLoss
            })?;

        let mut response = resp_rx.recv().map_err(|err| {
            warn!("error receiving response: {:?}", err);
            ZkError::ConnectionLoss
        })?;

        match response.header.err {
            0 => Ok(ReadFrom::read_from(&mut response.data).map_err(|_| ZkError::MarshallingError)?),
            e => Err(ZkError::from(e)),
        }
    }

    fn validate_path(path: &str) -> ZkResult<&str> {
        match path {
            "" => Err(ZkError::BadArguments),
            path => {
                if path.len() > 1 && path.ends_with('/') {
                    Err(ZkError::BadArguments)
                } else {
                    Ok(path)
                }
            }
        }
    }

    fn path(&self, path: &str) -> ZkResult<String> {
        match self.chroot {
            Some(ref chroot) => match path {
                "/" => Ok(chroot.clone()),
                path => Ok(chroot.clone() + Self::validate_path(path)?),
            },
            None => Ok(Self::validate_path(path)?.to_owned()),
        }
    }

    fn cut_chroot(&self, path: String) -> String {
        if let Some(ref chroot) = self.chroot {
            path[chroot.len()..].to_owned()
        } else {
            path
        }
    }

    /// Add the specified `scheme`:`auth` information to this connection.
    ///
    /// See `Acl` for more information.
    pub fn add_auth<S: ToString>(&self, scheme: S, auth: Vec<u8>) -> ZkResult<()> {
        trace!("ZooKeeper::add_auth");
        let req = AuthRequest {
            typ: 0,
            scheme: scheme.to_string(),
            auth: auth,
        };

        let _: EmptyResponse = self.request(OpCode::Auth, -4, req, None)?;

        Ok(())
    }

    /// Create a node with the given `path`. The node data will be the given `data`, and node ACL
    /// will be the given `acl`. The `mode` argument specifies the behavior of the created node (see
    /// `CreateMode` for more information).
    ///
    /// This operation, if successful, will trigger all the watches left on the node of the given
    /// path by `exists` and `get_data` API calls, and the watches left on the parent node by
    /// `get_children` API calls.
    ///
    /// # Errors
    /// If a node with the same actual path already exists in the ZooKeeper, the result will have
    /// `Err(ZkError::NodeExists)`. Note that since a different actual path is used for each
    /// invocation of creating sequential node with the same path argument, the call should never
    /// error in this manner.
    ///
    /// If the parent node does not exist in the ZooKeeper, `Err(ZkError::NoNode)` will be returned.
    ///
    /// An ephemeral node cannot have children. If the parent node of the given path is ephemeral,
    /// `Err(ZkError::NoChildrenForEphemerals)` will be returned.
    ///
    /// If the `acl` is invalid or empty, `Err(ZkError::InvalidACL)` is returned.
    ///
    /// The maximum allowable size of the data array is 1 MiB (1,048,576 bytes). Arrays larger than
    /// this will return `Err(ZkError::BadArguments)`.
    pub fn create(
        &self,
        path: &str,
        data: Vec<u8>,
        acl: Vec<Acl>,
        mode: CreateMode,
    ) -> ZkResult<String> {
        trace!("ZooKeeper::create");
        let req = CreateRequest {
            path: self.path(path)?,
            data: data,
            acl: acl,
            flags: mode as i32,
        };

        let response: CreateResponse = self.request(OpCode::Create, self.xid(), req, None)?;

        Ok(self.cut_chroot(response.path))
    }

    pub fn create_with_ttl(
        &self,
        path: &str,
        data: Vec<u8>,
        acl: Vec<Acl>,
        mode: CreateMode,
        ttl: i64,
    ) -> ZkResult<String> {
        trace!("Zookeeper::create_with_ttl");
        let req = CreateRequestWithTTL {
            path: self.path(path)?,
            data,
            acl,
            flags: mode as i32,
            ttl,
        };
        let response: CreateResponse = self.request(OpCode::CreateTTL, self.xid(), req, None)?;
        Ok(self.cut_chroot(response.path))
    }
    /// Delete the node with the given `path`. The call will succeed if such a node exists, and the
    /// given `version` matches the node's version (if the given version is `None`, it matches any
    /// node's versions).
    ///
    /// This operation, if successful, will trigger all the watches on the node of the given path
    /// left by `exists` API calls, watches left by `get_data` API calls, and the watches on the
    /// parent node left by `get_children` API calls.
    ///
    /// # Errors
    /// If the nodes does not exist, `Err(ZkError::NoNode)` will be returned.
    ///
    /// If the given `version` does not match the node's version, `Err(ZkError::BadVersion)` will be
    /// returned.
    ///
    /// If the node has children, `Err(ZkError::NotEmpty)` will be returned.
    pub fn delete(&self, path: &str, version: Option<i32>) -> ZkResult<()> {
        trace!("ZooKeeper::delete");
        let req = DeleteRequest {
            path: self.path(path)?,
            version: version.unwrap_or(-1),
        };

        let _: EmptyResponse = self.request(OpCode::Delete, self.xid(), req, None)?;

        Ok(())
    }

    /// Return the `Stat` of the node of the given `path` or `None` if no such node exists.
    ///
    /// If the `watch` is `true` and the call is successful (no error is returned), a watch will be
    /// left on the node with the given path. The watch will be triggered by a successful operation
    /// that creates/delete the node or sets the data on the node.
    pub fn exists(&self, path: &str, watch: bool) -> ZkResult<Option<Stat>> {
        trace!("ZooKeeper::exists");
        let req = ExistsRequest {
            path: self.path(path)?,
            watch: watch,
        };

        match self.request::<ExistsRequest, ExistsResponse>(OpCode::Exists, self.xid(), req, None) {
            Ok(response) => Ok(Some(response.stat)),
            Err(ZkError::NoNode) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Return the `Stat` of the node of the given `path` or `None` if no such node exists.
    ///
    /// Similar to `exists`, but sets an explicit `Watcher` instead of relying on the client's base
    /// `Watcher`.
    pub fn exists_w<W: Watcher + 'static>(&self, path: &str, watcher: W) -> ZkResult<Option<Stat>> {
        trace!("ZooKeeper::exists_w");
        let req = ExistsRequest {
            path: self.path(path)?,
            watch: true,
        };

        let watch = Watch {
            path: path.to_owned(),
            watch_type: WatchType::Exist,
            watcher: Box::new(watcher),
        };

        match self.request::<ExistsRequest, ExistsResponse>(
            OpCode::Exists,
            self.xid(),
            req,
            Some(watch),
        ) {
            Ok(response) => Ok(Some(response.stat)),
            Err(ZkError::NoNode) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Return the ACL and `Stat` of the node of the given path.
    ///
    /// # Errors
    /// If no node with the given path exists, `Err(ZkError::NoNode)` will be returned.
    pub fn get_acl(&self, path: &str) -> ZkResult<(Vec<Acl>, Stat)> {
        trace!("ZooKeeper::get_acl");
        let req = GetAclRequest {
            path: self.path(path)?,
        };

        let response: GetAclResponse = self.request(OpCode::GetAcl, self.xid(), req, None)?;

        Ok(response.acl_stat)
    }

    /// Set the ACL for the node of the given path if such a node exists and the given version
    /// matches the version of the node. Return the `Stat` of the node.
    ///
    /// # Errors
    /// If no node with the given path exists, `Err(ZkError::NoNode)` will be returned.
    ///
    /// If the given version does not match the node's version, `Err(ZkError::BadVersion)` will be
    /// returned.
    pub fn set_acl(&self, path: &str, acl: Vec<Acl>, version: Option<i32>) -> ZkResult<Stat> {
        trace!("ZooKeeper::set_acl");
        let req = SetAclRequest {
            path: self.path(path)?,
            acl: acl,
            version: version.unwrap_or(-1),
        };

        let response: SetAclResponse = self.request(OpCode::SetAcl, self.xid(), req, None)?;

        Ok(response.stat)
    }

    /// Return the list of the children of the node of the given `path`. The returned values are not
    /// prefixed with the provided `path`; i.e. if the database contains `/path/a` and `/path/b`,
    /// the result of `get_children` for `"/path"` will be `["a", "b"]`.
    ///
    /// If the `watch` is `true` and the call is successful (no error is returned), a watch will be
    /// left on the node with the given path. The watch will be triggered by a successful operation
    /// that deletes the node of the given path or creates/delete a child under the node.
    ///
    /// The list of children returned is not sorted and no guarantee is provided as to its natural
    /// or lexical order.
    ///
    /// # Errors
    /// If no node with the given path exists, `Err(ZkError::NoNode)` will be returned.
    pub fn get_children(&self, path: &str, watch: bool) -> ZkResult<Vec<String>> {
        trace!("ZooKeeper::get_children");
        let req = GetChildrenRequest {
            path: self.path(path)?,
            watch: watch,
        };

        let response: GetChildrenResponse =
            self.request(OpCode::GetChildren, self.xid(), req, None)?;

        Ok(response.children)
    }

    /// Return the list of the children of the node of the given `path`.
    ///
    /// Similar to `get_children`, but sets an explicit `Watcher` instead of relying on the client's
    /// base `Watcher`.
    pub fn get_children_w<W: Watcher + 'static>(
        &self,
        path: &str,
        watcher: W,
    ) -> ZkResult<Vec<String>> {
        trace!("ZooKeeper::get_children_w");
        let req = GetChildrenRequest {
            path: self.path(path)?,
            watch: true,
        };

        let watch = Watch {
            path: path.to_owned(),
            watch_type: WatchType::Child,
            watcher: Box::new(watcher),
        };

        let response: GetChildrenResponse =
            self.request(OpCode::GetChildren, self.xid(), req, Some(watch))?;

        Ok(response.children)
    }

    /// Return the data and the `Stat` of the node of the given path.
    ///
    /// If `watch` is `true` and the call is successful (no error is returned), a watch will be left
    /// on the node with the given path. The watch will be triggered by a successful operation that
    /// sets data on the node, or deletes the node.
    ///
    /// # Errors
    /// If no node with the given path exists, `Err(ZkError::NoNode)` will be returned.
    pub fn get_data(&self, path: &str, watch: bool) -> ZkResult<(Vec<u8>, Stat)> {
        trace!("ZooKeeper::get_data");
        let req = GetDataRequest {
            path: self.path(path)?,
            watch: watch,
        };

        let response: GetDataResponse = self.request(OpCode::GetData, self.xid(), req, None)?;

        Ok(response.data_stat)
    }

    /// Return the data and the `Stat` of the node of the given path.
    ///
    /// Similar to `get_data`, but sets an explicit `Watcher` instead of relying on the client's
    /// base `Watcher`.
    pub fn get_data_w<W: Watcher + 'static>(
        &self,
        path: &str,
        watcher: W,
    ) -> ZkResult<(Vec<u8>, Stat)> {
        trace!("ZooKeeper::get_data_w");
        let req = GetDataRequest {
            path: self.path(path)?,
            watch: true,
        };

        let watch = Watch {
            path: path.to_owned(),
            watch_type: WatchType::Data,
            watcher: Box::new(watcher),
        };

        let response: GetDataResponse =
            self.request(OpCode::GetData, self.xid(), req, Some(watch))?;

        Ok(response.data_stat)
    }

    /// Set the data for the node of the given `path` if such a node exists and the given version
    /// matches the version of the node (if the given version is `None`, it matches any node's
    /// versions). Return the `Stat` of the node.
    ///
    /// This operation, if successful, will trigger all the watches on the node of the given `path`
    /// left by `get_data` calls.
    ///
    /// # Errors
    /// If no node with the given `path` exists, `Err(ZkError::NoNode)` will be returned.
    ///
    /// If the given version does not match the node's version, `Err(ZkError::BadVersion)` will be
    /// returned.
    ///
    /// The maximum allowable size of the `data` array is 1 MiB (1,048,576 bytes). Arrays larger
    /// than this will return `Err(ZkError::BadArguments)`.
    pub fn set_data(&self, path: &str, data: Vec<u8>, version: Option<i32>) -> ZkResult<Stat> {
        trace!("ZooKeeper::set_data");
        let req = SetDataRequest {
            path: self.path(path)?,
            data: data,
            version: version.unwrap_or(-1),
        };

        let response: SetDataResponse = self.request(OpCode::SetData, self.xid(), req, None)?;

        Ok(response.stat)
    }

    /// Adds a state change `Listener`, which will be notified of changes to the client's `ZkState`.
    /// A unique identifier is returned, which is used in `remove_listener` to un-subscribe.
    pub fn add_listener<Listener: Fn(ZkState) + Send + 'static>(
        &self,
        listener: Listener,
    ) -> Subscription {
        trace!("ZooKeeper::add_listener");
        self.listeners.subscribe(listener)
    }

    /// Removes a state change `Listener` and closes the channel.
    pub fn remove_listener(&self, sub: Subscription) {
        trace!("ZooKeeper::remove_listener");
        self.listeners.unsubscribe(sub);
    }

    /// Close this client object. Once the client is closed, its session becomes invalid. All the
    /// ephemeral nodes in the ZooKeeper server associated with the session will be removed. The
    /// watches left on those nodes (and on their parents) will be triggered.
    pub fn close(&self) -> ZkResult<()> {
        trace!("ZooKeeper::close");
        let _: EmptyResponse = self.request(OpCode::CloseSession, 0, EmptyRequest, None)?;

        Ok(())
    }
}

impl Drop for ZooKeeper {
    fn drop(&mut self) {
        if let Err(err) = self.close() {
            error!("error closing zookeeper connection in drop: {:?}", err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ZooKeeper;

    // TODO This is flaky on Travis, it works on my Linux box though.
    #[test]
    #[cfg(target_os = "macos")]
    fn parse_connect_string() {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

        let (addrs, chroot) = ZooKeeper::parse_connect_string("127.0.0.1:2181,::1:2181/mesos")
            .ok()
            .expect("Parse 1");
        assert_eq!(
            addrs,
            vec![
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 2181)),
                SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1),
                    2181,
                    0,
                    0,
                )),
            ]
        );
        assert_eq!(chroot, Some("/mesos".to_owned()));

        let (addrs, chroot) = ZooKeeper::parse_connect_string("::1:2181")
            .ok()
            .expect("Parse 2");
        assert_eq!(
            addrs,
            vec![SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1),
                2181,
                0,
                0,
            ))]
        );
        assert_eq!(chroot, None);

        let (addrs, chroot) = ZooKeeper::parse_connect_string("::1:2181/")
            .ok()
            .expect("Parse 3");
        assert_eq!(
            addrs,
            vec![SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1),
                2181,
                0,
                0,
            ))]
        );
        assert_eq!(chroot, None);
    }

    #[test]
    #[should_panic(expected = "BadArguments")]
    fn parse_connect_string_fails() {
        // This fails with ZooKeeper.java: Path must not end with / character
        ZooKeeper::parse_connect_string("127.0.0.1:2181/mesos/").unwrap();
    }
}

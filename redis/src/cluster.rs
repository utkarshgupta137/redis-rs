//! Redis cluster support.
//!
//! This module extends the library to be able to use cluster.
//! ClusterClient implements traits of ConnectionLike and Commands.
//!
//! Note that the cluster support currently does not provide pubsub
//! functionality.
//!
//! # Example
//! ```rust,no_run
//! use redis::Commands;
//! use redis::cluster::ClusterClient;
//!
//! let nodes = vec!["redis://127.0.0.1:6379/", "redis://127.0.0.1:6378/", "redis://127.0.0.1:6377/"];
//! let client = ClusterClient::new(nodes).unwrap();
//! let mut connection = client.get_connection().unwrap();
//!
//! let _: () = connection.set("test", "test_data").unwrap();
//! let rv: String = connection.get("test").unwrap();
//!
//! assert_eq!(rv, "test_data");
//! ```
//!
//! # Pipelining
//! ```rust,no_run
//! use redis::Commands;
//! use redis::cluster::{cluster_pipe, ClusterClient};
//!
//! let nodes = vec!["redis://127.0.0.1:6379/", "redis://127.0.0.1:6378/", "redis://127.0.0.1:6377/"];
//! let client = ClusterClient::new(nodes).unwrap();
//! let mut connection = client.get_connection().unwrap();
//!
//! let key = "test";
//!
//! let _: () = cluster_pipe()
//!     .rpush(key, "123").ignore()
//!     .ltrim(key, -10, -1).ignore()
//!     .expire(key, 60).ignore()
//!     .query(&mut connection).unwrap();
//! ```
use std::collections::BTreeMap;
use std::iter::Iterator;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

use rand::{
    seq::{IteratorRandom, SliceRandom},
    thread_rng,
};

use crate::cluster_client::ClusterParams;
use crate::cluster_routing::{Routable, Slot, SLOT_SIZE};
use crate::cmd::{cmd, Cmd};
use crate::connection::{
    connect, Connection, ConnectionAddr, ConnectionInfo, ConnectionLike, RedisConnectionInfo,
};
use crate::parser::parse_redis_value;
use crate::types::{ErrorKind, HashMap, RedisError, RedisResult, Value};

pub use crate::cluster_client::{ClusterClient, ClusterClientBuilder};
pub use crate::cluster_pipeline::{cluster_pipe, ClusterPipeline};

type SlotMap = BTreeMap<u16, [String; 2]>;

/// This is a connection of Redis cluster.
#[derive(Default)]
pub struct ClusterConnection {
    cluster_params: ClusterParams,
    initial_nodes: Vec<ConnectionInfo>,

    connections: HashMap<String, Connection>,
    slots: SlotMap,
}

impl ClusterConnection {
    /// Set the timeout for connecting to new nodes.
    pub fn set_connect_timeout(&mut self, dur: Option<Duration>) -> RedisResult<()> {
        // Check if duration is valid before updating local value.
        if dur.is_some() && dur.unwrap().is_zero() {
            return Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "Duration should be None or non-zero.",
            )));
        }

        self.cluster_params.connect_timeout = dur;
        Ok(())
    }

    /// Set the write timeout for this connection.
    pub fn set_write_timeout(&mut self, dur: Option<Duration>) -> RedisResult<()> {
        // Check if duration is valid before updating local value.
        if dur.is_some() && dur.unwrap().is_zero() {
            return Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "Duration should be None or non-zero.",
            )));
        }

        self.cluster_params.write_timeout = dur;
        for conn in self.connections.values() {
            conn.set_write_timeout(dur)?;
        }
        Ok(())
    }

    /// Set the read timeout for this connection.
    pub fn set_read_timeout(&mut self, dur: Option<Duration>) -> RedisResult<()> {
        // Check if duration is valid before updating local value.
        if dur.is_some() && dur.unwrap().is_zero() {
            return Err(RedisError::from((
                ErrorKind::InvalidClientConfig,
                "Duration should be None or non-zero.",
            )));
        }

        self.cluster_params.read_timeout = dur;
        for conn in self.connections.values() {
            conn.set_read_timeout(dur)?;
        }
        Ok(())
    }

    /// Set the retries parameter for this connection.
    pub fn set_retries(&mut self, value: u8) {
        self.cluster_params.retries = value;
    }

    /// Set the auto reconnect parameter for this connection.
    pub fn set_auto_reconnect(&mut self, value: bool) {
        self.cluster_params.auto_reconnect = value;
    }

    pub(crate) fn new(
        cluster_params: &ClusterParams,
        initial_nodes: &[ConnectionInfo],
    ) -> RedisResult<ClusterConnection> {
        let mut connection = ClusterConnection {
            cluster_params: cluster_params.clone(),
            initial_nodes: initial_nodes.to_vec(),
            ..Default::default()
        };
        connection.create_initial_connections()?;

        Ok(connection)
    }

    pub(crate) fn execute_pipeline(&mut self, pipe: &ClusterPipeline) -> RedisResult<Vec<Value>> {
        self.send_recv_and_retry_cmds(pipe.commands())
    }

    /// Returns the connection status.
    ///
    /// The connection is open until any `read_response` call recieved an
    /// invalid response from the server (most likely a closed or dropped
    /// connection, otherwise a Redis protocol error). When using unix
    /// sockets the connection is open until writing a command failed with a
    /// `BrokenPipe` error.
    fn create_initial_connections(&mut self) -> RedisResult<()> {
        let mut connections = HashMap::with_capacity(self.initial_nodes.len());

        for info in self.initial_nodes.iter() {
            let addr = info.addr.to_string();

            if let Ok(mut conn) = self.connect(&addr) {
                if conn.check_connection() {
                    connections.insert(addr, conn);
                    break;
                }
            }
        }

        if connections.is_empty() {
            return Err(RedisError::from((
                ErrorKind::IoError,
                "It failed to check startup nodes.",
            )));
        }

        self.connections = connections;
        self.refresh_slots()?;
        Ok(())
    }

    // Query a node to discover slot-> master mappings.
    fn refresh_slots(&mut self) -> RedisResult<()> {
        let slots = self.create_new_slots()?;
        self.slots = slots;

        let mut nodes = self.slots.values().flatten().cloned().collect::<Vec<_>>();
        nodes.sort_unstable();
        nodes.dedup();

        let connections = nodes
            .into_iter()
            .filter_map(|addr| {
                if self.connections.contains_key(&addr) {
                    let mut conn = self.connections.remove(&addr).unwrap();
                    if conn.check_connection() {
                        return Some((addr.to_string(), conn));
                    }
                }

                if let Ok(mut conn) = self.connect(&addr) {
                    if conn.check_connection() {
                        conn.set_read_timeout(self.cluster_params.read_timeout)
                            .unwrap();
                        conn.set_write_timeout(self.cluster_params.write_timeout)
                            .unwrap();
                        return Some((addr.to_string(), conn));
                    }
                }

                None
            })
            .collect();
        self.connections = connections;

        Ok(())
    }

    fn create_new_slots(&mut self) -> RedisResult<SlotMap> {
        let mut cmd = Cmd::new();
        cmd.arg("CLUSTER").arg("SLOTS");

        let mut new_slots = None;
        let mut rng = thread_rng();
        let len = self.connections.len();
        let mut samples = self.connections.values_mut().choose_multiple(&mut rng, len);

        for conn in samples.iter_mut() {
            if let Ok(Value::Bulk(response)) = cmd.query(conn) {
                new_slots = Some(parse_slots_response(response, &self.cluster_params)?);
                break;
            }
        }

        match new_slots {
            Some(new_slots) => Ok(new_slots),
            None => Err(RedisError::from((
                ErrorKind::ResponseError,
                "Slot refresh error.",
                "didn't get any slots from server".to_string(),
            ))),
        }
    }

    fn connect(&self, node: &str) -> RedisResult<Connection> {
        let info = get_connection_info(node, &self.cluster_params);

        let mut conn = connect(&info, self.cluster_params.connect_timeout)?;
        if self.cluster_params.read_from_replicas {
            // If READONLY is sent to primary nodes, it will have no effect
            cmd("READONLY").query(&mut conn)?;
        }
        Ok(conn)
    }

    fn get_connection(&mut self, route: (u16, usize)) -> RedisResult<&mut Connection> {
        let node = self.get_node_for_route(route);
        self.get_connection_by_addr(&node)
    }

    fn get_connection_by_addr(&mut self, addr: &str) -> RedisResult<&mut Connection> {
        if self.connections.contains_key(addr) {
            Ok(self.connections.get_mut(addr).unwrap())
        } else {
            // Create new connection.
            // TODO: error handling
            let conn = self.connect(addr)?;
            Ok(self.connections.entry(addr.to_string()).or_insert(conn))
        }
    }

    fn get_node_for_route(&self, route: (u16, usize)) -> String {
        let (slot, idx) = route;
        self.slots.range(&slot..).next().unwrap().1[idx].clone()
    }

    fn map_cmds_to_nodes(&self, cmds: &[Cmd]) -> RedisResult<Vec<NodeCmd>> {
        let mut cmd_map: HashMap<String, NodeCmd> = HashMap::with_capacity(self.slots.len());

        for (idx, cmd) in cmds.iter().enumerate() {
            let node = match cmd.route()? {
                (Some(slot), idx) => self.get_node_for_route((slot, idx)),
                (None, _idx) => unreachable!(),
            };

            let nc = if let Some(nc) = cmd_map.get_mut(&node) {
                nc
            } else {
                cmd_map
                    .entry(node.to_string())
                    .or_insert_with(|| NodeCmd::new(&node))
            };

            nc.indexes.push(idx);
            cmd.write_packed_command(&mut nc.pipe);
        }

        // Workaround for https://github.com/tkaitchuck/aHash/issues/118
        Ok(cmd_map.into_iter().map(|(_k, v)| v).collect())
    }

    fn execute_on_all_nodes<T, F>(&mut self, mut func: F) -> RedisResult<T>
    where
        T: MergeResults,
        F: FnMut(&mut Connection) -> RedisResult<T>,
    {
        let mut results = HashMap::new();

        // TODO: reconnect and shit
        for (addr, connection) in self.connections.iter_mut() {
            results.insert(addr.as_str(), func(connection)?);
        }

        Ok(T::merge_results(results))
    }

    fn request<R, T, F>(&mut self, cmd: &R, mut func: F) -> RedisResult<T>
    where
        R: ?Sized + Routable,
        T: MergeResults + std::fmt::Debug,
        F: FnMut(&mut Connection) -> RedisResult<T>,
    {
        let route = match cmd.route()? {
            (Some(slot), idx) => (slot, idx),
            (None, _idx) => return self.execute_on_all_nodes(func),
        };

        let mut is_asking = false;
        let mut retries = self.cluster_params.retries;
        let mut redirected = None::<String>;
        loop {
            // Get target address and response.
            let rv = {
                let conn = if let Some(addr) = redirected.take() {
                    let conn = self.get_connection_by_addr(&addr)?;
                    if is_asking {
                        // if we are in asking mode we want to feed a single
                        // ASKING command into the connection before what we
                        // actually want to execute.
                        conn.req_packed_command(&b"*1\r\n$6\r\nASKING\r\n"[..])?;
                        is_asking = false;
                    }
                    conn
                } else {
                    self.get_connection(route)?
                };
                func(conn)
            };

            match rv {
                Ok(rv) => return Ok(rv),
                Err(err) => {
                    retries -= 1;
                    if retries == 0 {
                        return Err(err);
                    }

                    if err.is_cluster_error() {
                        let kind = err.kind();

                        if kind == ErrorKind::Ask {
                            redirected = err.redirect_node().map(|(node, _slot)| node.to_string());
                            is_asking = true;
                        } else if kind == ErrorKind::Moved {
                            // Refresh slots.
                            self.refresh_slots()?;

                            // Request again.
                            redirected = err.redirect_node().map(|(node, _slot)| node.to_string());
                            is_asking = false;
                        } else if kind == ErrorKind::TryAgain || kind == ErrorKind::ClusterDown {
                            // Sleep and retry.
                            let sleep_time = 2u64.pow(16 - u32::from(retries.max(9))) * 10;
                            thread::sleep(Duration::from_millis(sleep_time));
                        }
                    } else if self.cluster_params.auto_reconnect && err.is_io_error() {
                        self.create_initial_connections()?;
                    } else {
                        return Err(err);
                    }
                }
            }
        }
    }

    fn send_recv_and_retry_cmds(&mut self, cmds: &[Cmd]) -> RedisResult<Vec<Value>> {
        // Vector to hold the results, pre-populated with `Nil` values. This allows the original
        // cmd ordering to be re-established by inserting the response directly into the result
        // vector (e.g., results[10] = response).
        let mut results = vec![Value::Nil; cmds.len()];

        let to_retry = self
            .send_all_commands(cmds)
            .and_then(|node_cmds| self.recv_all_commands(&mut results, &node_cmds))?;

        if to_retry.is_empty() {
            return Ok(results);
        }

        // Refresh the slots to ensure that we have a clean slate for the retry attempts.
        self.refresh_slots()?;

        // Given that there are commands that need to be retried, it means something in the cluster
        // topology changed. Execute each command seperately to take advantage of the existing
        // retry logic that handles these cases.
        for retry_idx in to_retry {
            let cmd = &cmds[retry_idx];
            results[retry_idx] = self.request(cmd, move |conn| conn.req_command(cmd))?;
        }
        Ok(results)
    }

    // Build up a pipeline per node, then send it
    fn send_all_commands(&mut self, cmds: &[Cmd]) -> RedisResult<Vec<NodeCmd>> {
        let node_cmds = self.map_cmds_to_nodes(cmds)?;
        for nc in &node_cmds {
            self.get_connection_by_addr(&nc.node)?
                .send_packed_command(&nc.pipe)?;
        }
        Ok(node_cmds)
    }

    // Receive from each node, keeping track of which commands need to be retried.
    fn recv_all_commands(
        &mut self,
        results: &mut [Value],
        node_cmds: &[NodeCmd],
    ) -> RedisResult<Vec<usize>> {
        let mut to_retry = Vec::new();
        let mut first_err = None;

        for nc in node_cmds {
            for cmd_idx in &nc.indexes {
                match self.get_connection_by_addr(&nc.node)?.recv_response() {
                    Ok(item) => results[*cmd_idx] = item,
                    Err(err) if err.is_cluster_error() => to_retry.push(*cmd_idx),
                    Err(err) => first_err = first_err.or(Some(err)),
                }
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(to_retry),
        }
    }
}

trait MergeResults {
    fn merge_results(_values: HashMap<&str, Self>) -> Self
    where
        Self: Sized;
}

impl MergeResults for Value {
    fn merge_results(values: HashMap<&str, Value>) -> Value {
        let mut items = vec![];
        for (addr, value) in values.into_iter() {
            items.push(Value::Bulk(vec![
                Value::Data(addr.as_bytes().to_vec()),
                value,
            ]));
        }
        Value::Bulk(items)
    }
}

impl MergeResults for Vec<Value> {
    fn merge_results(_values: HashMap<&str, Vec<Value>>) -> Vec<Value> {
        unreachable!("attempted to merge a pipeline. This should not happen");
    }
}

// NodeCmd struct.
struct NodeCmd {
    node: String,
    // The original command indexes
    indexes: Vec<usize>,
    pipe: Vec<u8>,
}

impl NodeCmd {
    fn new(node: &str) -> NodeCmd {
        NodeCmd {
            node: node.to_string(),
            indexes: vec![],
            pipe: vec![],
        }
    }
}

impl ConnectionLike for ClusterConnection {
    fn supports_pipelining(&self) -> bool {
        false
    }

    fn req_command(&mut self, cmd: &Cmd) -> RedisResult<Value> {
        self.request(cmd, move |conn| conn.req_command(cmd))
    }

    fn req_packed_command(&mut self, cmd: &[u8]) -> RedisResult<Value> {
        let value = parse_redis_value(cmd)?;
        self.request(&value, move |conn| conn.req_packed_command(cmd))
    }

    fn req_packed_commands(
        &mut self,
        cmd: &[u8],
        offset: usize,
        count: usize,
    ) -> RedisResult<Vec<Value>> {
        let value = parse_redis_value(cmd)?;
        self.request(&value, move |conn| {
            conn.req_packed_commands(cmd, offset, count)
        })
    }

    fn get_db(&self) -> i64 {
        0
    }

    fn is_open(&self) -> bool {
        for conn in self.connections.values() {
            if !conn.is_open() {
                return false;
            }
        }
        true
    }

    fn check_connection(&mut self) -> bool {
        for conn in self.connections.values_mut() {
            if !conn.check_connection() {
                return false;
            }
        }
        true
    }
}

// Parse `CLUSTER SLOTS` response into SlotMap.
fn parse_slots_response(
    response: Vec<Value>,
    cluster_params: &ClusterParams,
) -> RedisResult<SlotMap> {
    let mut slots = Vec::with_capacity(2);
    let mut items = response.into_iter();
    while let Some(Value::Bulk(item)) = items.next() {
        if item.len() < 3 {
            continue;
        }

        let start = if let Value::Int(start) = item[0] {
            start as u16
        } else {
            continue;
        };
        let end = if let Value::Int(end) = item[1] {
            end as u16
        } else {
            continue;
        };

        let mut nodes: Vec<String> = item
            .into_iter()
            .skip(2)
            .filter_map(|node| {
                if let Value::Bulk(node) = node {
                    if node.len() < 2 {
                        return None;
                    }

                    let host = if let Value::Data(ref host) = node[0] {
                        String::from_utf8_lossy(host)
                    } else {
                        return None;
                    };
                    if host.is_empty() {
                        return None;
                    }

                    let port = if let Value::Int(port) = node[1] {
                        port as u16
                    } else {
                        return None;
                    };
                    Some(
                        get_connection_addr((host.into_owned(), port), cluster_params.tls_insecure)
                            .to_string(),
                    )
                } else {
                    None
                }
            })
            .collect();

        if nodes.is_empty() {
            continue;
        }

        let replicas = nodes.split_off(1);
        slots.push(Slot::new(start, end, nodes.pop().unwrap(), replicas));
    }

    slots.sort_unstable_by_key(|s| s.start());

    let last_slot = slots.iter().try_fold(0, |prev_end, slot_data| {
        if prev_end != slot_data.start() {
            return Err(RedisError::from((
                ErrorKind::ResponseError,
                "Slot refresh error.",
                format!(
                    "Received overlapping slots {} and {}..{}",
                    prev_end,
                    slot_data.start(),
                    slot_data.end()
                ),
            )));
        }
        Ok(slot_data.end() + 1)
    })?;

    if last_slot != SLOT_SIZE {
        return Err(RedisError::from((
            ErrorKind::ResponseError,
            "Slot refresh error.",
            format!("Lacks the slots >= {}", last_slot),
        )));
    }

    Ok(slots
        .iter()
        .map(|slot_data| {
            let nodes = {
                let replica =
                    if !cluster_params.read_from_replicas || slot_data.replicas().is_empty() {
                        slot_data.master().to_string()
                    } else {
                        slot_data
                            .replicas()
                            .choose(&mut thread_rng())
                            .unwrap()
                            .to_string()
                    };

                [slot_data.master().to_string(), replica]
            };

            (slot_data.end(), nodes)
        })
        .collect())
}

fn get_connection_info(node: &str, cluster_params: &ClusterParams) -> ConnectionInfo {
    ConnectionInfo {
        addr: get_connection_addr(parse_node_str(node), cluster_params.tls_insecure),
        redis: RedisConnectionInfo {
            password: cluster_params.password.clone(),
            username: cluster_params.username.clone(),
            ..Default::default()
        },
    }
}

fn get_connection_addr(node: (String, u16), tls_insecure: Option<bool>) -> ConnectionAddr {
    let (host, port) = node;
    match tls_insecure {
        Some(insecure) => ConnectionAddr::TcpTls {
            host,
            port,
            insecure,
        },
        _ => ConnectionAddr::Tcp(host, port),
    }
}

fn parse_node_str(node: &str) -> (String, u16) {
    let mut split = node.split(':');
    let host = split.next().unwrap().to_string();
    let port = u16::from_str(split.next().unwrap()).unwrap();
    (host, port)
}

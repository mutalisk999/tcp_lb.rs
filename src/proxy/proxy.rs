use tokio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::error::Error;

use std::collections::HashMap;
use std::sync::Arc;

use crate::proxy::config::Config;
use crate::proxy::connection::{NodeConnection, TargetConnection, new_connection_id};
use crate::proxy::target::{Target, TargetDumpOrder, calc_target_id_by_endpoint, dump_targets};
use crate::proxy::config::{read_config};


#[derive(Debug)]
pub struct ProxyServer {
    pub server_config: Config,
    pub targets_info: Arc<tokio::sync::Mutex<HashMap<String, Target>>>,
    pub connections_info: Arc<tokio::sync::Mutex<HashMap<String, (NodeConnection, TargetConnection)>>>,
}

impl ProxyServer {
    pub fn new() -> ProxyServer{
        ProxyServer {
            server_config: read_config(),
            targets_info: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            connections_info: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }
}

pub async fn start_tcp_proxy(proxy_server: &mut ProxyServer
    ) -> Result<(), Box<dyn Error>>{

    let node_listener = tokio::net::TcpListener::bind(proxy_server.server_config.lb_node.listen.as_str())
        .await.expect(format!("Failure binding node listen endpoint [{}]", proxy_server.server_config.lb_node.listen).as_str());

    loop {
        let (mut tcp_stream_node, node_remote_addr) = node_listener.accept().await?;
        println!("remote connection from {}", node_remote_addr);

        let targets_dump = dump_targets(proxy_server, TargetDumpOrder::AscOrder).await;

        let mut tcp_stream_target: Option<tokio::net::TcpStream> = None;
        let mut conn_target_info: Option<Target> = None;

        // try to connect from the least connection of the target
        for t in targets_dump.iter() {
            if !t.target.target_active {
                continue;
            }
            if t.target_conn_count > t.target.target_max_conn {
                continue;
            }

            let r = tokio::net::TcpSocket::new_v4();
            let socket_conn = match r {
                Ok(s) => Some(s),
                Err(_) => continue
            };
            let connect_timeout = tokio::time::Duration::from_secs(5);
            if let Ok(r) = tokio::time::timeout(connect_timeout,
                                         socket_conn.unwrap().connect(t.target.target_endpoint.parse().unwrap())).await {
                tcp_stream_target = match r {
                    Ok(c) => Some(c),
                    Err(_) => continue
                };

                conn_target_info = Some(t.target.clone());
                break;
            }
        }

        match tcp_stream_target {
            Some(_) => (),
            None => {
                let _ = tcp_stream_node.shutdown().await;
                continue;
            }
        }

        let node_connection_timeout = proxy_server.server_config.lb_node.timeout;
        let target_connection_timeout = conn_target_info.clone().unwrap().target_timeout;

        let conn_target_id = calc_target_id_by_endpoint(conn_target_info.clone().unwrap().target_endpoint);
        let tcp_stream_target = tcp_stream_target.unwrap();
        let target_local_addr = tcp_stream_target.local_addr().unwrap().to_string();

        let (mut tcp_stream_node_read, mut tcp_stream_node_write) = tcp_stream_node.into_split();
        let (mut tcp_stream_target_read, mut tcp_stream_target_write) = tcp_stream_target.into_split();

        let node_connection_info = NodeConnection::new(
            proxy_server.server_config.lb_node.listen.clone(),
            node_remote_addr.to_string());

        let target_connection_info = TargetConnection::new(
            target_local_addr.clone(),
            conn_target_info.clone().unwrap().target_endpoint,
            conn_target_id
        );

        let proxy_connection_id = new_connection_id();
        let connection_info_arc = proxy_server.connections_info.clone();
        let proxy_connection_id_dump = proxy_connection_id.clone();
        let connection_info_arc_dump = connection_info_arc.clone();

        let mut connection_info = proxy_server.connections_info.lock().await.clone();
        connection_info.insert(proxy_connection_id.clone(), (node_connection_info, target_connection_info));
        println!("success build connection pair |{}|, node: {}->{}, target: {}->{}",
                 proxy_connection_id,
                 node_remote_addr.to_string(),
                 proxy_server.server_config.lb_node.listen.clone(),
                 target_local_addr.clone(),
                 conn_target_info.clone().unwrap().target_endpoint);

        // task of bytes reading from node connection or writing to target connection
        tokio::spawn(async move {
            let mut buf = [0; 1024];
            let mut count;
            loop {
                let read_timeout = tokio::time::Duration::from_secs(node_connection_timeout as u64);
                if let Ok(r) = tokio::time::timeout(read_timeout, tcp_stream_node_read.read(&mut buf)).await {
                    match r {
                        Ok(n) if n == 0 => {
                            connection_info_arc.lock().await.remove(&proxy_connection_id);
                            eprintln!("|{}| tcp_stream_node_read: closed by remote", proxy_connection_id);
                            return;
                        },
                        Ok(n) => {
                            {
                                count = n;
                                let node_info = connection_info_arc.lock().await;
                                let v = node_info.get(&proxy_connection_id);
                                match v {
                                    Some((node_info, target_info)) => {
                                        let mut node_info_dump = node_info.clone();
                                        let target_info_dump = target_info.clone();
                                        node_info_dump.add_read_n(n as u64);
                                        connection_info_arc.lock().await.insert(proxy_connection_id.clone(), (node_info_dump, target_info_dump));
                                    },
                                    None => {
                                        connection_info_arc.lock().await.remove(&proxy_connection_id);
                                        eprintln!("|{}| tcp_stream_node_read: not find, disconnect", proxy_connection_id);
                                        return;
                                    },
                                }
                            }
                        },
                        Err(e) => {
                            connection_info_arc.lock().await.remove(&proxy_connection_id.clone());
                            eprintln!("|{}| tcp_stream_node_read: failed to read from socket; err = {:?}", proxy_connection_id, e);
                            return;
                        }
                    };
                } else {
                    // read from node timeout
                    connection_info_arc.lock().await.remove(&proxy_connection_id);
                    eprintln!("|{}| tcp_stream_node_read: timeout", proxy_connection_id);
                    return;
                }

                let write_timeout = tokio::time::Duration::from_secs(target_connection_timeout as u64);
                if let Ok(r) = tokio::time::timeout(write_timeout, tcp_stream_target_write.write_all(&buf[0..count])).await {
                    match r {
                        Ok(_) => {
                            let node_info = connection_info_arc.lock().await;
                            let v = node_info.get(&proxy_connection_id);
                            match v {
                                Some((node_info, target_info)) => {
                                    let node_info_dump = node_info.clone();
                                    let mut target_info_dump = target_info.clone();
                                    target_info_dump.add_write_n(count as u64);
                                    connection_info_arc.lock().await.insert(proxy_connection_id.clone(), (node_info_dump, target_info_dump));
                                },
                                None => {
                                    connection_info_arc.lock().await.remove(&proxy_connection_id);
                                    eprintln!("|{}| tcp_stream_target_write: not find, disconnect", proxy_connection_id);
                                    return;
                                },
                            }
                        }
                        Err(e) => {
                            connection_info_arc.lock().await.remove(&proxy_connection_id);
                            eprintln!("|{}| tcp_stream_target_write: failed to write to socket; err = {:?}", proxy_connection_id, e);
                            return;
                        }
                    }
                } else {
                    // write to target timeout
                    connection_info_arc.lock().await.remove(&proxy_connection_id);
                    eprintln!("|{}| tcp_stream_target_write: timeout", proxy_connection_id);
                    return;
                }
            }
        });

        // task of bytes reading from target connection or writing to node connection
        tokio::spawn(async move {
            let mut buf = [0; 1024];
            let mut count;
            loop {
                let read_timeout = tokio::time::Duration::from_secs(target_connection_timeout as u64);
                if let Ok(r) = tokio::time::timeout(read_timeout, tcp_stream_target_read.read(&mut buf)).await {
                    match r {
                        Ok(n) if n == 0 => {
                            connection_info_arc_dump.lock().await.remove(&proxy_connection_id_dump);
                            eprintln!("|{}| tcp_stream_target_read: closed by remote", proxy_connection_id_dump);
                            return
                        },
                        Ok(n) => {
                            {
                                count = n;
                                let node_info = connection_info_arc_dump.lock().await;
                                let v = node_info.get(&proxy_connection_id_dump);
                                match v {
                                    Some((node_info, target_info)) => {
                                        let node_info_dump = node_info.clone();
                                        let mut target_info_dump = target_info.clone();
                                        target_info_dump.add_read_n(n as u64);
                                        connection_info_arc_dump.lock().await.insert(proxy_connection_id_dump.clone(), (node_info_dump, target_info_dump));
                                    },
                                    None => {
                                        connection_info_arc_dump.lock().await.remove(&proxy_connection_id_dump);
                                        eprintln!("|{}| tcp_stream_target_read: not find, disconnect", proxy_connection_id_dump);
                                        return
                                    },
                                }
                            }
                        },
                        Err(e) => {
                            connection_info_arc_dump.lock().await.remove(&proxy_connection_id_dump);
                            eprintln!("|{}| tcp_stream_target_read: failed to read from socket; err = {:?}", proxy_connection_id_dump, e);
                            return;
                        }
                    };
                } else {
                    // read from target timeout
                    connection_info_arc_dump.lock().await.remove(&proxy_connection_id_dump);
                    eprintln!("|{}| tcp_stream_target_read: timeout", proxy_connection_id_dump);
                    return;
                }

                let write_timeout = tokio::time::Duration::from_secs(node_connection_timeout as u64);
                if let Ok(r) = tokio::time::timeout(write_timeout, tcp_stream_node_write.write_all(&buf[0..count])).await {
                    match r {
                        Ok(_) => {
                            {
                                let node_info = connection_info_arc_dump.lock().await;
                                let v = node_info.get(&proxy_connection_id_dump);
                                match v {
                                    Some((node_info, target_info)) => {
                                        let mut node_info_dump = node_info.clone();
                                        let target_info_dump = target_info.clone();
                                        node_info_dump.add_write_n(count as u64);
                                        connection_info_arc_dump.lock().await.insert(proxy_connection_id_dump.clone(), (node_info_dump, target_info_dump));
                                    },
                                    None => {
                                        connection_info_arc_dump.lock().await.remove(&proxy_connection_id_dump);
                                        eprintln!("|{}| tcp_stream_node_write: not find, disconnect", proxy_connection_id_dump);
                                        return;
                                    },
                                }
                            }
                        }
                        Err(e) => {
                            connection_info_arc_dump.lock().await.remove(&proxy_connection_id_dump);
                            eprintln!("|{}| tcp_stream_node_write: failed to write to socket; err = {:?}", proxy_connection_id_dump, e);
                            return;
                        }
                    }
                } else {
                    // write to node timeout
                    connection_info_arc_dump.lock().await.remove(&proxy_connection_id_dump);
                    eprintln!("|{}| tcp_stream_node_write: timeout", proxy_connection_id_dump);
                    return;
                }
            }
        });
    }
}

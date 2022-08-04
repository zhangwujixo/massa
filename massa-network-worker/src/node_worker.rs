// Copyright (c) 2022 MASSA LABS <info@massa.net>

use super::{
    binders::{ReadBinder, WriteBinder},
    messages::{BlockInfoType, Message, MessageTypeId},
};
use itertools::Itertools;
use massa_logging::massa_trace;
use massa_models::{
    constants::{MAX_ASK_BLOCKS_PER_MESSAGE, MAX_ENDORSEMENTS_PER_MESSAGE, NODE_SEND_CHANNEL_SIZE},
    node::NodeId,
    operation::{
        OperationIdsDeserializer, OperationIdsSerializer, OperationsDeserializer,
        OperationsSerializer,
    },
    wrapped::{Id, WrappedSerializer},
};
use massa_models::{BlockId, OperationId, SerializeCompact, SerializeVarInt};
use massa_network_exports::{
    AskForBlocksInfo, BlockInfoReply, ConnectionClosureReason, NetworkError, NetworkSettings,
    NodeCommand, NodeEvent, NodeEventType, ReplyForBlocksInfo,
};
use massa_serialization::Serializer;
use massa_storage::Storage;
use tokio::{
    sync::mpsc,
    sync::mpsc::{
        error::{SendTimeoutError, TrySendError},
        Sender,
    },
    time::timeout,
};
use tracing::{debug, trace, warn};

/// Manages connections
/// One worker per node.
pub struct NodeWorker {
    /// Protocol configuration.
    cfg: NetworkSettings,
    /// Node id associated to that worker.
    node_id: NodeId,
    /// Reader for incoming data.
    socket_reader: ReadBinder,
    /// Optional writer to send data.
    socket_writer_opt: Option<WriteBinder>,
    /// Channel to receive node commands.
    node_command_rx: mpsc::Receiver<NodeCommand>,
    /// Channel to send node events.
    node_event_tx: mpsc::Sender<NodeEvent>,
    /// Shared storage.
    storage: Storage,
}

/// The message to send,
/// or the id(s) of the objects required to construct such a message,
/// so the actual object(s) can be retrieved from shared storage.
/// TODO: decide whether to address the clippy warning.
#[allow(clippy::large_enum_variant)]
pub enum ToSend {
    Msg(Message),
    Header(BlockId),
    ReplyForBlocksInfo(Vec<(BlockId, ReplyForBlocksInfo)>),
    Operations(Vec<OperationId>),
}

impl NodeWorker {
    /// Creates a new node worker
    ///
    /// # Arguments
    /// * `cfg`: Network configuration.
    /// * `node_id`: Node id associated to that worker.
    /// * `socket_reader`: Reader for incoming data.
    /// * `socket_writer`: Writer for sending data.
    /// * `node_command_rx`: Channel to receive node commands.
    /// * `node_event_tx`: Channel to send node events.
    /// * `storage`: Shared storage.
    pub fn new(
        cfg: NetworkSettings,
        node_id: NodeId,
        socket_reader: ReadBinder,
        socket_writer: WriteBinder,
        node_command_rx: mpsc::Receiver<NodeCommand>,
        node_event_tx: mpsc::Sender<NodeEvent>,
        storage: Storage,
    ) -> NodeWorker {
        NodeWorker {
            cfg,
            node_id,
            socket_reader,
            socket_writer_opt: Some(socket_writer),
            node_command_rx,
            node_event_tx,
            storage,
        }
    }

    async fn send_node_event(&self, event: NodeEvent) {
        let result = self
            .node_event_tx
            .send_timeout(event, self.cfg.max_send_wait.to_duration())
            .await;
        match result {
            Ok(()) => {}
            Err(SendTimeoutError::Closed(event)) => {
                debug!(
                    "Failed to send NodeEvent due to channel closure: {:?}.",
                    event
                );
            }
            Err(SendTimeoutError::Timeout(event)) => {
                debug!("Failed to send NodeEvent due to timeout: {:?}.", event);
            }
        }
    }

    /// Tries to send a message to a node
    /// If the pipe is full, simply warn
    /// If the channel dropped, return an error
    pub fn try_send_to_node(
        &self,
        sender: &Sender<ToSend>,
        msg: ToSend,
    ) -> Result<(), NetworkError> {
        match sender.try_send(msg) {
            Err(TrySendError::Full(_)) => {
                debug!(
                    "failed sending message to node {}: send channel full",
                    self.node_id
                );
                Ok(())
            }
            Err(TrySendError::Closed(_)) => {
                debug!("failed sending message deconnected {}.", self.node_id);
                Err(NetworkError::ChannelError(
                    "failed sending message to node: channel closed".into(),
                ))
            }
            Ok(_) => Ok(()),
        }
    }

    /// node event loop. Consumes self.
    pub async fn run_loop(mut self) -> Result<ConnectionClosureReason, NetworkError> {
        let (writer_command_tx, mut writer_command_rx) =
            mpsc::channel::<ToSend>(NODE_SEND_CHANNEL_SIZE);
        let mut socket_writer = self.socket_writer_opt.take().ok_or_else(|| {
            NetworkError::GeneralProtocolError(
                "NodeWorker call run_loop more than once".to_string(),
            )
        })?;
        let write_timeout = self.cfg.message_timeout;
        let node_id_copy = self.node_id;
        let storage = self.storage.clone();
        let node_writer_handle = tokio::spawn(async move {
            loop {
                match writer_command_rx.recv().await {
                    Some(to_send) => {
                        let bytes_vec: Vec<u8> = match to_send {
                            ToSend::Msg(msg) => msg.to_bytes_compact().unwrap(),
                            ToSend::ReplyForBlocksInfo(reply_list) => {
                                let mut res: Vec<u8> = Vec::new();
                                res.extend(
                                    u32::from(MessageTypeId::ReplyForBlocks).to_varint_bytes(),
                                );
                                res.extend((reply_list.len() as u32).to_varint_bytes());
                                for (hash, info) in reply_list {
                                    res.extend(hash.to_bytes());
                                    match info {
                                        ReplyForBlocksInfo::Info(op_ids) => {
                                            res.extend(
                                                u32::from(BlockInfoType::Info).to_varint_bytes(),
                                            );
                                            let op_ids_serializer = OperationIdsSerializer::new();
                                            op_ids_serializer.serialize(&op_ids, &mut res)?;
                                        }
                                        ReplyForBlocksInfo::Operations(operation_ids) => {
                                            res.extend(
                                                u32::from(BlockInfoType::Operations)
                                                    .to_varint_bytes(),
                                            );
                                            let len =
                                                (operation_ids.len() as u32).to_varint_bytes();
                                            res.extend(len);
                                            let wrapped_operation_serializer =
                                                WrappedSerializer::new();
                                            let ops: Vec<OperationId> =
                                                operation_ids.into_iter().collect();
                                            storage.with_operations(&ops, |operations| {
                                                for operation in operations {
                                                    match operation {
                                                        Some(operation) => {
                                                            wrapped_operation_serializer
                                                                .serialize(*operation, &mut res)?;
                                                        }
                                                        None => {
                                                            return Err(
                                                                NetworkError::MissingOperation,
                                                            )
                                                        }
                                                    }
                                                }
                                                Ok(())
                                            })?;
                                        }
                                        ReplyForBlocksInfo::NotFound => {
                                            res.extend(
                                                u32::from(BlockInfoType::NotFound)
                                                    .to_varint_bytes(),
                                            );
                                        }
                                    }
                                }
                                res
                            }
                            ToSend::Header(block_id) => {
                                // Construct the message,
                                // using the serialized header retrieved from shared storage.
                                let mut res: Vec<u8> = Vec::new();
                                res.extend(u32::from(MessageTypeId::BlockHeader).to_varint_bytes());

                                let block = storage
                                    .retrieve_block(&block_id)
                                    .ok_or(NetworkError::MissingBlock)?;
                                let stored_block = block.read();
                                WrappedSerializer::new()
                                    .serialize(&stored_block.content.header, &mut res)?;
                                res
                            }
                            ToSend::Operations(operation_ids) => {
                                // Construct the message,
                                // using the serialized operations retrieved from shared storage.
                                let mut res: Vec<u8> = Vec::new();
                                res.extend(u32::from(MessageTypeId::Operations).to_varint_bytes());
                                let len = (operation_ids.len() as u32).to_varint_bytes();
                                res.extend(len);
                                let wrapped_operation_serializer = WrappedSerializer::new();

                                storage.with_operations(&operation_ids, |operations| {
                                    for operation in operations {
                                        match operation {
                                            Some(operation) => {
                                                wrapped_operation_serializer
                                                    .serialize(*operation, &mut res)?;
                                            }
                                            None => return Err(NetworkError::MissingOperation),
                                        }
                                    }
                                    Ok(())
                                })?;

                                res
                            }
                        };
                        match timeout(write_timeout.to_duration(), socket_writer.send(&bytes_vec))
                            .await
                        {
                            Err(_err) => {
                                massa_trace!("node_worker.run_loop.loop.writer_command_rx.recv.send.timeout", {
                                    "node": node_id_copy,
                                });
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "node data writing timed out",
                                )
                                .into());
                            }
                            Ok(Err(err)) => {
                                massa_trace!("node_worker.run_loop.loop.writer_command_rx.recv.send.error", {
                                    "node": node_id_copy, "err":  format!("{}", err),
                                });
                                return Err(err);
                            }
                            Ok(Ok(id)) => {
                                massa_trace!("node_worker.run_loop.loop.writer_command_rx.recv.send.ok", {
                                    "node": node_id_copy, "msg_id": id,
                                })
                            }
                        }
                    }
                    None => {
                        massa_trace!("node_worker.run_loop.loop.writer_command_rx.recv. None", {});
                        break;
                    }
                };
            }
            Ok(())
        });
        tokio::pin!(node_writer_handle);
        let mut writer_joined = false;

        let mut ask_peer_list_interval =
            tokio::time::interval(self.cfg.ask_peer_list_interval.to_duration());
        let mut exit_reason = ConnectionClosureReason::Normal;
        'select_loop: loop {
            /*
                select! without the "biased" modifier will randomly select the 1st branch to check,
                then will check the next ones in the order they are written.
                We choose this order:
                    * node_writer_handle (rare) to immediately register a stop and avoid wasting resources
                    * incoming socket data (high frequency): forward incoming data in priority to avoid contention
                    * node commands (high frequency): try to send, fail on contention
                    * ask peers: low frequency, non-critical
            */
            tokio::select! {
                res = &mut node_writer_handle => {
                    writer_joined = true;
                    match res {
                        Err(err) => {
                            massa_trace!("node_worker.run_loop.node_writer_handle.panic", {"node": self.node_id, "err": format!("{}", err)});
                            warn!("writer exited unexpectedly for node {}", self.node_id);
                            if exit_reason != ConnectionClosureReason::Banned {
                                exit_reason = ConnectionClosureReason::Failed;
                            }
                            break;
                        },
                        Ok(Err(err)) => {
                            massa_trace!("node_worker.run_loop.node_writer_handle.error", {"node": self.node_id, "err": format!("{}", err)});
                            if exit_reason != ConnectionClosureReason::Banned {
                                exit_reason = ConnectionClosureReason::Failed;
                            }
                            break;
                        },
                        Ok(Ok(())) => {
                            massa_trace!("node_worker.run_loop.node_writer_handle.clean_exit", {"node": self.node_id});
                            break;
                        }
                    }
                },

                // incoming socket data
                res = self.socket_reader.next() => match res {
                    Ok(Some((index, msg))) => {
                        massa_trace!(
                            "node_worker.run_loop. receive self.socket_reader.next()", {"index": index});
                        match msg {
                            Message::BlockHeader(header) => {
                                massa_trace!(
                                    "node_worker.run_loop. receive Message::BlockHeader",
                                    {"block_id": header.id.hash(), "header": header, "node": self.node_id}
                                );
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedBlockHeader(header))).await;
                            },
                            Message::AskForBlocks(list) => {
                                massa_trace!("node_worker.run_loop. receive Message::AskForBlocks", {"hashlist": list, "node": self.node_id});
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedAskForBlocks(list))).await;
                            }
                            Message::ReplyForBlocks(list) => {
                                massa_trace!("node_worker.run_loop. receive Message::AskForBlocks", {"hashlist": list, "node": self.node_id});
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedReplyForBlocks(list))).await;
                            }
                            Message::PeerList(pl) =>  {
                                massa_trace!("node_worker.run_loop. receive Message::PeerList", {"peerlist": pl, "node": self.node_id});
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedPeerList(pl))).await;
                            }
                            Message::AskPeerList => {
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::AskedPeerList)).await;
                            }
                            Message::Operations(operations) => {
                                massa_trace!(
                                    "node_worker.run_loop. receive Message::Operations: ",
                                    {"node": self.node_id, "operations": operations}
                                );
                                //massa_trace!("node_worker.run_loop. receive Message::Operations", {"node": self.node_id, "operations": operations});
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedOperations(operations))).await;
                            }
                            Message::AskForOperations(operation_prefix_ids) => {
                                massa_trace!(
                                    "node_worker.run_loop. receive Message::AskForOperations: ",
                                    {"node": self.node_id, "operation_ids": operation_prefix_ids}
                                );
                                //massa_trace!("node_worker.run_loop. receive Message::AskForOperations", {"node": self.node_id, "operations": operation_ids});
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedAskForOperations(operation_prefix_ids))).await;
                            }
                            Message::OperationsAnnouncement(operation_prefix_ids) => {
                                massa_trace!("node_worker.run_loop. receive Message::OperationsBatch", {"node": self.node_id, "operation_prefix_ids": operation_prefix_ids});
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedOperationAnnouncements(operation_prefix_ids))).await;
                            }
                            Message::Endorsements(endorsements) => {
                                massa_trace!("node_worker.run_loop. receive Message::Endorsement", {"node": self.node_id, "endorsements": endorsements});
                                self.send_node_event(NodeEvent(self.node_id, NodeEventType::ReceivedEndorsements(endorsements))).await;
                            }
                            _ => {
                                // TODO: Write a more user-friendly warning/logout after several consecutive fails? see #1082
                                massa_trace!("node_worker.run_loop.self.socket_reader.next(). Unexpected message Warning", {});
                            },
                        }
                    },
                    Ok(None)=> {
                        massa_trace!("node_worker.run_loop.self.socket_reader.next(). Ok(None) Error", {});
                        break
                    }, // peer closed cleanly
                    Err(err) => {  // stream error
                        massa_trace!("node_worker.run_loop.self.socket_reader.next(). receive error", {"error": format!("{}", err)});
                        exit_reason = ConnectionClosureReason::Failed;
                        break;
                    },
                },

                // node command
                cmd = self.node_command_rx.recv() => {
                    match cmd {
                        Some(NodeCommand::Close(r)) => {
                            exit_reason = r;
                            break;
                        },
                        Some(NodeCommand::SendPeerList(ip_vec)) => {
                            massa_trace!("node_worker.run_loop. send Message::PeerList", {"peerlist": ip_vec, "node": self.node_id});
                            if self.try_send_to_node(&writer_command_tx, ToSend::Msg(Message::PeerList(ip_vec))).is_err() {
                                break;
                            }
                        },
                        Some(NodeCommand::SendBlockHeader(block_id)) => {
                            massa_trace!("node_worker.run_loop. send Message::BlockHeader", {"hash": block_id, "node": self.node_id});
                            if self.try_send_to_node(&writer_command_tx, ToSend::Header(block_id)).is_err() {
                                break;
                            }
                        },
                        Some(NodeCommand::AskForBlocks(list)) => {
                            // cut hash list on sub list if exceed max_ask_blocks_per_message
                            massa_trace!("node_worker.run_loop. send Message::AskForBlocks", {"hashlist": list, "node": self.node_id});
                            for to_send_list in list.chunks(MAX_ASK_BLOCKS_PER_MESSAGE as usize) {
                                if self.try_send_to_node(&writer_command_tx, ToSend::Msg(Message::AskForBlocks(to_send_list.to_vec()))).is_err() {
                                    break 'select_loop;
                                }
                            }
                        },
                        Some(NodeCommand::ReplyForBlocks(list)) => {
                            // cut hash list on sub list if exceed max_ask_blocks_per_message
                            massa_trace!("node_worker.run_loop. send Message::ReplyForBlocks", {"hashlist": list, "node": self.node_id});
                            for to_send_list in list.chunks(MAX_ASK_BLOCKS_PER_MESSAGE as usize) {
                                if self.try_send_to_node(&writer_command_tx, ToSend::ReplyForBlocksInfo(to_send_list.to_vec())).is_err() {
                                    break 'select_loop;
                                }
                            }
                        },
                        Some(NodeCommand::SendOperations(operations)) => {
                            massa_trace!("node_worker.run_loop. send Message::SendOperations", {"node": self.node_id, "operations": operations});
                            let ops: Vec<OperationId> = operations.into_iter().collect();
                            for chunk in ops.chunks(self.cfg.max_operations_per_message as usize) {
                                if self.try_send_to_node(&writer_command_tx, ToSend::Operations(chunk.into())).is_err() {
                                    break 'select_loop;
                                }
                            }
                        },
                        Some(NodeCommand::SendOperationAnnouncements(operation_prefix_ids)) => {
                            massa_trace!("node_worker.run_loop. send Message::OperationsAnnouncement", {"node": self.node_id, "operation_ids": operation_prefix_ids});
                            for chunk in operation_prefix_ids
                            .into_iter()
                            .chunks(self.cfg.max_operations_per_message as usize)
                            .into_iter()
                            .map(|chunk| chunk.collect()) {
                                if self.try_send_to_node(&writer_command_tx, ToSend::Msg(Message::OperationsAnnouncement(chunk))).is_err() {
                                    break 'select_loop;
                                }
                            }
                        }
                        Some(NodeCommand::AskForOperations(operation_prefix_ids)) => {
                            //massa_trace!("node_worker.run_loop. send Message::AskForOperations", {"node": self.node_id, "operation_ids": operation_ids});
                            massa_trace!(
                                "node_worker.run_loop. send Message::AskForOperations",
                                {"node": self.node_id, "operation_ids": operation_prefix_ids}
                            );
                            for chunk in operation_prefix_ids
                            .into_iter()
                            .chunks(self.cfg.max_operations_per_message as usize)
                            .into_iter()
                            .map(|chunk| chunk.collect()) {
                                if self.try_send_to_node(&writer_command_tx, ToSend::Msg(Message::AskForOperations(chunk))).is_err() {
                                    break 'select_loop;
                                }
                            }
                        }
                        Some(NodeCommand::SendEndorsements(endorsements)) => {
                            massa_trace!("node_worker.run_loop. send Message::SendEndorsements", {"node": self.node_id, "endorsements": endorsements});
                            // cut endorsement list if it exceed max_endorsements_per_message
                            for to_send_list in endorsements.chunks(MAX_ENDORSEMENTS_PER_MESSAGE as usize) {
                                if self.try_send_to_node(&writer_command_tx, ToSend::Msg(Message::Endorsements(to_send_list.to_vec()))).is_err() {
                                    break 'select_loop;
                                }
                            }
                        },
                        None => {
                            // Note: this should never happen,
                            // since it implies the network worker dropped its node command sender
                            // before having shut-down the node and joined on its handle.
                            return Err(NetworkError::UnexpectedNodeCommandChannelClosure);
                        },
                    };
                },

                _ = ask_peer_list_interval.tick() => {
                    debug!("timer-based asking node_id={} for peer list", self.node_id);
                    massa_trace!("node_worker.run_loop. timer_ask_peer_list", {"node_id": self.node_id});
                    massa_trace!("node_worker.run_loop.select.timer send Message::AskPeerList", {"node": self.node_id});
                    writer_command_tx.send(ToSend::Msg(Message::AskPeerList)).await.map_err(
                        |_| NetworkError::ChannelError("writer send ask peer list failed".into())
                    )?;
                    trace!("after sending Message::AskPeerList from writer_command_tx in node_worker run_loop");
                }
            }
        }

        // Note: since we close the channel here,
        // if the network worker tries to send additional commands,
        // those sends will fail with an error.
        self.node_command_rx.close();

        // 1. Close writer command channel.
        drop(writer_command_tx);

        // 2. Join on the writer handle.
        if !writer_joined {
            match node_writer_handle.await {
                Err(err) => {
                    massa_trace!("node_worker.run_loop.cleanup.node_writer_handle.panic", {"node": self.node_id, "err": format!("{}", err)});
                    warn!("writer exited unexpectedly for node {}", self.node_id);
                    exit_reason = ConnectionClosureReason::Failed;
                }
                Ok(Err(err)) => {
                    massa_trace!("node_worker.run_loop.cleanup.node_writer_handle.error", {"node": self.node_id, "err": format!("{}", err)});
                    exit_reason = ConnectionClosureReason::Failed;
                }
                Ok(Ok(())) => {
                    massa_trace!("node_worker.run_loop.cleanup.node_writer_handle.clean_exit", {"node": self.node_id});
                }
            }
        }

        Ok(exit_reason)
    }
}

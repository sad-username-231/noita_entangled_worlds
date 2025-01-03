use bitcode::{Decode, Encode};
use rayon::iter::IntoParallelIterator;
use rayon::iter::ParallelIterator;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::f32::consts::TAU;
use std::{env, mem};
use tracing::{debug, info, warn};
use world_model::{
    chunk::{Chunk, Pixel},
    ChunkCoord, ChunkData, ChunkDelta, WorldModel, CHUNK_SIZE,
};

pub use world_model::encoding::NoitaWorldUpdate;

use crate::bookkeeping::save_state::{SaveState, SaveStateEntry};

use super::{
    messages::{Destination, MessageRequest},
    omni::OmniPeerId,
    DebugMarker,
};

pub mod world_info;
pub mod world_model;

#[derive(Debug, Serialize, Deserialize)]
pub enum WorldUpdateKind {
    Update(NoitaWorldUpdate),
    End,
}

#[derive(Debug, Decode, Encode, Clone)]
pub(crate) enum WorldNetMessage {
    // Authority request
    RequestAuthority {
        chunk: ChunkCoord,
        priority: u8,
        can_wait: bool,
    },
    // have peer make Authority request
    AskForAuthority {
        chunk: ChunkCoord,
        priority: u8,
    },
    // switch peer to temp authority
    LoseAuthority {
        chunk: ChunkCoord,
        new_priority: u8,
        new_authority: OmniPeerId,
    },
    // Change priority
    ChangePriority {
        chunk: ChunkCoord,
        priority: u8,
    },
    // When got authority
    GotAuthority {
        chunk: ChunkCoord,
        chunk_data: Option<ChunkData>,
        priority: u8,
    },
    // Tell host that someone is losing authority
    RelinquishAuthority {
        chunk: ChunkCoord,
        chunk_data: Option<ChunkData>,
        world_num: i32,
    },
    // Ttell how to update a chunk storage
    UpdateStorage {
        chunk: ChunkCoord,
        chunk_data: Option<ChunkData>,
        world_num: i32,
    },
    // When listening
    AuthorityAlreadyTaken {
        chunk: ChunkCoord,
        authority: OmniPeerId,
    },
    ListenRequest {
        chunk: ChunkCoord,
    },
    ListenStopRequest {
        chunk: ChunkCoord,
    },
    UnloadChunk {
        chunk: ChunkCoord,
    },
    // Listen responses/messages
    ListenInitialResponse {
        chunk: ChunkCoord,
        chunk_data: Option<ChunkData>,
        priority: u8,
    },
    ListenUpdate {
        delta: ChunkDelta,
        priority: u8,
        take_auth: bool,
    },
    ChunkPacket {
        chunkpacket: Vec<(ChunkDelta, u8)>,
    },
    ListenAuthorityRelinquished {
        chunk: ChunkCoord,
    },
    // Authority transfer stuff (due to priority)
    GetAuthorityFrom {
        chunk: ChunkCoord,
        current_authority: OmniPeerId,
    },
    RequestAuthorityTransfer {
        chunk: ChunkCoord,
    },
    TransferOk {
        chunk: ChunkCoord,
        chunk_data: Option<ChunkData>,
        listeners: FxHashSet<OmniPeerId>,
    },
    TransferFailed {
        chunk: ChunkCoord,
    },
    NotifyNewAuthority {
        chunk: ChunkCoord,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum ChunkState {
    /// Chunk isn't synced yet, but will request authority for it.
    RequestAuthority { priority: u8, can_wait: bool },
    /// Transitioning into Listening or Authority state.
    WaitingForAuthority,
    /// Listening for chunk updates from this peer.
    Listening { authority: OmniPeerId, priority: u8 },
    /// Sending chunk updates to these listeners.
    Authority {
        listeners: FxHashSet<OmniPeerId>,
        priority: u8,
        new_authority: Option<(OmniPeerId, u8)>,
        stop_sending: bool,
    },
    /// Chunk is to be cleaned up.
    UnloadPending,
    /// We've requested to take authority from someone else, and waiting for transfer to complete.
    Transfer,
    /// Has higher priority and is waiting for next chunk update
    WantToGetAuth {
        authority: OmniPeerId,
        auth_priority: u8,
        my_priority: u8,
    },
}
impl ChunkState {
    fn authority(priority: u8) -> ChunkState {
        ChunkState::Authority {
            listeners: Default::default(),
            priority,
            new_authority: None,
            stop_sending: false,
        }
    }
}
// TODO handle exits.
pub(crate) struct WorldManager {
    pub nice_terraforming: bool,
    is_host: bool,
    my_pos: (i32, i32),
    cam_pos: (i32, i32),
    is_notplayer: bool,
    my_peer_id: OmniPeerId,
    save_state: SaveState,
    /// We receive changes from other clients here, intending to send them to Noita.
    inbound_model: WorldModel,
    /// We use that to create changes to be sent to other clients.
    outbound_model: WorldModel,
    /// Stores chunks that aren't under any authority.
    chunk_storage: FxHashMap<ChunkCoord, ChunkData>,
    /// Who is the current chunk authority.
    authority_map: FxHashMap<ChunkCoord, (OmniPeerId, u8)>,
    /// Chunk states, according to docs/distributed_world_sync.drawio
    chunk_state: FxHashMap<ChunkCoord, ChunkState>,
    emitted_messages: Vec<MessageRequest<WorldNetMessage>>,
    /// Which update it is?
    /// Incremented every time `add_end()` gets called.
    current_update: u64,
    /// Update number in which chunk has been updated locally.
    /// Used to track which chunks can be unloaded.
    chunk_last_update: FxHashMap<ChunkCoord, u64>,
    /// Stores last priority we used for that chunk, in case transfer fails and we'll need to request authority normally.
    last_request_priority: FxHashMap<ChunkCoord, u8>,
    world_num: i32,
    pub durabilities: HashMap<u16, (u8, u32)>,
}

impl WorldManager {
    pub(crate) fn new(is_host: bool, my_peer_id: OmniPeerId, save_state: SaveState) -> Self {
        let chunk_storage = save_state.load().unwrap_or_default();
        WorldManager {
            nice_terraforming: true,
            is_host,
            my_pos: (i32::MIN / 2, i32::MIN / 2),
            cam_pos: (i32::MIN / 2, i32::MIN / 2),
            is_notplayer: false,
            my_peer_id,
            save_state,
            inbound_model: Default::default(),
            outbound_model: Default::default(),
            authority_map: Default::default(),
            chunk_storage,
            chunk_state: Default::default(),
            emitted_messages: Default::default(),
            current_update: 0,
            chunk_last_update: Default::default(),
            last_request_priority: Default::default(),
            world_num: 0,
            durabilities: HashMap::new(),
        }
    }

    pub(crate) fn add_update(&mut self, update: NoitaWorldUpdate) {
        self.outbound_model.apply_noita_update(&update);
    }

    pub(crate) fn add_end(&mut self, priority: u8, pos: &[i32]) {
        let updated_chunks = self
            .outbound_model
            .updated_chunks()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        self.current_update += 1;
        let mut chunks_to_send = Vec::new();
        for chunk in updated_chunks.clone() {
            chunks_to_send.push(self.chunk_updated_locally(chunk, priority, pos));
        }
        let mut chunk_packet: HashMap<OmniPeerId, Vec<(ChunkDelta, u8)>> = HashMap::new();
        for (chunk, who_sending) in updated_chunks.iter().zip(chunks_to_send.iter()) {
            let Some(delta) = self.outbound_model.get_chunk_delta(*chunk, false) else {
                continue;
            };
            for (peer, pri) in who_sending {
                chunk_packet
                    .entry(*peer)
                    .or_default()
                    .push((delta.clone(), *pri));
            }
        }
        let mut emit_queue = Vec::new();
        for (peer, chunkpacket) in chunk_packet {
            emit_queue.push((
                Destination::Peer(peer),
                WorldNetMessage::ChunkPacket { chunkpacket },
            ));
        }
        for (dst, msg) in emit_queue {
            self.emit_msg(dst, msg)
        }
        self.outbound_model.reset_change_tracking();
    }

    fn chunk_updated_locally(
        &mut self,
        chunk: ChunkCoord,
        priority: u8,
        pos: &[i32],
    ) -> Vec<(OmniPeerId, u8)> {
        if pos.len() == 6 {
            self.my_pos = (pos[0], pos[1]);
            self.cam_pos = (pos[2], pos[3]);
            self.is_notplayer = pos[4] == 1;
            if self.world_num != pos[5] {
                self.world_num = pos[5];
                self.reset();
            }
        } else if self.world_num != pos[0] {
            self.world_num = pos[0];
            self.reset();
        }
        let entry = self.chunk_state.entry(chunk).or_insert_with(|| {
            debug!("Created entry for {chunk:?}");
            ChunkState::RequestAuthority {
                priority,
                can_wait: true,
            }
        });
        let mut emit_queue = Vec::new();
        self.chunk_last_update.insert(chunk, self.current_update);
        let mut chunks_to_send = Vec::new();
        match entry {
            ChunkState::Listening {
                authority,
                priority: pri,
            } => {
                if *pri > priority {
                    let cs = ChunkState::WantToGetAuth {
                        authority: *authority,
                        auth_priority: *pri,
                        my_priority: priority,
                    };
                    emit_queue.push((
                        Destination::Peer(*authority),
                        WorldNetMessage::LoseAuthority {
                            chunk,
                            new_priority: priority,
                            new_authority: self.my_peer_id,
                        },
                    ));
                    self.chunk_state.insert(chunk, cs);
                }
            }
            ChunkState::WantToGetAuth {
                authority,
                auth_priority: auth_pri,
                my_priority: my_pri,
            } => {
                if *my_pri != priority {
                    *my_pri = priority;
                    if *auth_pri <= priority {
                        let cs = ChunkState::Listening {
                            authority: *authority,
                            priority: *auth_pri,
                        };
                        self.chunk_state.insert(chunk, cs);
                    } else {
                        emit_queue.push((
                            Destination::Peer(*authority),
                            WorldNetMessage::LoseAuthority {
                                chunk,
                                new_priority: priority,
                                new_authority: self.my_peer_id,
                            },
                        ));
                    }
                }
            }
            ChunkState::Authority {
                listeners,
                priority: pri,
                new_authority,
                stop_sending,
            } => {
                let Some(delta) = self.outbound_model.get_chunk_delta(chunk, false) else {
                    return Vec::new();
                };
                if *pri != priority {
                    *pri = priority;
                    emit_queue.push((
                        Destination::Host,
                        WorldNetMessage::ChangePriority { chunk, priority },
                    ));
                }
                let mut new_auth = None;
                if let Some(new) = new_authority {
                    if new.1 >= priority {
                        *new_authority = None;
                        *stop_sending = false
                    } else {
                        new_auth = Some(new.0)
                    }
                } else {
                    *stop_sending = false
                }
                let mut new_auth_got = false;
                if !*stop_sending {
                    for &listener in listeners.iter() {
                        let take_auth = new_auth == Some(listener);
                        if take_auth {
                            new_auth_got = true
                        }
                        if take_auth {
                            emit_queue.push((
                                Destination::Peer(listener),
                                WorldNetMessage::ListenUpdate {
                                    delta: delta.clone(),
                                    priority,
                                    take_auth,
                                },
                            ));
                            chunks_to_send = Vec::new()
                        } else {
                            chunks_to_send.push((listener, priority));
                        }
                    }
                }
                if new_auth_got && new_auth.is_some() {
                    *stop_sending = true
                }
            }
            _ => {}
        }
        for (dst, msg) in emit_queue {
            self.emit_msg(dst, msg)
        }
        chunks_to_send
    }

    pub(crate) fn update(&mut self) {
        fn should_kill(
            my_pos: (i32, i32),
            cam_pos: (i32, i32),
            chx: i32,
            chy: i32,
            is_notplayer: bool,
        ) -> bool {
            let (x, y) = my_pos;
            let (cx, cy) = cam_pos;
            if (x - cx).abs() > 2 || (y - cy).abs() > 2 {
                !(chx <= x + 2 && chx >= x - 2 && chy <= y + 2 && chy >= y - 2
                    || chx <= cx + 2 && chx >= cx - 2 && chy <= cy + 2 && chy >= cy - 2)
            } else if is_notplayer {
                !(chx <= x + 2 && chx >= x - 2 && chy <= y + 2 && chy >= y - 2)
            } else {
                !(chx <= x + 3 && chx >= x - 3 && chy <= y + 3 && chy >= y - 3)
            }
        }
        let mut emit_queue = Vec::new();
        for (&chunk, state) in self.chunk_state.iter_mut() {
            let chunk_last_update = self
                .chunk_last_update
                .get(&chunk)
                .copied()
                .unwrap_or_default();
            match state {
                ChunkState::RequestAuthority { priority, can_wait } => {
                    let priority = *priority;
                    emit_queue.push((
                        Destination::Host,
                        WorldNetMessage::RequestAuthority {
                            chunk,
                            priority,
                            can_wait: *can_wait,
                        },
                    ));
                    *state = ChunkState::WaitingForAuthority;
                    self.last_request_priority.insert(chunk, priority);
                    debug!("Requested authority for {chunk:?}")
                }
                // This state doesn't have much to do.
                ChunkState::WaitingForAuthority => {
                    if should_kill(
                        self.my_pos,
                        self.cam_pos,
                        chunk.0,
                        chunk.1,
                        self.is_notplayer,
                    ) {
                        *state = ChunkState::UnloadPending;
                    }
                }
                ChunkState::Listening { authority, .. } => {
                    if should_kill(
                        self.my_pos,
                        self.cam_pos,
                        chunk.0,
                        chunk.1,
                        self.is_notplayer,
                    ) {
                        debug!("Unloading [listening] chunk {chunk:?}");
                        emit_queue.push((
                            Destination::Peer(*authority),
                            WorldNetMessage::ListenStopRequest { chunk },
                        ));
                        *state = ChunkState::UnloadPending;
                    }
                }
                ChunkState::Authority { new_authority, .. } => {
                    if should_kill(
                        self.my_pos,
                        self.cam_pos,
                        chunk.0,
                        chunk.1,
                        self.is_notplayer,
                    ) {
                        if let Some(new) = new_authority {
                            emit_queue.push((
                                Destination::Peer(new.0),
                                WorldNetMessage::AskForAuthority {
                                    chunk,
                                    priority: new.1,
                                },
                            ));
                        }
                        debug!("Unloading [authority] chunk {chunk:?} (updates: {chunk_last_update} {})", self.current_update);
                        emit_queue.push((
                            Destination::Host,
                            WorldNetMessage::RelinquishAuthority {
                                chunk,
                                chunk_data: self.outbound_model.get_chunk_data(chunk),
                                world_num: self.world_num,
                            },
                        ));
                        *state = ChunkState::UnloadPending;
                    }
                }
                ChunkState::WantToGetAuth { .. } => {
                    if should_kill(
                        self.my_pos,
                        self.cam_pos,
                        chunk.0,
                        chunk.1,
                        self.is_notplayer,
                    ) {
                        debug!("Unloading [want to get auth] chunk {chunk:?}");
                        *state = ChunkState::UnloadPending;
                    }
                }
                ChunkState::UnloadPending => {}
                ChunkState::Transfer => {}
            }
        }

        for (dst, msg) in emit_queue {
            self.emit_msg(dst, msg)
        }
        self.chunk_state.retain(|chunk, state| {
            let retain = *state != ChunkState::UnloadPending;
            if !retain {
                // Models are basically caches, no need to keep the chunk around in them.
                self.inbound_model.forget_chunk(*chunk);
                self.outbound_model.forget_chunk(*chunk);
            }
            retain
        });
    }

    pub(crate) fn get_noita_updates(&mut self) -> Vec<Vec<u8>> {
        // Sends random data to noita to check if it crashes.
        if env::var_os("NP_WORLD_SYNC_TEST").is_some() && self.current_update % 10 == 0 {
            let chunk_data = ChunkData::make_random();
            self.inbound_model
                .apply_chunk_data(ChunkCoord(0, 0), &chunk_data)
        }
        let updates = self.inbound_model.get_all_noita_updates();
        self.inbound_model.reset_change_tracking();
        updates
    }

    pub(crate) fn reset(&mut self) {
        self.inbound_model.reset();
        self.outbound_model.reset();
        self.chunk_storage.clear();
        self.authority_map.clear();
        self.chunk_last_update.clear();
        self.chunk_state.clear();
    }

    pub(crate) fn get_emitted_msgs(&mut self) -> Vec<MessageRequest<WorldNetMessage>> {
        mem::take(&mut self.emitted_messages)
    }

    fn emit_msg(&mut self, dst: Destination, msg: WorldNetMessage) {
        // Short-circuit for messages intended for myself
        if (self.is_host && dst == Destination::Host) || dst == Destination::Peer(self.my_peer_id) {
            self.handle_msg(self.my_peer_id, msg);
            return;
        }
        // Also handle broadcast messages this way.
        if dst == Destination::Broadcast {
            self.handle_msg(self.my_peer_id, msg.clone());
        }

        self.emitted_messages.push(MessageRequest {
            reliability: tangled::Reliability::Reliable,
            dst,
            msg,
        })
    }

    fn emit_got_authority(&mut self, chunk: ChunkCoord, source: OmniPeerId, priority: u8) {
        let auth = self.authority_map.get(&chunk).cloned();
        self.authority_map.insert(chunk, (source, priority));
        let chunk_data = if auth.map(|a| a.0 != source).unwrap_or(true) {
            self.chunk_storage.get(&chunk).cloned()
        } else {
            None
        };
        self.emit_msg(
            Destination::Peer(source),
            WorldNetMessage::GotAuthority {
                chunk,
                chunk_data,
                priority,
            },
        );
    }

    fn emit_transfer_authority(
        &mut self,
        chunk: ChunkCoord,
        source: OmniPeerId,
        priority: u8,
        current_authority: OmniPeerId,
    ) {
        self.authority_map.insert(chunk, (source, priority));
        self.emit_msg(
            Destination::Peer(source),
            WorldNetMessage::GetAuthorityFrom {
                chunk,
                current_authority,
            },
        );
    }

    pub(crate) fn handle_msg(&mut self, source: OmniPeerId, msg: WorldNetMessage) {
        match msg {
            WorldNetMessage::RequestAuthority {
                chunk,
                priority,
                can_wait,
            } => {
                if !self.is_host {
                    warn!("{} sent RequestAuthority to not-host.", source);
                    return;
                }
                let current_authority = self.authority_map.get(&chunk).copied();
                match current_authority {
                    Some((authority, priority_state)) => {
                        if source == authority {
                            debug!("{source} already has authority of {chunk:?}");
                            self.emit_got_authority(chunk, source, priority);
                        } else if priority_state > priority && !can_wait {
                            debug!("{source} is gaining priority over {chunk:?} from {authority}");
                            self.emit_transfer_authority(chunk, source, priority, authority);
                        } else {
                            debug!("{source} requested authority for {chunk:?}, but it's already taken by {authority}");
                            self.emit_msg(
                                Destination::Peer(source),
                                WorldNetMessage::AuthorityAlreadyTaken { chunk, authority },
                            );
                        }
                    }
                    None => {
                        debug!("Granting {source} authority of {chunk:?}");
                        self.emit_got_authority(chunk, source, priority);
                    }
                }
            }
            WorldNetMessage::AskForAuthority { chunk, priority } => {
                self.emit_msg(
                    Destination::Host,
                    WorldNetMessage::RequestAuthority {
                        chunk,
                        priority,
                        can_wait: false,
                    },
                );
                self.chunk_state
                    .insert(chunk, ChunkState::WaitingForAuthority);
            }
            WorldNetMessage::LoseAuthority {
                chunk,
                new_authority,
                new_priority,
            } => {
                if let Some(ChunkState::Authority {
                    new_authority: new_auth,
                    ..
                }) = self.chunk_state.get_mut(&chunk)
                {
                    if new_authority == self.my_peer_id {
                        *new_auth = None;
                    } else if let Some(new) = new_auth {
                        if new.1 > new_priority {
                            *new_auth = Some((new_authority, new_priority));
                        }
                    } else {
                        *new_auth = Some((new_authority, new_priority))
                    }
                }
            }
            WorldNetMessage::ChangePriority { chunk, priority } => {
                if !self.is_host {
                    warn!("{} sent RequestAuthority to not-host.", source);
                    return;
                }
                let current_authority = self.authority_map.get(&chunk).copied();
                match current_authority {
                    Some((authority, _)) => {
                        if source == authority {
                            self.authority_map.insert(chunk, (source, priority));
                        } else {
                            debug!("{source} requested authority for {chunk:?}, but it's already taken by {authority}");
                        }
                    }
                    None => {
                        debug!("Granting {source} authority of {chunk:?}");
                    }
                }
            }
            WorldNetMessage::GotAuthority {
                chunk,
                chunk_data,
                priority,
            } => {
                self.chunk_state
                    .insert(chunk, ChunkState::authority(priority));
                self.last_request_priority.remove(&chunk);
                if let Some(chunk_data) = chunk_data {
                    self.inbound_model.apply_chunk_data(chunk, &chunk_data);
                    self.outbound_model.apply_chunk_data(chunk, &chunk_data);
                }
            }
            WorldNetMessage::UpdateStorage {
                chunk,
                chunk_data,
                world_num,
            } => {
                if !self.is_host {
                    warn!("{} sent RelinquishAuthority to not-host.", source);
                    return;
                }
                if world_num != self.world_num {
                    return;
                }
                if let Some(chunk_data) = chunk_data {
                    self.chunk_storage.insert(chunk, chunk_data);
                }
            }
            WorldNetMessage::RelinquishAuthority {
                chunk,
                chunk_data,
                world_num,
            } => {
                if !self.is_host {
                    warn!("{} sent RelinquishAuthority to not-host.", source);
                    return;
                }
                if world_num != self.world_num {
                    return;
                }
                if let Some(state) = self.authority_map.get(&chunk) {
                    if state.0 != source {
                        warn!("{source} sent RelinquishAuthority for {chunk:?}, but isn't currently an authority");
                        return;
                    }
                }
                self.authority_map.remove(&chunk);
                if let Some(chunk_data) = chunk_data {
                    self.chunk_storage.insert(chunk, chunk_data);
                }
                self.emit_msg(
                    Destination::Broadcast,
                    WorldNetMessage::ListenAuthorityRelinquished { chunk },
                )
            }
            WorldNetMessage::UnloadChunk { chunk } => {
                self.chunk_state.insert(chunk, ChunkState::UnloadPending {});
            }

            WorldNetMessage::AuthorityAlreadyTaken { chunk, authority } => {
                self.emit_msg(
                    Destination::Peer(authority),
                    WorldNetMessage::ListenRequest { chunk },
                );
                self.last_request_priority.remove(&chunk);
            }
            WorldNetMessage::ListenRequest { chunk } => {
                let Some(ChunkState::Authority {
                    listeners,
                    priority,
                    ..
                }) = self.chunk_state.get_mut(&chunk)
                else {
                    self.emit_msg(
                        Destination::Peer(source),
                        WorldNetMessage::UnloadChunk { chunk },
                    );
                    //warn!("Can't listen for {chunk:?} - not an authority");
                    return;
                };
                listeners.insert(source);
                let chunk_data = self.outbound_model.get_chunk_data(chunk);
                let priority = *priority;
                self.emit_msg(
                    Destination::Peer(source),
                    WorldNetMessage::ListenInitialResponse {
                        chunk,
                        chunk_data,
                        priority,
                    },
                );
            }
            WorldNetMessage::ListenStopRequest { chunk } => {
                let Some(ChunkState::Authority { listeners, .. }) =
                    self.chunk_state.get_mut(&chunk)
                else {
                    //warn!("Can't stop listen for {chunk:?} - not an authority");
                    return;
                };
                listeners.remove(&source);
            }
            WorldNetMessage::ListenInitialResponse {
                chunk,
                chunk_data,
                priority,
            } => {
                self.chunk_state.insert(
                    chunk,
                    ChunkState::Listening {
                        authority: source,
                        priority,
                    },
                );
                if let Some(chunk_data) = chunk_data {
                    self.inbound_model.apply_chunk_data(chunk, &chunk_data);
                } else {
                    warn!("Initial listen response has None chunk_data. It's generally supposed to have some.");
                }
            }
            WorldNetMessage::ListenUpdate {
                delta,
                priority,
                take_auth,
            } => {
                match self.chunk_state.get_mut(&delta.chunk_coord) {
                    Some(ChunkState::Listening { priority: pri, .. }) => {
                        *pri = priority;
                        if take_auth {
                            self.emit_msg(
                                Destination::Peer(source),
                                WorldNetMessage::LoseAuthority {
                                    chunk: delta.chunk_coord,
                                    new_priority: priority,
                                    new_authority: source,
                                },
                            );
                        }
                    }
                    Some(ChunkState::WantToGetAuth {
                        authority,
                        my_priority,
                        ..
                    }) => {
                        if priority > *my_priority {
                            if take_auth {
                                let rq = WorldNetMessage::RequestAuthority {
                                    chunk: delta.chunk_coord,
                                    priority: *my_priority,
                                    can_wait: false,
                                };
                                self.emit_msg(Destination::Host, rq);
                                self.chunk_state
                                    .insert(delta.chunk_coord, ChunkState::WaitingForAuthority);
                            }
                        } else {
                            let cs = ChunkState::Listening {
                                authority: *authority,
                                priority,
                            };
                            self.chunk_state.insert(delta.chunk_coord, cs);
                        }
                    }
                    _ if take_auth => {
                        self.emit_msg(
                            Destination::Peer(source),
                            WorldNetMessage::LoseAuthority {
                                chunk: delta.chunk_coord,
                                new_priority: priority,
                                new_authority: source,
                            },
                        );
                    }
                    _ => return,
                }
                self.inbound_model.apply_chunk_delta(&delta);
            }
            WorldNetMessage::ChunkPacket { chunkpacket } => {
                for (delta, priority) in chunkpacket {
                    match self.chunk_state.get_mut(&delta.chunk_coord) {
                        Some(ChunkState::Listening { priority: pri, .. }) => {
                            *pri = priority;
                        }
                        Some(ChunkState::WantToGetAuth {
                            authority,
                            my_priority,
                            ..
                        }) => {
                            if priority <= *my_priority {
                                let cs = ChunkState::Listening {
                                    authority: *authority,
                                    priority,
                                };
                                self.chunk_state.insert(delta.chunk_coord, cs);
                            }
                        }
                        _ => continue,
                    }
                    self.inbound_model.apply_chunk_delta(&delta);
                }
            }
            WorldNetMessage::ListenAuthorityRelinquished { chunk } => {
                self.chunk_state.insert(chunk, ChunkState::UnloadPending);
            }
            WorldNetMessage::GetAuthorityFrom {
                chunk,
                current_authority,
            } => {
                if self.chunk_state.get(&chunk) != Some(&ChunkState::UnloadPending) {
                    debug!("Will request authority transfer");
                    self.chunk_state.insert(chunk, ChunkState::Transfer);
                    self.emit_msg(
                        Destination::Peer(current_authority),
                        WorldNetMessage::RequestAuthorityTransfer { chunk },
                    );
                } else {
                    self.emit_msg(
                        Destination::Host,
                        WorldNetMessage::RelinquishAuthority {
                            chunk,
                            chunk_data: None,
                            world_num: self.world_num,
                        },
                    );
                }
            }
            WorldNetMessage::RequestAuthorityTransfer { chunk } => {
                debug!("Got a request for authority transfer");
                let state = self.chunk_state.get(&chunk);
                if let Some(ChunkState::Authority { listeners, .. }) = state {
                    let chunk_data = self.outbound_model.get_chunk_data(chunk);
                    self.emit_msg(
                        Destination::Peer(source),
                        WorldNetMessage::TransferOk {
                            chunk,
                            chunk_data,
                            listeners: listeners.clone(),
                        },
                    );
                    self.chunk_state.insert(chunk, ChunkState::UnloadPending);
                    let chunk_data = self.outbound_model.get_chunk_data(chunk);
                    self.emit_msg(
                        Destination::Host,
                        WorldNetMessage::UpdateStorage {
                            chunk,
                            chunk_data,
                            world_num: self.world_num,
                        },
                    );
                } else {
                    self.emit_msg(
                        Destination::Peer(source),
                        WorldNetMessage::TransferFailed { chunk },
                    );
                }
            }
            WorldNetMessage::TransferOk {
                chunk,
                chunk_data,
                listeners,
            } => {
                debug!("Transfer ok");
                if let Some(chunk_data) = chunk_data {
                    self.inbound_model.apply_chunk_data(chunk, &chunk_data);
                    self.outbound_model.apply_chunk_data(chunk, &chunk_data);
                }
                for listener in listeners.iter() {
                    self.emit_msg(
                        Destination::Peer(*listener),
                        WorldNetMessage::NotifyNewAuthority { chunk },
                    );
                }
                self.chunk_state.insert(
                    chunk,
                    ChunkState::Authority {
                        listeners,
                        priority: self.last_request_priority.remove(&chunk).unwrap_or(0),
                        new_authority: None,
                        stop_sending: false,
                    },
                );
            }
            WorldNetMessage::TransferFailed { chunk } => {
                warn!("Transfer failed, requesting authority normally");
                let priority = self
                    .last_request_priority
                    .get(&chunk)
                    .copied()
                    .unwrap_or(255);
                self.chunk_state.insert(
                    chunk,
                    ChunkState::RequestAuthority {
                        priority,
                        can_wait: true,
                    },
                );
            }
            WorldNetMessage::NotifyNewAuthority { chunk } => {
                debug!("Notified of new authority");
                let state = self.chunk_state.get_mut(&chunk);
                if let Some(ChunkState::Listening { authority, .. }) = state {
                    *authority = source;
                } else {
                    debug!("Got notified of new authority, but not a listener");
                }
            }
        }
    }

    /// Should be called when player disconnects.
    /// This frees up any authority that player had.
    pub(crate) fn handle_peer_left(&mut self, source: OmniPeerId) {
        if !self.is_host {
            return;
        }
        let mut pending_messages = Vec::new();

        for (&chunk, peer) in self.authority_map.iter() {
            if peer.0 == source {
                info!("Removing authority from disconnected peer: {chunk:?}");
                pending_messages.push(WorldNetMessage::ListenAuthorityRelinquished { chunk });
            }
        }
        self.authority_map.retain(|_, peer| peer.0 != source);

        for message in pending_messages {
            self.emit_msg(Destination::Broadcast, message)
        }
    }

    pub(crate) fn get_debug_markers(&self) -> Vec<DebugMarker> {
        self.chunk_state
            .iter()
            .map(|(&chunk, state)| {
                let message = match state {
                    ChunkState::RequestAuthority { .. } => "req auth",
                    ChunkState::WaitingForAuthority => "wai auth",
                    ChunkState::Listening { .. } => "list",
                    ChunkState::Authority { .. } => "auth",
                    ChunkState::UnloadPending => "unl",
                    ChunkState::Transfer => "tran",
                    ChunkState::WantToGetAuth { .. } => "want auth",
                };
                let mut priority = String::new();
                if let Some(n) = self.authority_map.get(&chunk).copied() {
                    priority = n.1.to_string()
                }
                DebugMarker {
                    x: (chunk.0 * 128) as f64,
                    y: (chunk.1 * 128) as f64,
                    message: message.to_owned() + &priority,
                }
            })
            .collect()
    }

    pub(crate) fn cut_through_world(&mut self, x: i32, y_min: i32, y_max: i32, radius: i32) {
        let max_wiggle = 5;
        let interval = 300.0;

        let cut_x_clip_range =
            (x - radius - max_wiggle - CHUNK_SIZE as i32)..(x + radius + max_wiggle);
        let cut_x_range = x - radius..x + radius;

        let air_pixel = Pixel {
            flags: world_model::chunk::PixelFlags::Normal,
            material: 0,
        };
        for (chunk_coord, chunk_encoded) in self.chunk_storage.iter_mut() {
            // Check if this chunk is anywhere close to the cut. Skip if it isn't.
            let chunk_start_x = chunk_coord.0 * CHUNK_SIZE as i32;
            let chunk_start_y = chunk_coord.1 * CHUNK_SIZE as i32;
            if !cut_x_clip_range.contains(&chunk_start_x) {
                continue;
            }

            let mut chunk = Chunk::default();
            chunk_encoded.apply_to_chunk(&mut chunk);

            for in_chunk_y in 0..(CHUNK_SIZE as i32) {
                let global_y = in_chunk_y + chunk_start_y;
                // Skip if higher/lower than the cut.
                if global_y < y_min || global_y > y_max {
                    continue;
                }

                let wiggle = -f32::cos((global_y as f32) / interval * TAU) * max_wiggle as f32;
                let wiggle = wiggle as i32; // TODO find a more accurate way to compute wiggle.

                let in_chunk_x_range = cut_x_range.start - chunk_start_x + wiggle
                    ..cut_x_range.end - chunk_start_x + wiggle;
                let in_chunk_x_range = in_chunk_x_range.start.clamp(0, CHUNK_SIZE as i32 - 1)
                    ..in_chunk_x_range.end.clamp(0, CHUNK_SIZE as i32);

                for in_chunk_x in in_chunk_x_range {
                    chunk.set_pixel(
                        (in_chunk_y as usize) * CHUNK_SIZE + (in_chunk_x as usize),
                        air_pixel,
                    );
                }
            }

            *chunk_encoded = chunk.to_chunk_data();
        }
    }

    pub(crate) fn cut_through_world_line(&mut self, x: i32, y: i32, lx: i32, ly: i32, r: i32) {
        if !self.is_host && !self.nice_terraforming {
            return;
        }
        let (min_cx, max_cx) = if x < lx {
            (
                (x - r).div_euclid(CHUNK_SIZE as i32),
                (lx + r).div_euclid(CHUNK_SIZE as i32),
            )
        } else {
            (
                (lx - r).div_euclid(CHUNK_SIZE as i32),
                (x + r).div_euclid(CHUNK_SIZE as i32),
            )
        };
        let (min_cy, max_cy) = if y < ly {
            (
                (y - r).div_euclid(CHUNK_SIZE as i32),
                (ly + r).div_euclid(CHUNK_SIZE as i32),
            )
        } else {
            (
                (ly - r).div_euclid(CHUNK_SIZE as i32),
                (y + r).div_euclid(CHUNK_SIZE as i32),
            )
        };

        let dmx = lx - x;
        let dmy = ly - y;
        if dmx == 0 && dmy == 0 {
            self.cut_through_world_circle(x, y, r, None);
            return;
        }
        let dm2 = ((dmx * dmx + dmy * dmy) as f64).recip();
        let air_pixel = Pixel {
            flags: world_model::chunk::PixelFlags::Normal,
            material: 0,
        };
        let close_check = max_cx == min_cx || max_cy == min_cy;
        let iter_check = [
            (x + r, y),
            (x - r, y),
            (x, y + r),
            (x, y - r),
            (lx + r, ly),
            (lx - r, ly),
            (lx, ly + r),
            (lx, ly - r),
        ]
        .into_iter();
        for chunk_x in min_cx..=max_cx {
            for chunk_y in min_cy..=max_cy {
                let chunk_start_x = chunk_x * CHUNK_SIZE as i32;
                let chunk_start_y = chunk_y * CHUNK_SIZE as i32;
                if close_check
                    || [
                        (chunk_start_x, chunk_start_y),
                        (
                            chunk_start_x + CHUNK_SIZE as i32 - 1,
                            chunk_start_y + CHUNK_SIZE as i32 - 1,
                        ),
                        (chunk_start_x + CHUNK_SIZE as i32 - 1, chunk_start_y),
                        (chunk_start_x, chunk_start_y + CHUNK_SIZE as i32 - 1),
                    ]
                    .iter()
                    .any(|(cx, cy)| {
                        let dcx = cx - x;
                        let dcy = cy - y;
                        let m = ((dcx * dmx + dcy * dmy) as f64 * dm2).clamp(0.0, 1.0);
                        let dx = dcx - (m * dmx as f64) as i32;
                        let dy = dcy - (m * dmy as f64) as i32;
                        dx * dx + dy * dy <= r * r
                    })
                    || {
                        let (end_x, end_y) = (
                            chunk_start_x + CHUNK_SIZE as i32 - 1,
                            chunk_start_y + CHUNK_SIZE as i32 - 1,
                        );
                        iter_check.clone().any(|(x, y)| {
                            end_x >= x && x >= chunk_start_x && end_y >= y && y >= chunk_start_y
                        })
                    }
                {
                    let mut chunk = Chunk::default();
                    let mut chunkin = Chunk::default();
                    let mut chunkout = Chunk::default();
                    let coord = ChunkCoord(chunk_x, chunk_y);
                    if let Some(chunk_encoded) = self.chunk_storage.get(&coord) {
                        chunk_encoded.apply_to_chunk(&mut chunk)
                    } else if !self.nice_terraforming {
                        continue;
                    }
                    let mut has_in = false;
                    if self.nice_terraforming {
                        if let Some(chunk_encoded) = self.inbound_model.get_chunk_data(coord) {
                            has_in = true;
                            chunk_encoded.apply_to_chunk(&mut chunkin)
                        };
                    }
                    let mut has_out = false;
                    if self.nice_terraforming {
                        if let Some(chunk_encoded) = self.outbound_model.get_chunk_data(coord) {
                            has_out = true;
                            chunk_encoded.apply_to_chunk(&mut chunkout)
                        }
                    }
                    for icx in 0..CHUNK_SIZE as i32 {
                        let cx = chunk_start_x + icx;
                        let dcx = cx - x;
                        let dx2 = dcx * dmx;
                        for icy in 0..CHUNK_SIZE as i32 {
                            let cy = chunk_start_y + icy;
                            let dcy = cy - y;
                            let m = ((dx2 + dcy * dmy) as f64 * dm2).clamp(0.0, 1.0);
                            let dx = dcx - (m * dmx as f64) as i32;
                            let dy = dcy - (m * dmy as f64) as i32;
                            if dx * dx + dy * dy <= r * r {
                                let px = icy as usize * CHUNK_SIZE + icx as usize;
                                if self.is_host {
                                    chunk.set_pixel(px, air_pixel);
                                }
                                if has_in {
                                    chunkin.set_pixel(px, air_pixel);
                                }
                                if has_out {
                                    chunkout.set_pixel(px, air_pixel);
                                }
                            }
                        }
                    }
                    if self.is_host {
                        self.chunk_storage.insert(coord, chunk.to_chunk_data());
                    }
                    if has_in {
                        self.inbound_model
                            .apply_chunk_data(coord, &chunkin.to_chunk_data())
                    } else if has_out {
                        self.inbound_model
                            .apply_chunk_data(coord, &chunkout.to_chunk_data())
                    }
                    if has_out {
                        self.outbound_model
                            .apply_chunk_data(coord, &chunkout.to_chunk_data())
                    }
                }
            }
        }
    }
    pub(crate) fn cut_through_world_circle(&mut self, x: i32, y: i32, r: i32, mat: Option<u16>) {
        if !self.is_host && !self.nice_terraforming {
            return;
        }
        let (min_cx, max_cx) = (
            (x - r).div_euclid(CHUNK_SIZE as i32),
            (x + r).div_euclid(CHUNK_SIZE as i32),
        );
        let (min_cy, max_cy) = (
            (y - r).div_euclid(CHUNK_SIZE as i32),
            (y + r).div_euclid(CHUNK_SIZE as i32),
        );
        let air_pixel = Pixel {
            flags: world_model::chunk::PixelFlags::Normal,
            material: mat.unwrap_or(0),
        };
        let (chunkx, chunky) = (
            x.div_euclid(CHUNK_SIZE as i32),
            y.div_euclid(CHUNK_SIZE as i32),
        );
        let do_continue = mat.unwrap_or(0) != 0;
        for chunk_x in min_cx..=max_cx {
            for chunk_y in min_cy..=max_cy {
                if r <= CHUNK_SIZE as i32 || {
                    let close_x = if chunk_x < chunkx {
                        (chunk_x + 1) * CHUNK_SIZE as i32 - 1
                    } else {
                        chunk_x * CHUNK_SIZE as i32
                    };
                    let close_y = if chunk_y < chunky {
                        (chunk_y + 1) * CHUNK_SIZE as i32 - 1
                    } else {
                        chunk_y * CHUNK_SIZE as i32
                    };
                    let dx = close_x - x;
                    let dy = close_y - y;
                    dx * dx + dy * dy <= r * r
                } {
                    let coord = ChunkCoord(chunk_x, chunk_y);
                    let chunk_start_x = chunk_x * CHUNK_SIZE as i32;
                    let chunk_start_y = chunk_y * CHUNK_SIZE as i32;
                    let mut chunk = Chunk::default();
                    let mut chunkin = Chunk::default();
                    let mut chunkout = Chunk::default();
                    if let Some(chunk_encoded) = self.chunk_storage.get(&coord) {
                        chunk_encoded.apply_to_chunk(&mut chunk)
                    } else if do_continue || !self.nice_terraforming {
                        continue;
                    }
                    let mut has_in = false;
                    if self.nice_terraforming {
                        if let Some(chunk_encoded) = self.inbound_model.get_chunk_data(coord) {
                            has_in = true;
                            chunk_encoded.apply_to_chunk(&mut chunkin)
                        };
                    }
                    let mut has_out = false;
                    if self.nice_terraforming {
                        if let Some(chunk_encoded) = self.outbound_model.get_chunk_data(coord) {
                            has_out = true;
                            chunk_encoded.apply_to_chunk(&mut chunkout)
                        }
                    }
                    for icx in 0..CHUNK_SIZE as i32 {
                        let cx = chunk_start_x + icx;
                        let dx = cx - x;
                        let dd = dx * dx;
                        for icy in 0..CHUNK_SIZE as i32 {
                            let cy = chunk_start_y + icy;
                            let dy = cy - y;
                            if dd + dy * dy <= r * r {
                                let px = icy as usize * CHUNK_SIZE + icx as usize;
                                if chunk.pixel(px).material != 0 {
                                    chunk.set_pixel(px, air_pixel);
                                    if has_in {
                                        chunkin.set_pixel(px, air_pixel);
                                    }
                                    if has_out {
                                        chunkout.set_pixel(px, air_pixel);
                                    }
                                }
                            }
                        }
                    }
                    if self.is_host {
                        self.chunk_storage.insert(coord, chunk.to_chunk_data());
                    }
                    if has_in {
                        self.inbound_model
                            .apply_chunk_data(coord, &chunkin.to_chunk_data())
                    } else if has_out {
                        self.inbound_model
                            .apply_chunk_data(coord, &chunkout.to_chunk_data())
                    }
                    if has_out {
                        self.outbound_model
                            .apply_chunk_data(coord, &chunkout.to_chunk_data())
                    }
                }
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn do_ray(
        &self,
        mut x: i32,
        mut y: i32,
        end_x: i32,
        end_y: i32,
        mut ray: u32,
        d: u8,
        mult: f32,
    ) -> Option<(i32, i32)> {
        //Bresenham's line algorithm
        let dx = (end_x - x).abs();
        let dy = (end_y - y).abs();
        if dx == 0 && dy == 0 {
            return None;
        }
        let sx = if x < end_x { 1 } else { -1 };
        let sy = if y < end_y { 1 } else { -1 };
        let mut err = if dx > dy { dx } else { -dy } / 2;
        let mut e2;
        let mut working_chunk = Chunk::default();
        let mut last_co = ChunkCoord(
            x.div_euclid(CHUNK_SIZE as i32),
            y.div_euclid(CHUNK_SIZE as i32),
        );
        let mut last;
        if let Some(c) = self.outbound_model.get_chunk_data(last_co) {
            last = c
        } else if let Some(c) = self.inbound_model.get_chunk_data(last_co) {
            last = c
        } else if let Some(c) = self.chunk_storage.get(&last_co).cloned() {
            last = c
        } else {
            return None;
        };
        last.apply_to_chunk(&mut working_chunk);
        let mut last_coord = None;
        while x != end_x || y != end_y {
            let co = ChunkCoord(
                x.div_euclid(CHUNK_SIZE as i32),
                y.div_euclid(CHUNK_SIZE as i32),
            );
            if co != last_co {
                if let Some(c) = self.outbound_model.get_chunk_data(co) {
                    last = c
                } else if let Some(c) = self.outbound_model.get_chunk_data(co) {
                    last = c
                } else if let Some(c) = self.chunk_storage.get(&co).cloned() {
                    last = c
                } else {
                    return last_coord;
                };
                last.apply_to_chunk(&mut working_chunk);
                last_co = co;
            }

            let icx = x.rem_euclid(CHUNK_SIZE as i32);
            let icy = y.rem_euclid(CHUNK_SIZE as i32);
            let px = icy as usize * CHUNK_SIZE + icx as usize;
            let pixel = working_chunk.pixel(px);
            if let Some(stats) = self.durabilities.get(&pixel.material) {
                let h = (stats.1 as f32 * mult) as u32;
                if stats.0 > d || ray < h {
                    return last_coord;
                }
                ray = ray.saturating_sub(h);
            }

            last_coord = Some((x, y));
            e2 = err;
            if e2 > -dx {
                err -= dy;
                x += sx;
            }
            if e2 < dy {
                err += dx;
                y += sy;
            }
        }
        Some((x, y))
    }
    pub(crate) fn cut_through_world_explosion(&mut self, x: i32, y: i32, r: u32, d: u8, ray: u32) {
        let rays = r.next_power_of_two().clamp(8, 256);
        let t = TAU / rays as f32;
        let results: Vec<i32> = (0..rays)
            .into_par_iter()
            .map(|n| {
                let theta = t * (n as f32 + 0.5);
                let end_x = x + (r as f32 * theta.cos()) as i32;
                let end_y = y + (r as f32 * theta.sin()) as i32;
                let mult = (((theta + TAU / 8.0) % (TAU / 4.0)) - TAU / 8.0)
                    .cos()
                    .recip();
                if let Some((ex, ey)) = self.do_ray(x, y, end_x, end_y, ray, d, mult) {
                    let dx = ex - x;
                    let dy = ey - y;
                    if dx != 0 || dy != 0 {
                        dx * dx + dy * dy
                    } else {
                        0
                    }
                } else {
                    0
                }
            })
            .collect();
        self.cut_through_world_explosion_list(x, y, rays, results);
    }
    pub(crate) fn cut_through_world_explosion_list(
        &mut self,
        x: i32,
        y: i32,
        rays: u32,
        list: Vec<i32>,
    ) {
        let rs = *list.iter().max().unwrap_or(&0);
        let r = (rs as f64).sqrt().ceil() as i32;
        if r == 0 {
            return;
        }
        let (min_cx, max_cx) = (
            (x - r).div_euclid(CHUNK_SIZE as i32),
            (x + r).div_euclid(CHUNK_SIZE as i32),
        );
        let (min_cy, max_cy) = (
            (y - r).div_euclid(CHUNK_SIZE as i32),
            (y + r).div_euclid(CHUNK_SIZE as i32),
        );
        let air_pixel = Pixel {
            flags: world_model::chunk::PixelFlags::Normal,
            material: 0,
        };
        let (chunkx, chunky) = (
            x.div_euclid(CHUNK_SIZE as i32),
            y.div_euclid(CHUNK_SIZE as i32),
        );
        for chunk_x in min_cx..=max_cx {
            for chunk_y in min_cy..=max_cy {
                if r <= CHUNK_SIZE as i32 || {
                    if r >= 8 * CHUNK_SIZE as i32 {
                        let close_x = if chunk_x < chunkx {
                            (chunk_x + 1) * CHUNK_SIZE as i32 - 1
                        } else {
                            chunk_x * CHUNK_SIZE as i32
                        };
                        let close_y = if chunk_y < chunky {
                            (chunk_y + 1) * CHUNK_SIZE as i32 - 1
                        } else {
                            chunk_y * CHUNK_SIZE as i32
                        };
                        let (adj_x1, adj_x2) = (
                            chunk_x * CHUNK_SIZE as i32,
                            (chunk_x + 1) * CHUNK_SIZE as i32 - 1,
                        );
                        let (adj_y1, adj_y2) = if (chunk_x < chunkx) == (chunk_y < chunky) {
                            (
                                (chunk_y + 1) * CHUNK_SIZE as i32 - 1,
                                chunk_y * CHUNK_SIZE as i32,
                            )
                        } else {
                            (
                                chunk_y * CHUNK_SIZE as i32,
                                (chunk_y + 1) * CHUNK_SIZE as i32 - 1,
                            )
                        };
                        let dx = close_x - x;
                        let dy = close_y - y;
                        let adj_dx = adj_x1 - x;
                        let adj_dy = adj_y1 - y;
                        let mut i = rays as f32 * (adj_dy as f32).atan2(adj_dx as f32) / TAU;
                        if i.is_sign_negative() {
                            i += rays as f32
                        }
                        let adj_dx = adj_x2 - x;
                        let adj_dy = adj_y2 - y;
                        let mut j = rays as f32 * (adj_dy as f32).atan2(adj_dx as f32) / TAU;
                        if j.is_sign_negative() {
                            j += rays as f32
                        }
                        let i = i as usize;
                        let j = j as usize;
                        let r = list[i.min(j)..=i.max(j)].iter().max().unwrap_or(&0);
                        dx * dx + dy * dy <= *r
                    } else {
                        let close_x = if chunk_x < chunkx {
                            (chunk_x + 1) * CHUNK_SIZE as i32 - 1
                        } else {
                            chunk_x * CHUNK_SIZE as i32
                        };
                        let close_y = if chunk_y < chunky {
                            (chunk_y + 1) * CHUNK_SIZE as i32 - 1
                        } else {
                            chunk_y * CHUNK_SIZE as i32
                        };
                        let dx = close_x - x;
                        let dy = close_y - y;
                        dx * dx + dy * dy <= rs
                    }
                } {
                    let mut chunk = Chunk::default();
                    let coord = ChunkCoord(chunk_x, chunk_y);
                    if self.outbound_model.get_chunk_data(coord).is_some()
                        || self.inbound_model.get_chunk_data(coord).is_some()
                    {
                        continue;
                    } else if let Some(chunk_encoded) = self.chunk_storage.get(&coord) {
                        chunk_encoded.apply_to_chunk(&mut chunk);
                    } else {
                        continue;
                    }
                    let chunk_start_x = chunk_x * CHUNK_SIZE as i32;
                    let chunk_start_y = chunk_y * CHUNK_SIZE as i32;
                    for icx in 0..CHUNK_SIZE as i32 {
                        let cx = chunk_start_x + icx;
                        let dx = cx - x;
                        let dd = dx * dx;
                        for icy in 0..CHUNK_SIZE as i32 {
                            let cy = chunk_start_y + icy;
                            let dy = cy - y;
                            let mut i = rays as f32 * (dy as f32).atan2(dx as f32) / TAU;
                            if i.is_sign_negative() {
                                i += rays as f32
                            }
                            if dd + dy * dy <= list[i as usize] {
                                let px = icy as usize * CHUNK_SIZE + icx as usize;
                                chunk.set_pixel(px, air_pixel);
                            }
                        }
                    }
                    self.chunk_storage.insert(coord, chunk.to_chunk_data());
                }
            }
        }
    }
}
impl Drop for WorldManager {
    fn drop(&mut self) {
        if self.is_host {
            self.save_state.save(&self.chunk_storage);
            info!("Saved chunk data");
        }
    }
}

impl SaveStateEntry for FxHashMap<ChunkCoord, ChunkData> {
    const FILENAME: &'static str = "world_chunks";
}

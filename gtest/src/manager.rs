// This file is part of Gear.

// Copyright (C) 2021-2022 Gear Technologies Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    log::{CoreLog, RunResult},
    program::WasmProgram,
};
use core_processor::{common::*, configs::BlockInfo, Ext};
use gear_backend_wasmtime::WasmtimeEnvironment;
use gear_core::{
    memory::PageNumber,
    message::{Dispatch, DispatchKind, Message, MessageId},
    program::{CodeHash, Program as CoreProgram, ProgramId},
};
use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) type Balance = u128;

#[derive(Debug)]
pub(crate) enum Actor {
    Initialized(Program),
    // Contract: program is always `Some`, option is used to take ownership
    Uninitialized(Option<MessageId>, Option<Program>),
    Dormant,
}

impl Actor {
    pub(crate) fn new(prog: Program) -> Self {
        Actor::Uninitialized(None, Some(prog))
    }

    // # Panics
    // If actor is initialized or dormant
    fn set_initialized(&mut self) {
        assert!(
            self.is_uninitialized(),
            "can't transmute actor, which isn't uninitialized"
        );

        if let Actor::Uninitialized(_, maybe_prog) = self {
            let mut prog = maybe_prog
                .take()
                .expect("actor storage contains only `Some` values by contract");
            if let Program::Genuine(p) = &mut prog {
                p.set_initialized();
            }
            *self = Actor::Initialized(prog);
        }
    }

    fn is_dormant(&self) -> bool {
        matches!(self, Actor::Dormant)
    }

    fn is_uninitialized(&self) -> bool {
        matches!(self, Actor::Uninitialized(..))
    }

    fn as_mut_core_prog(&mut self) -> Option<&mut CoreProgram> {
        match self {
            Actor::Initialized(Program::Genuine(prog)) => Some(prog),
            _ => None,
        }
    }

    // Takes ownership over mock program, putting `None` value instead of it.
    fn take_mock(&mut self) -> Option<Box<dyn WasmProgram>> {
        match self {
            Actor::Initialized(Program::Mock(mock)) => mock.take(),
            Actor::Uninitialized(_, Some(Program::Mock(mock))) => mock.take(),
            _ => None,
        }
    }

    // Gets a new executable actor derived from the inner program.
    fn get_executable_actor(&self, balance: Balance) -> Option<ExecutableActor> {
        let program = match self {
            Actor::Initialized(Program::Genuine(program)) => Some(program.clone()),
            Actor::Uninitialized(_, Some(Program::Genuine(program))) => Some(program.clone()),
            _ => None,
        };
        program.map(|program| ExecutableActor { program, balance })
    }
}

#[derive(Debug)]
pub(crate) enum Program {
    Genuine(CoreProgram),
    // Contract: is always `Some`, option is used to take ownership
    Mock(Option<Box<dyn WasmProgram>>),
}

impl Program {
    pub(crate) fn new(prog: CoreProgram) -> Self {
        Program::Genuine(prog)
    }

    pub(crate) fn new_mock(mock: impl WasmProgram + 'static) -> Self {
        Program::Mock(Some(Box::new(mock)))
    }
}

#[derive(Default, Debug)]
pub(crate) struct ExtManager {
    // State metadata
    pub(crate) block_info: BlockInfo,

    // Messaging and programs meta
    pub(crate) msg_nonce: u64,
    pub(crate) id_nonce: u64,

    // State
    pub(crate) actors: BTreeMap<ProgramId, (Actor, Balance)>,
    pub(crate) dispatch_queue: VecDeque<Dispatch>,
    pub(crate) mailbox: BTreeMap<ProgramId, Vec<Message>>,
    pub(crate) wait_list: BTreeMap<(ProgramId, MessageId), Dispatch>,
    pub(crate) wait_init_list: BTreeMap<ProgramId, Vec<MessageId>>,

    // Last run info
    pub(crate) origin: ProgramId,
    pub(crate) msg_id: MessageId,
    pub(crate) log: Vec<Message>,
    pub(crate) main_failed: bool,
    pub(crate) others_failed: bool,
}

impl ExtManager {
    pub(crate) fn new() -> Self {
        Self {
            msg_nonce: 1,
            id_nonce: 1,
            block_info: BlockInfo {
                height: 0,
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_secs(),
            },
            ..Default::default()
        }
    }

    pub(crate) fn fetch_inc_message_nonce(&mut self) -> u64 {
        let nonce = self.msg_nonce;
        self.msg_nonce += 1;
        nonce
    }

    pub(crate) fn free_id_nonce(&mut self) -> u64 {
        while self.actors.contains_key(&self.id_nonce.into()) {
            self.id_nonce += 1;
        }
        self.id_nonce
    }

    fn get_entry_point(message: &Message, actor: &Actor) -> DispatchKind {
        message
            .reply()
            .map(|_| DispatchKind::HandleReply)
            .unwrap_or_else(|| {
                // Init message id should be set before dispatches addressed to the actor are stored.
                assert!(
                    !matches!(actor, Actor::Uninitialized(None, _)),
                    "internal error: uninitialized actor hasn't got init msg id"
                );
                if matches!(actor, Actor::Uninitialized(Some(id), _) if *id == message.id()) {
                    DispatchKind::Init
                } else {
                    DispatchKind::Handle
                }
            })
    }

    pub(crate) fn run_message(&mut self, message: Message) -> RunResult {
        self.prepare_for(message.id(), message.source());

        {
            let maybe_actor = self.actors.get_mut(&message.dest());
            if let Some((actor, _)) = maybe_actor {
                let kind = if actor.is_dormant() {
                    DispatchKind::Handle
                } else {
                    if let Actor::Uninitialized(maybe_message_id, _) = actor {
                        maybe_message_id.get_or_insert(message.id());
                    }
                    Self::get_entry_point(&message, &*actor)
                };

                let dispatch = Dispatch {
                    kind,
                    message,
                    payload_store: None,
                };
                self.dispatch_queue.push_back(dispatch);
            } else {
                self.mailbox
                    .entry(message.dest())
                    .or_default()
                    .push(message);
            }
        }

        while let Some(dispatch) = self.dispatch_queue.pop_front() {
            let message_id = dispatch.message.id();
            let dest = dispatch.message.dest();

            if self.check_is_for_wait_list(&dispatch) {
                self.wait_init_list
                    .entry(dest)
                    .or_default()
                    .push(message_id);
                self.wait_dispatch(dispatch);

                continue;
            }

            let (actor, balance) = self
                .actors
                .get_mut(&dest)
                .expect("Somehow message queue contains message for user");

            if let Some(executable_actor) = actor.get_executable_actor(*balance) {
                self.process_normal(executable_actor, dispatch);
            } else if let Some(mock) = actor.take_mock() {
                self.process_mock(mock, dispatch);
            } else {
                self.process_dormant(dispatch);
            }
        }

        let log = self.log.clone();

        RunResult {
            main_failed: self.main_failed,
            others_failed: self.others_failed,
            log: log.into_iter().map(CoreLog::from_message).collect(),
        }
    }

    fn prepare_for(&mut self, msg_id: MessageId, origin: ProgramId) {
        self.msg_id = msg_id;
        self.origin = origin;
        self.log.clear();
        self.main_failed = false;
        self.others_failed = false;

        if !self.dispatch_queue.is_empty() {
            panic!("Message queue isn't empty");
        }
    }

    fn mark_failed(&mut self, msg_id: MessageId) {
        if self.msg_id == msg_id {
            self.main_failed = true;
        } else {
            self.others_failed = true;
        }
    }

    fn init_success(&mut self, message_id: MessageId, program_id: ProgramId) {
        let (actor, _) = self
            .actors
            .get_mut(&program_id)
            .expect("Can't find existing program");

        actor.set_initialized();

        self.move_waiting_msgs_to_queue(message_id, program_id);
    }

    fn init_failure(&mut self, message_id: MessageId, program_id: ProgramId) {
        let (actor, _) = self
            .actors
            .get_mut(&program_id)
            .expect("Can't find existing program");

        *actor = Actor::Dormant;

        self.move_waiting_msgs_to_queue(message_id, program_id);
        self.mark_failed(message_id);
    }

    fn move_waiting_msgs_to_queue(&mut self, message_id: MessageId, program_id: ProgramId) {
        if let Some(ids) = self.wait_init_list.remove(&program_id) {
            for id in ids {
                self.wake_message(message_id, program_id, id);
            }
        }
    }

    // When called for the `dispatch`, it must be in queue.
    fn check_is_for_wait_list(&self, dispatch: &Dispatch) -> bool {
        let Dispatch { message, .. } = dispatch;

        let (actor, _) = self
            .actors
            .get(&message.dest())
            .expect("method called for unknown destination");
        if let Actor::Uninitialized(maybe_message_id, _) = actor {
            let id = maybe_message_id.expect("message in dispatch queue has id");
            message.reply().is_none() && id != message.id()
        } else {
            false
        }
    }

    fn process_mock(&mut self, mut mock: Box<dyn WasmProgram>, dispatch: Dispatch) {
        let Dispatch { message, kind, .. } = dispatch;

        let message_id = message.id();
        let program_id = message.dest();
        let payload = message.payload().to_vec();

        let response = match kind {
            DispatchKind::Init => mock.init(payload),
            DispatchKind::Handle => mock.handle(payload),
            DispatchKind::HandleReply => mock.handle_reply(payload),
        };

        match response {
            Ok(reply) => {
                if let DispatchKind::Init = kind {
                    self.message_dispatched(DispatchOutcome::InitSuccess {
                        message_id,
                        program_id,
                        origin: message.source(),
                    });
                }

                if let Some(payload) = reply {
                    let nonce = self.fetch_inc_message_nonce();

                    let reply_message = Message::new_reply(
                        nonce.into(),
                        program_id,
                        message.source(),
                        payload.into(),
                        0,
                        message_id,
                        0,
                    );
                    self.send_dispatch(message_id, Dispatch::new_reply(reply_message));
                }
            }
            Err(expl) => {
                mock.debug(expl);

                if let DispatchKind::Init = kind {
                    self.message_dispatched(DispatchOutcome::InitFailure {
                        message_id,
                        program_id,
                        origin: message.source(),
                        reason: expl,
                    });
                } else {
                    self.message_dispatched(DispatchOutcome::MessageTrap {
                        message_id,
                        program_id,
                        trap: Some(expl),
                    })
                }

                let nonce = self.fetch_inc_message_nonce();

                let reply_message = Message::new_reply(
                    nonce.into(),
                    program_id,
                    message.source(),
                    Default::default(),
                    0,
                    message_id,
                    1,
                );
                self.send_dispatch(message_id, Dispatch::new_reply(reply_message));
            }
        }

        // After run either `init_success` is called or `init_failed`.
        // So only active (init success) program can be modified
        self.actors.entry(program_id).and_modify(|(actor, _)| {
            if let Actor::Initialized(old_mock) = actor {
                *old_mock = Program::Mock(Some(mock));
            }
        });
    }

    fn process_normal(&mut self, executable_actor: ExecutableActor, dispatch: Dispatch) {
        self.process_dispatch(Some(executable_actor), dispatch);
    }

    fn process_dormant(&mut self, dispatch: Dispatch) {
        self.process_dispatch(None, dispatch);
    }

    fn process_dispatch(&mut self, executable_actor: Option<ExecutableActor>, dispatch: Dispatch) {
        let journal = core_processor::process::<Ext, WasmtimeEnvironment<Ext>>(
            executable_actor,
            dispatch,
            self.block_info,
            crate::EXISTENTIAL_DEPOSIT,
            self.origin,
        );

        core_processor::handle_journal(journal, self);
    }
}

impl JournalHandler for ExtManager {
    fn message_dispatched(&mut self, outcome: DispatchOutcome) {
        match outcome {
            DispatchOutcome::MessageTrap { message_id, .. } => self.mark_failed(message_id),
            DispatchOutcome::Success(_) | DispatchOutcome::NoExecution(_) => {}
            DispatchOutcome::InitFailure {
                message_id,
                program_id,
                ..
            } => self.init_failure(message_id, program_id),
            DispatchOutcome::InitSuccess {
                message_id,
                program_id,
                ..
            } => self.init_success(message_id, program_id),
        }
    }

    fn gas_burned(&mut self, _message_id: MessageId, _amount: u64) {}

    fn exit_dispatch(&mut self, id_exited: ProgramId, _value_destination: ProgramId) {
        self.actors.remove(&id_exited);
    }

    fn message_consumed(&mut self, message_id: MessageId) {
        if let Some(index) = self
            .dispatch_queue
            .iter()
            .position(|d| d.message.id() == message_id)
        {
            self.dispatch_queue.remove(index);
        }
    }

    fn send_dispatch(&mut self, _message_id: MessageId, mut dispatch: Dispatch) {
        let Dispatch {
            ref mut message, ..
        } = dispatch;
        if self.actors.contains_key(&message.dest()) {
            // imbuing gas-less messages with maximum gas!
            if message.gas_limit.is_none() {
                message.gas_limit = Some(u64::max_value());
            }
            self.dispatch_queue.push_back(dispatch);
        } else {
            self.mailbox
                .entry(message.dest())
                .or_default()
                .push(message.clone());
            self.log.push(dispatch.message);
        }
    }

    fn wait_dispatch(&mut self, dispatch: Dispatch) {
        self.message_consumed(dispatch.message.id());
        self.wait_list
            .insert((dispatch.message.dest(), dispatch.message.id()), dispatch);
    }

    fn wake_message(
        &mut self,
        _message_id: MessageId,
        program_id: ProgramId,
        awakening_id: MessageId,
    ) {
        if let Some(dispatch) = self.wait_list.remove(&(program_id, awakening_id)) {
            self.dispatch_queue.push_back(dispatch);
        }
    }

    fn update_nonce(&mut self, program_id: ProgramId, nonce: u64) {
        let (actor, _) = self
            .actors
            .get_mut(&program_id)
            .expect("Can't find existing program");
        if let Some(prog) = actor.as_mut_core_prog() {
            prog.set_message_nonce(nonce);
        }
    }

    fn update_page(
        &mut self,
        program_id: ProgramId,
        page_number: PageNumber,
        data: Option<Vec<u8>>,
    ) {
        let (actor, _) = self
            .actors
            .get_mut(&program_id)
            .expect("Can't find existing program");
        if let Some(prog) = actor.as_mut_core_prog() {
            if let Some(data) = data {
                let _ = prog.set_page(page_number, &data);
            } else {
                prog.remove_page(page_number);
            }
        } else {
            unreachable!("No pages update for non-initialized program")
        }
    }

    fn send_value(&mut self, from: ProgramId, to: Option<ProgramId>, value: Balance) {
        if let Some(to) = to {
            if let Some((_, balance)) = self.actors.get_mut(&from) {
                if *balance < value {
                    panic!("Actor {:?} balance is less then sent value", from);
                }

                *balance -= value;
            };

            if let Some((_, balance)) = self.actors.get_mut(&to) {
                *balance += value;
            };
        }
    }

    fn store_new_programs(
        &mut self,
        _code_hash: CodeHash,
        _candidates: Vec<(ProgramId, MessageId)>,
    ) {
        // todo!() #714
    }
}

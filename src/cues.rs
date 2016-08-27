//! Cues (lists of commands).

use state::{Context, Message};
use backend::BackendSender;
use std::collections::HashMap;
use mio::EventLoop;
use uuid::Uuid;
use std::fmt;

#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub enum ChainType {
    Unattached,
    Q(String)
}
impl fmt::Display for ChainType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &ChainType::Unattached => write!(f, "X"),
            &ChainType::Q(ref st) => write!(f, "Q{}.", st),
        }
    }
}
#[derive(Clone)]
pub struct Chain {
    fsm: QFSM,
    pub commands: Vec<Uuid>,
    pub fallthru: HashMap<Uuid, bool>
}

/// Fixed state machine for a cue runner.
#[derive(Clone)]
pub enum QFSM {
    /// Nothing happening
    Idle,
    /// Next in line for execution (all cues loaded)
    Standby,
    /// Executing, blocked on tuple.0
    Blocked(Uuid)
}

impl Chain {
    pub fn new() -> Self {
        Chain {
            fsm: QFSM::Idle,
            commands: Vec::new(),
            fallthru: HashMap::new()
        }
    }
    pub fn push(&mut self, cmd: Uuid) {
        self.commands.push(cmd);
        self.fallthru.insert(cmd, false);
    }
    pub fn remove(&mut self, cmd: Uuid) -> bool {
        let mut idx = None;
        for (i, uu) in self.commands.iter().enumerate() {
            if cmd == *uu {
                idx = Some(i);
                break;
            }
        }
        if let Some(i) = idx {
            self.commands.remove(i);
            self.fallthru.remove(&cmd);
            true
        }
        else { false }
    }
    pub fn set_fallthru(&mut self, cmd: Uuid, ft: bool) -> bool {
        if self.fallthru.get(&cmd).is_some() {
            self.fallthru.insert(cmd, ft);
            true
        }
        else {
            false
        }
    }
    pub fn is_blocked_on(&mut self, uu: Uuid) -> bool {
        if let QFSM::Blocked(u2) = self.fsm {
            u2 == uu
        }
        else {
            false
        }
    }
    pub fn on_exec_completed(&mut self, completed: Uuid, ctx: &mut Context, evl: &mut EventLoop<Context>) -> bool {
        if let QFSM::Blocked(uu) = self.fsm.clone() {
            if uu == completed {
                let mut next = None;
                for (i, cmd) in self.commands.iter().enumerate() {
                    if *cmd == uu {
                        if let Some(uu) = self.commands.get(i+1) {
                            next = Some(*uu);
                        }
                        break;
                    }
                }
                if let Some(uu) = next {
                    self.exec(uu, ctx, evl);
                }
                else {
                    self.fsm = QFSM::Idle;
                }
                return true;
            }
        }
        false
    }
    fn exec(&mut self, now: Uuid, ctx: &mut Context, evl: &mut EventLoop<Context>) {
        self.fsm = QFSM::Blocked(now);
        if ctx.exec_cmd(now, evl) || self.fallthru.get(&now).map(|x| *x).unwrap_or(false) {
            self.on_exec_completed(now, ctx, evl);
        }
    }
    pub fn go(&mut self, ctx: &mut Context, evl: &mut EventLoop<Context>) {
        if let Some(uu) = self.commands.get(0).map(|x| *x) {
            self.exec(uu, ctx, evl);
        }
    }
    pub fn standby(&mut self, ctx: &mut Context, evl: &mut EventLoop<Context>) {
        if let QFSM::Idle = self.fsm {
            for cmd in self.commands.iter() {
                ctx.load_cmd(*cmd, evl);
            }
            self.fsm = QFSM::Standby;
        }
    }
    pub fn unstandby(&mut self, ctx: &mut Context, evl: &mut EventLoop<Context>) {
        if let QFSM::Standby = self.fsm {
            for cmd in self.commands.iter() {
                ctx.unload_cmd(*cmd, evl);
            }
            self.fsm = QFSM::Idle;
        }
    }
}

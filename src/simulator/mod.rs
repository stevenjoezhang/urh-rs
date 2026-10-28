//! Simulator rule engine — port of `urh/simulator` (the non-GUI core).
//!
//! Modules:
//! - [`item`]: the simulator item tree (arena + `NodeKind` enum) replacing
//!   Python's `SimulatorItem` class hierarchy
//! - [`expression`]: formula/condition parser + evaluator
//!   (`SimulatorExpressionParser.py`)
//! - [`config`]: configuration model + XML save/load (`SimulatorConfiguration.py`)
//! - [`engine`]: the simulation run loop (`Simulator.py`)
//! - [`transcript`]: the message transcript (`Transcript.py`)
//!
//! Not ported (Qt scene/view layer): `GraphicsItem.py`, `LabelItem.py`,
//! `MessageItem.py`, `ParticipantItem.py`, `RuleItem.py`, `ActionItem.py`,
//! `UnlabeledRangeItem.py`.

pub mod config;
pub mod engine;
pub mod expression;
pub mod item;
pub mod transcript;

pub use config::{SimModulator, SimParticipant, SimulatorConfiguration};
pub use engine::{SimError, Simulator, SimulatorIO, SimulatorSettings};
pub use expression::{
    BinOp, CmpOp, Expr, ExprError, ExpressionParser, IdentifierResolver, Value,
};
pub use item::{
    ChecksumSpec, ConditionType, NodeId, NodeKind, SimLabel, SimMessage, SimNode,
};
pub use transcript::Transcript;

//! Client wrapper `AuditLogger` for callers (staking, rebalancing).
//!
//! Other contracts construct `AuditLogger::new(env, &sink_address)` and call
//! `.log_event(...)`. `AuditLogger` translates the call into a contract
//! invocation so callers don't have to write boilerplate each time.
//!
//! This module is a *client* helper — it never runs in the audit contract
//! itself. The companion `#[contractimpl]` block in [`crate::lib`] is what
//! receives the `invoke_contract` call.

extern crate alloc;

use soroban_sdk::{symbol_short, Address, Env, IntoVal, String, Symbol, Vec};

use crate::records::{AuditEventType, AuditLog, StateSnapshot};

/// Client for the audit-log contract.
pub struct AuditLogger<'a> {
    env: &'a Env,
    contract: Address,
}

impl<'a> AuditLogger<'a> {
    /// Construct a logger targeting `contract`. The address should be the
    /// deployed `AuditContract` — callers obtain it via `set_audit_sink`
    /// admin endpoints on each consumer contract.
    pub fn new(env: &'a Env, contract: &Address) -> Self {
        Self {
            env,
            contract: contract.clone(),
        }
    }

    /// Append one audit entry. Returns the sequence id assigned by the
    /// audit contract.
    ///
    /// `actor` must already have authenticated for the calling contract —
    /// `require_auth()` on the auditor's side will simply re-evaluate the
    /// same auth context.
    #[allow(clippy::too_many_arguments)]
    pub fn log_event(
        &self,
        actor: Address,
        event_type: AuditEventType,
        portfolio: Symbol,
        permissions: u32,
        state_before: StateSnapshot,
        state_after: StateSnapshot,
        outcome: Symbol,
        detail: String,
    ) -> u64 {
        let fn_name = symbol_short!("log_event");
        let args: Vec<soroban_sdk::Val> = (
            actor,
            event_type,
            portfolio,
            permissions,
            state_before,
            state_after,
            outcome,
            detail,
        )
            .into_val(self.env);
        // invoke_contract returns T directly; panics on failure.
        self.env
            .invoke_contract::<u64>(&self.contract, &fn_name, args)
    }

    /// Query the audit log via the contract. Used for cross-contract
    /// verification requests (e.g. compliance reconciliation).
    pub fn query(&self) -> Vec<AuditLog> {
        let fn_name = symbol_short!("query");
        let empty_args: Vec<soroban_sdk::Val> = Vec::new(self.env);
        self.env
            .invoke_contract::<Vec<AuditLog>>(&self.contract, &fn_name, empty_args)
    }
}

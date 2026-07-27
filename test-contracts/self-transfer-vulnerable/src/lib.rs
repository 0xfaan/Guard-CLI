#![no_std]
use soroban_sdk::{contract, contractimpl, Address, Env};

#[contract]
pub struct SelfTransferVulnerable;

#[contractimpl]
impl SelfTransferVulnerable {
    /// Vulnerable: named like a token transfer but does not guard against `from == to`.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        let _ = (env, to, amount);
    }
}

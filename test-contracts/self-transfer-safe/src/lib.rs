#![no_std]
use soroban_sdk::{contract, contractimpl, Address, Env};

#[contract]
pub struct SelfTransferSafe;

#[contractimpl]
impl SelfTransferSafe {
    /// Safe: explicitly rejects self-transfers before performing transfer logic.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        assert!(from != to, "self-transfer is not allowed");
        from.require_auth();
        let _ = (env, amount);
    }
}
